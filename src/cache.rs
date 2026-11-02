//! Per-source, time-windowed HLS media segment cache.
//!
//! Only complete segments are published. Prefetch runs independently of decoded
//! frame/packet backpressure, and its downloads are cancellable on seek/drop.
use super::*;
use std::io::Cursor;

#[derive(Clone, Debug)]
pub struct HlsCacheConfig {
    /// Retain already downloaded segments intersecting this much prior media.
    pub backward: Duration,
    /// Prefetch complete segments intersecting this much future media.
    pub forward: Duration,
}

impl Default for HlsCacheConfig {
    fn default() -> Self {
        Self {
            backward: Duration::from_secs(30),
            forward: Duration::from_secs(45),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SegmentKey {
    url: Url,
    range: Option<ByteWindowSpec>,
}

impl From<&SegmentEntry> for SegmentKey {
    fn from(entry: &SegmentEntry) -> Self {
        Self {
            url: entry.url.clone(),
            range: entry.range,
        }
    }
}

struct State {
    segments: Vec<SegmentEntry>,
    center: usize,
    config: HlsCacheConfig,
    entries: HashMap<SegmentKey, Arc<[u8]>>,
    failures: HashMap<SegmentKey, Instant>,
    token: CancellationToken,
    stopped: bool,
    hits: u64,
    misses: u64,
}

impl State {
    fn limits(&self) -> (f64, f64) {
        let center = self.segments[self.center].start_seconds;
        (
            center - self.config.backward.as_secs_f64(),
            center + self.config.forward.as_secs_f64(),
        )
    }

    fn retained(&self, entry: &SegmentEntry) -> bool {
        let (start, end) = self.limits();
        entry.start_seconds < end && entry.start_seconds + entry.duration_seconds > start
    }

    fn prune(&mut self) {
        let permitted: HashSet<_> = self
            .segments
            .iter()
            .filter(|entry| self.retained(entry))
            .map(SegmentKey::from)
            .collect();
        self.entries.retain(|key, _| permitted.contains(key));
        self.failures.retain(|key, _| permitted.contains(key));
    }

    fn next(&self) -> Option<(SegmentKey, SegmentEntry, CancellationToken)> {
        let (_, end) = self.limits();
        self.segments
            .iter()
            .skip(self.center)
            .take_while(|entry| entry.start_seconds < end)
            .find(|entry| {
                let key = SegmentKey::from(*entry);
                !self.entries.contains_key(&key)
                    && self
                        .failures
                        .get(&key)
                        .map_or(true, |at| at.elapsed() >= Duration::from_secs(5))
            })
            .map(|entry| (SegmentKey::from(entry), entry.clone(), self.token.clone()))
    }

    fn summary(&self) -> (usize, usize, f64, f64) {
        let bytes = self.entries.values().map(|value| value.len()).sum();
        // Report contiguous cached media, not an overestimated span
        // across missing segments (for example, after a failed fetch).
        let mut behind = 0.0;
        for index in (0..self.center).rev() {
            let segment = &self.segments[index];
            if !self.entries.contains_key(&SegmentKey::from(segment)) {
                break;
            }
            behind += segment.duration_seconds;
        }
        let mut ahead = 0.0;
        for segment in self.segments.iter().skip(self.center) {
            if !self.entries.contains_key(&SegmentKey::from(segment)) {
                break;
            }
            ahead += segment.duration_seconds;
        }
        (self.entries.len(), bytes, behind, ahead)
    }
}

type Fetcher = Arc<dyn Fn(&SegmentEntry, &CancellationToken) -> Result<Vec<u8>> + Send + Sync>;

#[derive(Clone)]
pub(super) struct SegmentCache {
    state: Arc<Mutex<State>>,
    wake: mpsc::Sender<()>,
}

impl SegmentCache {
    pub(super) fn new(segments: &[SegmentEntry], config: HlsCacheConfig) -> Self {
        Self::with_fetcher(segments, config, Arc::new(fetch_segment))
    }

    fn with_fetcher(segments: &[SegmentEntry], config: HlsCacheConfig, fetch: Fetcher) -> Self {
        assert!(
            !segments.is_empty(),
            "HLS cache requires an indexed playlist"
        );
        let state = Arc::new(Mutex::new(State {
            segments: segments.to_vec(),
            center: 0,
            config,
            entries: HashMap::new(),
            failures: HashMap::new(),
            token: CancellationToken::new(),
            stopped: false,
            hits: 0,
            misses: 0,
        }));
        let (wake, rx) = mpsc::channel();
        let shared = Arc::clone(&state);
        std::thread::Builder::new()
            .name("oxideav-hls-segment-cache".into())
            .spawn(move || worker(shared, rx, fetch))
            .expect("start HLS segment prefetch worker");
        Self { state, wake }
    }

    pub(super) fn get(&self, segment: &SegmentEntry) -> Option<Arc<[u8]>> {
        let mut state = self.state.lock().unwrap();
        let hit = state.entries.get(&SegmentKey::from(segment)).cloned();
        if hit.is_some() {
            state.hits += 1;
        } else {
            state.misses += 1;
        }
        hit
    }

    /// Used by the successor-demux preparation worker. Network prefetch owns
    /// the segment download; this worker waits to demux that same completed
    /// cached resource instead of starting a competing HTTP transfer.
    /// A distant seek or cache shutdown cancels the wait.
    pub(super) fn wait_for_prefetch(&self, segment: &SegmentEntry) -> Result<()> {
        let key = SegmentKey::from(segment);
        let generation = self.state.lock().unwrap().token.clone();
        loop {
            let state = self.state.lock().unwrap();
            if state.stopped || generation.is_cancelled() {
                return Err(Error::cancelled("HLS segment prefetch window cancelled"));
            }
            if state.entries.contains_key(&key) {
                return Ok(());
            }
            if state.failures.contains_key(&key)
                || !state.segments.iter().any(|candidate| {
                    SegmentKey::from(candidate) == key && state.retained(candidate)
                })
            {
                // A failed background download may still succeed through the
                // original streaming path; do not make prefetch failure fatal.
                return Ok(());
            }
            drop(state);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn recenter(&self, current: usize, segments: &[SegmentEntry]) {
        let mut state = self.state.lock().unwrap();
        let changed_far = current.abs_diff(state.center) > 1;
        if changed_far {
            state.token.cancel();
            state.token = CancellationToken::new();
        }
        state.segments = segments.to_vec();
        state.center = current;
        state.prune();
        let (count, bytes, behind, ahead) = state.summary();
        log::info!(
            "oxideav-hls: cache window center={:.3}s segments={count} bytes={bytes} behind={behind:.1}s ahead={ahead:.1}s hits={} misses={}",
            state.segments[current].start_seconds, state.hits, state.misses
        );
        drop(state);
        let _ = self.wake.send(());
    }

    pub(super) fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        state.token.cancel();
        drop(state);
        let _ = self.wake.send(());
    }
}

fn fetch_segment(segment: &SegmentEntry, cancellation: &CancellationToken) -> Result<Vec<u8>> {
    // Regular segments use cancellable whole-body HTTP requests. The safety
    // ceiling is per-resource, not the configurable time-window cache policy.
    const MAX_SINGLE_SEGMENT_BYTES: u64 = 128 * 1024 * 1024;
    if let Some(range) = segment.range {
        if range.length > MAX_SINGLE_SEGMENT_BYTES {
            return Err(Error::unsupported(
                "hls: segment byte range exceeds safety ceiling",
            ));
        }
        // A byte-range resource may be a much larger container; fetch only
        // the requested range via the existing seekable HTTP driver.
        let bytes = oxideav_http::open_http(segment.url.as_str())?;
        let mut input = windowed_reader(bytes, Some(range))?;
        let mut output = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::cancelled("HLS segment prefetch cancelled"));
            }
            let n = input.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..n]);
        }
        Ok(output)
    } else {
        oxideav_http::fetch_bytes_cancellable(
            segment.url.as_str(),
            MAX_SINGLE_SEGMENT_BYTES,
            cancellation,
        )
    }
}

fn worker(shared: Arc<Mutex<State>>, rx: mpsc::Receiver<()>, fetch: Fetcher) {
    loop {
        let task = {
            let mut state = shared.lock().unwrap();
            if state.stopped {
                return;
            }
            state.prune();
            state.next()
        };
        if let Some((key, segment, token)) = task {
            let result = fetch(&segment, &token);
            let mut state = shared.lock().unwrap();
            if state.stopped {
                return;
            }
            // Never publish obsolete downloads from a prior seek window.
            if !token.is_cancelled()
                && state
                    .segments
                    .iter()
                    .any(|e| SegmentKey::from(e) == key && state.retained(e))
            {
                match result {
                    Ok(bytes) if !bytes.is_empty() => {
                        let size = bytes.len();
                        state.entries.insert(key, Arc::from(bytes));
                        log::debug!(
                            "oxideav-hls: cached segment start={:.3}s bytes={size}",
                            segment.start_seconds
                        );
                    }
                    Ok(_) => {
                        state.failures.insert(key, Instant::now());
                        log::warn!("oxideav-hls: empty prefetched segment");
                    }
                    Err(error) => {
                        state.failures.insert(key, Instant::now());
                        log::warn!("oxideav-hls: segment prefetch failed: {error}");
                    }
                }
            }
        } else {
            if rx
                .recv_timeout(Duration::from_secs(1))
                .is_err_and(|e| matches!(e, mpsc::RecvTimeoutError::Disconnected))
            {
                return;
            }
            // Collapse obsolete wake notifications.
            while rx.try_recv().is_ok() {}
        }
    }
}

pub(super) fn cached_segment_reader(
    cache: &SegmentCache,
    segment: &SegmentEntry,
) -> Option<Box<dyn ReadSeek>> {
    cache
        .get(segment)
        .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn ReadSeek>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_segments(count: usize) -> Vec<SegmentEntry> {
        (0..count)
            .map(|i| SegmentEntry {
                url: Url::parse(&format!("https://example.test/{i}.ts")).unwrap(),
                range: None,
                map: None,
                start_seconds: i as f64 * 10.0,
                duration_seconds: 10.0,
            })
            .collect()
    }

    fn wait_for(cache: &SegmentCache, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while cache.state.lock().unwrap().entries.len() < count {
            assert!(Instant::now() < deadline, "cache prefetch timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn prefetches_time_window_and_retains_history_on_advance() {
        let segments = make_segments(15);
        let fetched = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fetched);
        let fetcher: Fetcher = Arc::new(move |entry, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(entry.url.path().as_bytes().to_vec())
        });
        let cache = SegmentCache::with_fetcher(&segments, HlsCacheConfig::default(), fetcher);
        wait_for(&cache, 5);
        assert!(cache.get(&segments[0]).is_some());
        assert!(cache.get(&segments[4]).is_some());
        assert!(cache.get(&segments[5]).is_none());
        cache.recenter(3, &segments);
        wait_for(&cache, 8);
        assert!(cache.get(&segments[0]).is_some()); // 30 s back
        assert!(cache.get(&segments[7]).is_some()); // 45 s ahead
        cache.recenter(5, &segments);
        assert!(cache.get(&segments[0]).is_none());
        assert!(cache.get(&segments[2]).is_some());
        let previous = fetched.load(Ordering::SeqCst);
        assert!(cache.get(&segments[3]).is_some());
        assert_eq!(previous, fetched.load(Ordering::SeqCst)); // hit never refetches
        cache.stop();
    }

    #[test]
    fn stale_download_discarded_when_seek_changes_window() {
        let segments = make_segments(20);
        let fetcher: Fetcher = Arc::new(|entry, token| {
            if entry.start_seconds == 0.0 {
                while !token.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                return Err(Error::cancelled("obsolete segment"));
            }
            Ok(vec![1u8])
        });
        let cache = SegmentCache::with_fetcher(&segments, HlsCacheConfig::default(), fetcher);
        cache.recenter(10, &segments);
        wait_for(&cache, 5);
        assert!(cache.get(&segments[0]).is_none());
        assert!(cache.get(&segments[10]).is_some());
        cache.stop();
    }
}
