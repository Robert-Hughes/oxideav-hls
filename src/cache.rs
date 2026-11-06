//! Per-source HLS cache: foreground and prefetch reads share the same byte stream.
use super::*;
use std::io::{self, SeekFrom};
use std::sync::Condvar;

const MAX_SEGMENT_BYTES: usize = 128 * 1024 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct HlsCacheConfig {
    pub backward: Duration,
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
    fn from(s: &SegmentEntry) -> Self {
        Self {
            url: s.url.clone(),
            range: s.range,
        }
    }
}
#[derive(Default)]
struct Body {
    bytes: Vec<u8>,
    finished: bool,
    error: Option<String>,
    finished_at: Option<Instant>,
}
struct Download {
    body: Mutex<Body>,
    ready: Condvar,
    cancellation: CancellationToken,
}
impl Download {
    fn new() -> Self {
        Self {
            body: Mutex::new(Body::default()),
            ready: Condvar::new(),
            cancellation: CancellationToken::new(),
        }
    }
    fn finish(&self, outcome: Result<()>) {
        let mut body = self.body.lock().unwrap();
        body.error = outcome.err().map(|error| error.to_string());
        body.finished = true;
        body.finished_at = Some(Instant::now());
        self.ready.notify_all();
    }
    fn cancel(&self) {
        self.cancellation.cancel();
        self.ready.notify_all();
    }
    fn completed(&self) -> bool {
        let b = self.body.lock().unwrap();
        b.finished && b.error.is_none()
    }
    fn retryable(&self) -> bool {
        let b = self.body.lock().unwrap();
        b.finished
            && b.error.is_some()
            && b.finished_at.is_some_and(|at| at.elapsed() >= RETRY_DELAY)
    }
}
struct CachedReader {
    download: Arc<Download>,
    pos: u64,
}
impl Read for CachedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut b = self.download.body.lock().unwrap();
        loop {
            if self.pos < b.bytes.len() as u64 {
                let n = output.len().min((b.bytes.len() as u64 - self.pos) as usize);
                output[..n].copy_from_slice(&b.bytes[self.pos as usize..self.pos as usize + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if self.download.cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "HLS segment cancelled",
                ));
            }
            if b.finished {
                if let Some(error) = &b.error {
                    return Err(io::Error::other(error.clone()));
                }
                return Ok(0);
            }
            b = self.download.ready.wait(b).unwrap();
        }
    }
}
impl Seek for CachedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => {
                let mut b = self.download.body.lock().unwrap();
                while !b.finished && !self.download.cancellation.is_cancelled() {
                    b = self.download.ready.wait(b).unwrap();
                }
                if self.download.cancellation.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "HLS segment cancelled",
                    ));
                }
                if let Some(error) = &b.error {
                    return Err(io::Error::other(error.clone()));
                }
                b.bytes.len() as i128 + n as i128
            }
        };
        if target < 0 || target > u64::MAX as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HLS segment seek",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}
type Fetcher = Arc<
    dyn Fn(&SegmentEntry, &CancellationToken, &dyn Fn(&[u8]) -> Result<()>) -> Result<()>
        + Send
        + Sync,
>;

struct State {
    segments: Vec<SegmentEntry>,
    center: usize,
    config: HlsCacheConfig,
    entries: HashMap<SegmentKey, Arc<Download>>,
    stopped: bool,
    hits: u64,
    misses: u64,
    completed_hits: u64,
    inflight_joins: u64,
    demand_starts: u64,
    prefetch_starts: u64,
    downloads_completed: u64,
    downloads_failed: u64,
    downloads_cancelled: u64,
    downloads_bytes: u64,
    download_millis: u64,
    evicted: u64,
    last_report: Instant,
}
impl State {
    fn limits(&self) -> (f64, f64) {
        let center = self.segments[self.center].start_seconds;
        (
            center - self.config.backward.as_secs_f64(),
            center + self.config.forward.as_secs_f64(),
        )
    }
    fn retained(&self, e: &SegmentEntry) -> bool {
        let (start, end) = self.limits();
        e.start_seconds < end && e.start_seconds + e.duration_seconds > start
    }
    fn prune(&mut self) {
        let keep: HashSet<_> = self
            .segments
            .iter()
            .filter(|e| self.retained(e))
            .map(SegmentKey::from)
            .collect();
        let mut removed = 0u64;
        let mut interrupted = 0u64;
        self.entries.retain(|key, download| {
            if keep.contains(key) {
                true
            } else {
                removed += 1;
                if !download.body.lock().unwrap().finished {
                    interrupted += 1;
                }
                download.cancel();
                log::debug!("oxideav-hls: cache evicted segment entry");
                false
            }
        });
        self.evicted += removed;
        self.downloads_cancelled += interrupted;
    }
    /// One priority rule: current, nearest future first, nearest history first.
    fn next(&mut self) -> Option<(SegmentEntry, Arc<Download>)> {
        let (_, end) = self.limits();
        let forward = self
            .segments
            .iter()
            .skip(self.center)
            .take_while(|e| e.start_seconds < end);
        let backward = self.segments[..self.center]
            .iter()
            .rev()
            .take_while(|e| self.retained(e));
        for entry in forward.chain(backward) {
            let key = SegmentKey::from(entry);
            if self
                .entries
                .get(&key)
                .is_some_and(|download| !download.retryable())
            {
                continue;
            }
            let entry = entry.clone();
            let download = Arc::new(Download::new());
            self.entries.insert(key, Arc::clone(&download));
            self.prefetch_starts += 1;
            return Some((entry, download));
        }
        None
    }
    /// Routine production statistics, emitted on a window change and at most
    /// once every 30 seconds while the download worker is active or idle.
    fn report(&mut self, force: bool) {
        if !force && self.last_report.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_report = Instant::now();
        let (entries, bytes, behind, ahead) = self.summary();
        let complete = self
            .entries
            .values()
            .filter(|entry| entry.completed())
            .count();
        let downloading = self
            .entries
            .values()
            .filter(|entry| !entry.body.lock().unwrap().finished)
            .count();
        let mean_ms = self
            .download_millis
            .checked_div(self.downloads_completed)
            .unwrap_or(0);
        log::info!(
            "oxideav-hls: cache stats center={:.1}s entries={entries} complete={complete} downloading={downloading} bytes={bytes} behind={behind:.1}s ahead={ahead:.1}s hits={} completed_hits={} inflight_joins={} misses={} demand={} prefetch={} fetched={} downloaded_bytes={} avg_download_ms={mean_ms} failed={} cancelled={} evicted={}",
            self.segments[self.center].start_seconds,
            self.hits, self.completed_hits, self.inflight_joins, self.misses,
            self.demand_starts, self.prefetch_starts, self.downloads_completed,
            self.downloads_bytes, self.downloads_failed, self.downloads_cancelled,
            self.evicted,
        );
    }

    fn summary(&self) -> (usize, usize, f64, f64) {
        let bytes = self
            .entries
            .values()
            .map(|e| e.body.lock().unwrap().bytes.len())
            .sum();
        let mut behind = 0.0;
        for e in self.segments[..self.center].iter().rev() {
            if !self
                .entries
                .get(&SegmentKey::from(e))
                .is_some_and(|v| v.completed())
            {
                break;
            }
            behind += e.duration_seconds;
        }
        let mut ahead = 0.0;
        for e in self.segments.iter().skip(self.center) {
            if !self
                .entries
                .get(&SegmentKey::from(e))
                .is_some_and(|v| v.completed())
            {
                break;
            }
            ahead += e.duration_seconds;
        }
        (self.entries.len(), bytes, behind, ahead)
    }
}
#[derive(Clone)]
pub(super) struct SegmentCache {
    state: Arc<Mutex<State>>,
    fetch: Fetcher,
    wake: mpsc::Sender<()>,
}
impl SegmentCache {
    pub(super) fn new(segments: &[SegmentEntry], config: HlsCacheConfig) -> Self {
        Self::with_fetcher(segments, config, Arc::new(fetch_segment))
    }
    fn with_fetcher(segments: &[SegmentEntry], config: HlsCacheConfig, fetch: Fetcher) -> Self {
        assert!(!segments.is_empty());
        let state = Arc::new(Mutex::new(State {
            segments: segments.to_vec(),
            center: 0,
            config,
            entries: HashMap::new(),
            stopped: false,
            hits: 0,
            misses: 0,
            completed_hits: 0,
            inflight_joins: 0,
            demand_starts: 0,
            prefetch_starts: 0,
            downloads_completed: 0,
            downloads_failed: 0,
            downloads_cancelled: 0,
            downloads_bytes: 0,
            download_millis: 0,
            evicted: 0,
            last_report: Instant::now(),
        }));
        let (wake, rx) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker_fetch = Arc::clone(&fetch);
        thread::Builder::new()
            .name("oxideav-hls-prefetch".into())
            .spawn(move || worker(worker_state, rx, worker_fetch))
            .expect("start HLS prefetch worker");
        Self { state, fetch, wake }
    }
    /// A cache miss immediately starts a foreground transfer. Background and
    /// foreground readers of the same entry share one ongoing HTTP transfer.
    pub(super) fn reader(&self, segment: &SegmentEntry) -> Box<dyn ReadSeek> {
        let key = SegmentKey::from(segment);
        let (download, start) = {
            let mut state = self.state.lock().unwrap();
            if let Some(existing) = state
                .entries
                .get(&key)
                .filter(|d| {
                    let b = d.body.lock().unwrap();
                    !b.finished || b.error.is_none()
                })
                .cloned()
            {
                state.hits += 1;
                if existing.completed() {
                    state.completed_hits += 1;
                } else {
                    state.inflight_joins += 1;
                }
                (existing, false)
            } else {
                state.misses += 1;
                state.demand_starts += 1;
                let d = Arc::new(Download::new());
                state.entries.insert(key, Arc::clone(&d));
                (d, true)
            }
        };
        if start {
            let entry = segment.clone();
            let d = Arc::clone(&download);
            let fetch = Arc::clone(&self.fetch);
            let state = Arc::clone(&self.state);
            if let Err(err) = thread::Builder::new()
                .name("oxideav-hls-demand".into())
                .spawn(move || perform_fetch(&entry, &d, &fetch, &state, "demand"))
            {
                download.finish(Err(Error::other(format!("start HLS download: {err}"))));
            }
        }
        Box::new(CachedReader { download, pos: 0 })
    }
    #[cfg(test)]
    pub(super) fn completed(&self, segment: &SegmentEntry) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(&SegmentKey::from(segment))
            .is_some_and(|entry| entry.completed())
    }
    pub(super) fn recenter(&self, current: usize, segments: &[SegmentEntry]) {
        let mut state = self.state.lock().unwrap();
        state.segments = segments.to_vec();
        state.center = current;
        state.prune();
        state.report(true);
        drop(state);
        let _ = self.wake.send(());
    }
    pub(super) fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return;
        }
        state.stopped = true;
        let mut interrupted = 0;
        for entry in state.entries.values() {
            if !entry.body.lock().unwrap().finished {
                interrupted += 1;
            }
            entry.cancel();
        }
        state.downloads_cancelled += interrupted;
        state.report(true);
        drop(state);
        let _ = self.wake.send(());
    }
}
fn fetch_segment(
    segment: &SegmentEntry,
    cancel: &CancellationToken,
    emit: &dyn Fn(&[u8]) -> Result<()>,
) -> Result<()> {
    // Segment bytes enter the cache regardless of who requested them.
    // The underlying HTTP future is dropped promptly on cancellation even
    // when headers or the next network chunk are stalled.
    oxideav_http::fetch_stream_cancellable(
        segment.url.as_str(),
        MAX_SEGMENT_BYTES as u64,
        segment.range.map(|range| (range.offset, range.length)),
        cancel,
        emit,
    )
}
/// Re-fetch complete segments and compare their already-published prefix before
/// appending new data. Unlike an unchecked HTTP Range resume, replay verification
/// protects against a CDN changing bytes when no strong validator is provided.
/// Bounded retries preserve readers of partially downloaded cache entries.
fn fetch_with_retries(
    segment: &SegmentEntry,
    download: &Arc<Download>,
    fetch: &Fetcher,
) -> Result<()> {
    const BACKOFFS: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(300)];
    for attempt in 0..=BACKOFFS.len() {
        if download.cancellation.is_cancelled() {
            return Err(Error::cancelled("HLS segment download cancelled"));
        }
        let replay_offset = Mutex::new(0usize);
        let changed = std::sync::atomic::AtomicBool::new(false);
        let result = fetch(segment, &download.cancellation, &|chunk| {
            if download.cancellation.is_cancelled() {
                return Err(Error::cancelled("HLS segment download cancelled"));
            }
            let mut offset = replay_offset.lock().unwrap();
            let mut body = download.body.lock().unwrap();
            let overlap = chunk.len().min(body.bytes.len().saturating_sub(*offset));
            if body.bytes[*offset..*offset + overlap] != chunk[..overlap] {
                changed.store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(Error::invalid(
                    "HLS segment changed across retries: prefix mismatch",
                ));
            }
            let fresh = &chunk[overlap..];
            if !fresh.is_empty() {
                if body.bytes.len().saturating_add(fresh.len()) > MAX_SEGMENT_BYTES {
                    return Err(Error::unsupported(
                        "HLS segment exceeds 128 MiB safety ceiling",
                    ));
                }
                body.bytes.extend_from_slice(fresh);
                download.ready.notify_all();
            }
            *offset += chunk.len();
            Ok(())
        });
        if download.cancellation.is_cancelled() {
            return Err(Error::cancelled("HLS segment download cancelled"));
        }
        // A successful but shorter replay would abandon bytes already delivered
        // to consumers, and must therefore be treated as a changed segment.
        if result.is_ok()
            && *replay_offset.lock().unwrap() < download.body.lock().unwrap().bytes.len()
        {
            return Err(Error::invalid(
                "HLS segment changed across retries: shorter replay",
            ));
        }
        match result {
            Ok(()) => {
                if attempt > 0 {
                    log::info!("oxideav-hls: segment recovered after {attempt} retries media={:.3}s bytes={}",
                        segment.start_seconds, download.body.lock().unwrap().bytes.len());
                }
                return Ok(());
            }
            Err(error) if error.is_cancelled() => return Err(error),
            Err(error) if changed.load(std::sync::atomic::Ordering::Relaxed) => return Err(error),
            Err(error) if attempt == BACKOFFS.len() => {
                return Err(Error::other(format!(
                    "HLS segment failed after {} attempts media={:.3}s cached_bytes={}: {error}",
                    attempt + 1,
                    segment.start_seconds,
                    download.body.lock().unwrap().bytes.len()
                )));
            }
            Err(error) => {
                log::warn!(
                    "oxideav-hls: segment retry {}/{} media={:.3}s cached_bytes={} cause={error}",
                    attempt + 1,
                    BACKOFFS.len(),
                    segment.start_seconds,
                    download.body.lock().unwrap().bytes.len()
                );
            }
        }
        // Short, cancellable backoff: a seek must not wait for retries on an
        // obsolete segment.
        let until = Instant::now() + BACKOFFS[attempt];
        loop {
            if download.cancellation.is_cancelled() {
                return Err(Error::cancelled("HLS segment download cancelled"));
            }
            let Some(remaining) = until.checked_duration_since(Instant::now()) else {
                break;
            };
            thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    unreachable!()
}
fn perform_fetch(
    segment: &SegmentEntry,
    download: &Arc<Download>,
    fetch: &Fetcher,
    state: &Arc<Mutex<State>>,
    source: &str,
) {
    let started = Instant::now();
    log::debug!(
        "oxideav-hls: cache download started source={source} media={:.3}s",
        segment.start_seconds
    );
    let result = fetch_with_retries(segment, download, fetch);
    let cancelled = download.cancellation.is_cancelled()
        || result
            .as_ref()
            .err()
            .is_some_and(|error| error.is_cancelled());
    let size = download.body.lock().unwrap().bytes.len();
    let elapsed = started.elapsed();
    if cancelled {
        log::debug!("oxideav-hls: cache download cancelled source={source} media={:.3}s after={elapsed:?} bytes={size}", segment.start_seconds);
    } else if let Err(ref error) = result {
        log::warn!("oxideav-hls: cache download failed source={source} media={:.3}s after={elapsed:?} bytes={size}: {error}", segment.start_seconds);
    } else {
        log::debug!("oxideav-hls: cache download complete source={source} media={:.3}s after={elapsed:?} bytes={size}", segment.start_seconds);
    }
    let success = !cancelled && result.is_ok();
    download.finish(result);
    let mut stats = state.lock().unwrap();
    // Only account once for the current resident entry. An evicted, cancelled
    // transfer has already been counted when its window was discarded.
    let current = stats
        .entries
        .get(&SegmentKey::from(segment))
        .is_some_and(|entry| Arc::ptr_eq(entry, download));
    if current {
        if success {
            stats.downloads_completed += 1;
            stats.downloads_bytes += size as u64;
            stats.download_millis += elapsed.as_millis() as u64;
        } else if !cancelled {
            stats.downloads_failed += 1;
        }
    }
    stats.report(false);
}
fn worker(shared: Arc<Mutex<State>>, wake: mpsc::Receiver<()>, fetch: Fetcher) {
    loop {
        let task = {
            let mut state = shared.lock().unwrap();
            if state.stopped {
                return;
            }
            state.prune();
            state.next()
        };
        if let Some((segment, download)) = task {
            perform_fetch(&segment, &download, &fetch, &shared, "prefetch");
        } else {
            shared.lock().unwrap().report(false);
            if wake
                .recv_timeout(Duration::from_millis(250))
                .is_err_and(|err| matches!(err, mpsc::RecvTimeoutError::Disconnected))
            {
                return;
            }
            while wake.try_recv().is_ok() {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn segments(n: usize) -> Vec<SegmentEntry> {
        (0..n)
            .map(|i| SegmentEntry {
                url: Url::parse(&format!("https://example.test/{i}.ts")).unwrap(),
                range: None,
                map: None,
                start_seconds: i as f64 * 10.0,
                duration_seconds: 10.0,
            })
            .collect()
    }
    fn wait(cache: &SegmentCache, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while cache
            .state
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|d| d.completed())
            .count()
            < count
        {
            assert!(Instant::now() < deadline, "prefetch never finished");
            thread::sleep(Duration::from_millis(3));
        }
    }
    #[test]
    fn forward_then_backward_priority_and_eviction() {
        let all = segments(15);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let fetch: Fetcher = Arc::new(move |entry, _, emit| {
            log.lock().unwrap().push(entry.start_seconds as usize / 10);
            emit(entry.url.path().as_bytes())
        });
        let cache = SegmentCache::with_fetcher(&all, HlsCacheConfig::default(), fetch);
        wait(&cache, 5);
        cache.recenter(5, &all);
        wait(&cache, 8);
        let order = seen.lock().unwrap().clone();
        assert_eq!(&order[0..5], &[0, 1, 2, 3, 4]);
        assert_eq!(&order[5..10], &[5, 6, 7, 8, 9]);
        assert!(cache.completed(&all[2]));
        assert!(!cache.completed(&all[1]));
        let mut reader = cache.reader(&all[2]);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"/2.ts");
        cache.stop();
    }
    #[test]
    fn distant_seek_downloads_missing_history_after_forward_window() {
        let all = segments(20);
        let order = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&order);
        let fetch: Fetcher = Arc::new(move |entry, _, emit| {
            observed
                .lock()
                .unwrap()
                .push((entry.start_seconds / 10.0) as usize);
            emit(b"segment")
        });
        let cache = SegmentCache::with_fetcher(&all, HlsCacheConfig::default(), fetch);
        cache.recenter(12, &all);
        wait(&cache, 8);
        for (index, entry) in all.iter().enumerate().take(17).skip(12) {
            assert!(cache.completed(entry), "missing forward segment {index}");
        }
        for (index, entry) in all.iter().enumerate().take(12).skip(9) {
            assert!(cache.completed(entry), "missing historical segment {index}");
        }
        assert!(!cache.completed(&all[8]));
        let selected: Vec<_> = order
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|index| *index >= 9)
            .collect();
        assert_eq!(&selected[..8], &[12, 13, 14, 15, 16, 11, 10, 9]);
        cache.stop();
    }
    #[test]
    fn streaming_readers_share_partial_bytes_without_redownload() {
        let all = segments(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&calls);
        let b = Arc::clone(&released);
        let fetch: Fetcher = Arc::new(move |_, token, emit| {
            a.fetch_add(1, Ordering::SeqCst);
            emit(b"first")?;
            while !b.load(Ordering::SeqCst) {
                if token.is_cancelled() {
                    return Err(Error::cancelled("test cancelled"));
                }
                thread::sleep(Duration::from_millis(2));
            }
            emit(b"second")
        });
        let cache = SegmentCache::with_fetcher(&all, HlsCacheConfig::default(), fetch);
        let mut a = cache.reader(&all[0]);
        let mut b = cache.reader(&all[0]);
        let mut prefix = [0u8; 5];
        a.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"first");
        b.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"first");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        released.store(true, Ordering::SeqCst);
        wait(&cache, 1);
        let mut suffix = [0u8; 6];
        a.read_exact(&mut suffix).unwrap();
        assert_eq!(&suffix, b"second");
        b.seek(SeekFrom::Start(0)).unwrap();
        let mut all_data = Vec::new();
        b.read_to_end(&mut all_data).unwrap();
        assert_eq!(all_data, b"firstsecond");
        cache.stop();
    }
    #[test]
    fn seeking_far_cancels_old_download_and_starts_new_demand() {
        let all = segments(15);
        let active = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&active);
        let fetch: Fetcher = Arc::new(move |entry, token, emit| {
            if entry.start_seconds == 0.0 {
                started.store(true, Ordering::SeqCst);
                while !token.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                return Err(Error::cancelled("old transfer cancelled"));
            }
            emit(b"ok")
        });
        let cache = SegmentCache::with_fetcher(&all, HlsCacheConfig::default(), fetch);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !active.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        cache.recenter(10, &all);
        let mut reader = cache.reader(&all[10]);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"ok");
        wait(&cache, 5);
        assert!(!cache.completed(&all[0]));
        cache.stop();
    }
    #[test]
    fn interrupted_segment_resumes_without_duplicate_bytes_for_two_readers() {
        let all = segments(1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let fetch: Fetcher = Arc::new(move |_, _, emit| {
            let attempt = observed.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                emit(b"hello")?;
                return Err(Error::other("connection dropped"));
            }
            emit(b"hel")?;
            emit(b"lo world")
        });
        let cache = SegmentCache::with_fetcher(&all, HlsCacheConfig::default(), fetch);
        let mut first = cache.reader(&all[0]);
        let mut second = cache.reader(&all[0]);
        let mut prefix = [0u8; 5];
        first.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"hello");
        wait(&cache, 1);
        let mut all_bytes = Vec::new();
        first.seek(SeekFrom::Start(0)).unwrap();
        first.read_to_end(&mut all_bytes).unwrap();
        assert_eq!(all_bytes, b"hello world");
        all_bytes.clear();
        second.read_to_end(&mut all_bytes).unwrap();
        assert_eq!(all_bytes, b"hello world");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        cache.stop();
    }

    #[test]
    fn changed_segment_on_retry_fails_without_appending_corrupt_bytes() {
        let entry = segments(1).remove(0);
        let download = Arc::new(Download::new());
        let attempts = AtomicUsize::new(0);
        let fetch: Fetcher = Arc::new(move |_, _, emit| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                emit(b"prefix")?;
                Err(Error::other("short HTTP response"))
            } else {
                emit(b"prEfix_changed")
            }
        });
        let error = fetch_with_retries(&entry, &download, &fetch).unwrap_err();
        assert!(error.to_string().contains("prefix mismatch"), "{error}");
        assert_eq!(download.body.lock().unwrap().bytes, b"prefix");
    }

    #[test]
    fn repeated_segment_errors_exhaust_bounded_budget_without_byte_duplication() {
        let entry = segments(1).remove(0);
        let download = Arc::new(Download::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let fetch: Fetcher = Arc::new(move |_, _, emit| {
            observed.fetch_add(1, Ordering::SeqCst);
            emit(b"partial")?;
            Err(Error::other("transient transport EOF"))
        });
        let error = fetch_with_retries(&entry, &download, &fetch).unwrap_err();
        assert!(error.to_string().contains("after 3 attempts"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(download.body.lock().unwrap().bytes, b"partial");
    }

    #[test]
    fn cancellation_during_retry_backoff_skips_further_http_attempts() {
        let entry = segments(1).remove(0);
        let download = Arc::new(Download::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let fetch: Fetcher = Arc::new(move |_, _, emit| {
            observed.fetch_add(1, Ordering::SeqCst);
            emit(b"first")?;
            Err(Error::other("connection closed"))
        });
        let cancel = Arc::clone(&download);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cancel.cancel();
        });
        let error = fetch_with_retries(&entry, &download, &fetch).unwrap_err();
        stopper.join().unwrap();
        assert!(error.is_cancelled(), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn real_http_truncated_byterange_is_retried_with_verified_prefix() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(
                    String::from_utf8_lossy(&request).contains("bytes=4-9"),
                    "byte range must be preserved across retries"
                );
                let header = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/20\r\nContent-Length: 6\r\nConnection: close\r\n\r\n";
                socket.write_all(header).unwrap();
                socket
                    .write_all(if attempt == 0 { b"abc" } else { b"abcdef" })
                    .unwrap();
            }
        });
        let entry = SegmentEntry {
            url: Url::parse(&format!("http://{addr}/segment.ts")).unwrap(),
            range: Some(ByteWindowSpec {
                offset: 4,
                length: 6,
            }),
            map: None,
            start_seconds: 0.0,
            duration_seconds: 10.0,
        };
        let cache = SegmentCache::new(std::slice::from_ref(&entry), HlsCacheConfig::default());
        let mut reader = cache.reader(&entry);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abcdef");
        server.join().unwrap();
        cache.stop();
    }

    #[test]
    fn real_http_stalled_segment_closes_promptly_on_cache_stop() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = origin.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = origin.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut header = Vec::new();
            let mut byte = [0u8; 1];
            while !header.ends_with(b"\r\n\r\n") {
                conn.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            conn.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: keep-alive\r\n\r\nhello",
            )
            .unwrap();
            conn.flush().unwrap();
            match conn.read(&mut byte) {
                Ok(0) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                _ => false,
            }
        });
        let entries = vec![SegmentEntry {
            url: Url::parse(&format!("http://{endpoint}/0.ts")).unwrap(),
            range: None,
            map: None,
            start_seconds: 0.0,
            duration_seconds: 10.0,
        }];
        let cache = SegmentCache::new(&entries, HlsCacheConfig::default());
        let mut reader = cache.reader(&entries[0]);
        let mut prefix = [0u8; 5];
        reader.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"hello");
        let start = Instant::now();
        cache.stop();
        assert!(reader.read(&mut [0u8; 1]).is_err());
        assert!(
            server.join().unwrap(),
            "stalled HTTP segment stayed connected"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "cache HTTP cancellation was slow"
        );
    }
}
