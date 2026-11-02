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
    fn append(&self, chunk: &[u8]) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::cancelled("HLS download cancelled"));
        }
        let mut body = self.body.lock().unwrap();
        if body.bytes.len().saturating_add(chunk.len()) > MAX_SEGMENT_BYTES {
            return Err(Error::unsupported(
                "HLS segment exceeds 128 MiB safety ceiling",
            ));
        }
        body.bytes.extend_from_slice(chunk);
        self.ready.notify_all();
        Ok(())
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
        self.entries.retain(|key, download| {
            if keep.contains(key) {
                true
            } else {
                download.cancel();
                false
            }
        });
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
            return Some((entry, download));
        }
        None
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
                (existing, false)
            } else {
                state.misses += 1;
                let d = Arc::new(Download::new());
                state.entries.insert(key, Arc::clone(&d));
                (d, true)
            }
        };
        if start {
            let entry = segment.clone();
            let d = Arc::clone(&download);
            let fetch = Arc::clone(&self.fetch);
            if let Err(err) = thread::Builder::new()
                .name("oxideav-hls-demand".into())
                .spawn(move || perform_fetch(&entry, &d, &fetch))
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
        for entry in state.entries.values() {
            entry.cancel();
        }
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
    let bytes = oxideav_http::open_http(segment.url.as_str())?;
    let mut input = windowed_reader(bytes, segment.range)?;
    let mut buf = [0u8; 65536];
    loop {
        if cancel.is_cancelled() {
            return Err(Error::cancelled("HLS download cancelled"));
        }
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        emit(&buf[..n])?;
    }
}
fn perform_fetch(segment: &SegmentEntry, download: &Arc<Download>, fetch: &Fetcher) {
    let result = fetch(segment, &download.cancellation, &|bytes| {
        download.append(bytes)
    });
    if let Err(ref err) = result {
        log::warn!(
            "oxideav-hls: download failed segment={:.3}s: {err}",
            segment.start_seconds
        );
    }
    download.finish(result);
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
            perform_fetch(&segment, &download, &fetch);
        } else {
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
}
