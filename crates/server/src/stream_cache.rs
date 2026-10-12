use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::{Condvar, Mutex};

use crate::transcode::{TranscodeMode, TranscodeQuality};

const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;
pub const MEMORY_CACHE_MAX_BYTES: usize = MAX_CACHE_BYTES * MAX_CACHE_ENTRIES;
pub const CACHE_RANGE_MS: u32 = 5_000;
const SEEK_PREROLL_MS: u32 = 80;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub track_id: String,
    pub frame_ms: u32,
    pub mode: TranscodeMode,
    pub quality: TranscodeQuality,
    pub range_start_ms: u32,
    source: String,
}

impl CacheKey {
    pub fn new(
        track_id: &str,
        frame_ms: u32,
        mode: TranscodeMode,
        quality: TranscodeQuality,
    ) -> Self {
        Self {
            source: String::new(),
            track_id: track_id.into(),
            frame_ms,
            mode,
            quality,
            range_start_ms: 0,
        }
    }
    pub fn with_source(mut self, path: &Path) -> io::Result<Self> {
        let metadata = path.metadata()?;
        self.source = format!(
            "{}:{}:{:?}",
            path.display(),
            metadata.len(),
            metadata.modified()?
        );
        Ok(self)
    }
    pub fn with_range(mut self, start_ms: u32) -> Self {
        self.range_start_ms = start_ms;
        self
    }
    pub fn source_version(&self) -> String {
        blake3::hash(self.source.as_bytes()).to_hex().to_string()
    }
}

#[derive(Clone)]
pub struct StreamCache {
    enabled: bool,
    entries: Arc<Mutex<CacheEntries>>,
    budget: Arc<tokio::sync::Semaphore>,
}
#[derive(Default)]
struct CacheEntries {
    items: HashMap<CacheKey, Arc<MemoryCache>>,
    order: VecDeque<CacheKey>,
}
impl StreamCache {
    pub fn new(_root: PathBuf, enabled: bool) -> Self {
        Self {
            enabled,
            entries: Arc::new(Mutex::new(CacheEntries::default())),
            budget: Arc::new(tokio::sync::Semaphore::new(MAX_CACHE_ENTRIES)),
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn transient(&self, key: &CacheKey) -> io::Result<(CacheWriter, CacheReader)> {
        let permit = self
            .reserve(&mut self.entries.lock())
            .ok_or_else(|| io::Error::other("stream cache capacity exhausted; retry playback"))?;
        let cache = Arc::new(MemoryCache::new(
            key.frame_ms,
            key.range_start_ms,
            MAX_CACHE_BYTES,
            Some(permit),
        ));
        Ok((
            CacheWriter {
                cache: Arc::clone(&cache),
                retained: false,
                index: 0,
                finalized: false,
            },
            CacheReader { cache },
        ))
    }
    pub fn reader(&self, key: &CacheKey) -> io::Result<Option<CacheReader>> {
        if !self.enabled {
            return Ok(None);
        }
        let mut entries = self.entries.lock();
        if let Some(cache) = entries.items.get(key).cloned() {
            entries.order.retain(|item| item != key);
            entries.order.push_back(key.clone());
            return Ok(Some(CacheReader { cache }));
        }
        Ok(None)
    }
    /// One independent producer per source/rendition/range. Partial frames survive
    /// cancellation and can be retried; readers never turn producer failure into EOS.
    pub fn writer(&self, key: CacheKey) -> io::Result<Option<CacheWriter>> {
        if !self.enabled {
            return Ok(None);
        }
        let mut entries = self.entries.lock();
        if let Some(cache) = entries.items.get(&key).cloned() {
            let mut state = cache.state.lock();
            if state.producing || state.complete {
                return Ok(None);
            }
            state.producing = true;
            state.failure = None;
            drop(state);
            return Ok(Some(CacheWriter {
                cache,
                retained: true,
                index: 0,
                finalized: false,
            }));
        }
        let Some(permit) = self.reserve(&mut entries) else {
            return Ok(None);
        };
        let cache = Arc::new(MemoryCache::new(
            key.frame_ms,
            key.range_start_ms,
            MAX_CACHE_BYTES,
            Some(permit),
        ));
        entries.items.insert(key.clone(), Arc::clone(&cache));
        entries.order.push_back(key);
        Ok(Some(CacheWriter {
            cache,
            retained: true,
            index: 0,
            finalized: false,
        }))
    }
    fn reserve(&self, entries: &mut CacheEntries) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if let Ok(permit) = Arc::clone(&self.budget).try_acquire_owned() {
            return Some(permit);
        }
        let victim = entries
            .order
            .iter()
            .find(|key| {
                entries
                    .items
                    .get(*key)
                    .map(|cache| Arc::strong_count(cache) == 1)
                    .unwrap_or(false)
            })
            .cloned()?;
        entries.items.remove(&victim);
        entries.order.retain(|item| item != &victim);
        Arc::clone(&self.budget).try_acquire_owned().ok()
    }
}

struct MemoryCache {
    _reservation: Option<tokio::sync::OwnedSemaphorePermit>,
    frame_ms: u32,
    range_start_ms: u32,
    max_bytes: usize,
    state: Mutex<CacheState>,
    ready: Condvar,
}
struct CacheState {
    header: Option<Bytes>,
    frames: Vec<Bytes>,
    bytes: usize,
    producing: bool,
    complete: bool,
    failure: Option<String>,
}
impl MemoryCache {
    fn new(
        frame_ms: u32,
        range_start_ms: u32,
        max_bytes: usize,
        reservation: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            _reservation: reservation,
            frame_ms,
            range_start_ms,
            max_bytes,
            state: Mutex::new(CacheState {
                header: None,
                frames: Vec::new(),
                bytes: 0,
                producing: true,
                complete: false,
                failure: None,
            }),
            ready: Condvar::new(),
        }
    }
}
/// Pins a range against eviction. Dropping the final playback reader does not
/// discard reusable encoded data or stop its bounded 5-second producer.
pub struct CacheGuard {
    _cache: Arc<MemoryCache>,
}
pub struct CacheReader {
    cache: Arc<MemoryCache>,
}
impl CacheReader {
    pub fn guard(&self) -> CacheGuard {
        CacheGuard {
            _cache: Arc::clone(&self.cache),
        }
    }
    pub fn can_start(&self, start_ms: u32) -> bool {
        let state = self.cache.state.lock();
        state.header.is_some()
            && (start_ms.saturating_sub(self.cache.range_start_ms) / self.cache.frame_ms)
                < state.frames.len() as u32
    }
    pub fn stream_to(
        &self,
        start_ms: u32,
        tx: &tokio::sync::mpsc::Sender<Result<Bytes, io::Error>>,
    ) -> Result<(), String> {
        let header = {
            let mut state = self.cache.state.lock();
            loop {
                if tx.is_closed() {
                    return Err("stream closed".into());
                }
                if let Some(header) = &state.header {
                    break header.clone();
                }
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                if !state.producing {
                    return Err("cache range has no header".into());
                }
                self.cache
                    .ready
                    .wait_for(&mut state, Duration::from_millis(25));
            }
        };
        let (header, mut index) = replay_header(
            &header,
            start_ms.saturating_sub(self.cache.range_start_ms),
            self.cache.frame_ms,
        )?;
        tx.blocking_send(Ok(header)).map_err(|_| "stream closed")?;
        loop {
            let frame = {
                let mut state = self.cache.state.lock();
                loop {
                    if tx.is_closed() {
                        return Err("stream closed".into());
                    }
                    if let Some(frame) = state.frames.get(index) {
                        index += 1;
                        break Some(frame.clone());
                    }
                    if let Some(error) = &state.failure {
                        return Err(error.clone());
                    }
                    if state.complete {
                        break None;
                    }
                    if !state.producing {
                        return Err("cache range ended without completion".into());
                    }
                    self.cache
                        .ready
                        .wait_for(&mut state, Duration::from_millis(25));
                }
            };
            match frame {
                Some(frame) => tx.blocking_send(Ok(frame)).map_err(|_| "stream closed")?,
                None => {
                    tx.blocking_send(Ok(Bytes::from_static(&[0, 0])))
                        .map_err(|_| "stream closed")?;
                    return Ok(());
                }
            }
        }
    }
}

/// Cached interior seeks need decoder preroll and exact sample trimming. Header
/// pre_skip (samples/channel at offset 26) already includes encoder lookahead.
fn replay_header(header: &Bytes, offset_ms: u32, frame_ms: u32) -> Result<(Bytes, usize), String> {
    if header.len() < 28 || &header[..8] != b"OPUSR01\0" {
        return Ok((header.clone(), (offset_ms / frame_ms) as usize));
    }
    let rate = u32::from_le_bytes(header[12..16].try_into().unwrap()) as u64;
    let lookahead = u16::from_le_bytes(header[26..28].try_into().unwrap()) as u64;
    let frame_samples = rate * frame_ms as u64 / 1000;
    if frame_samples == 0 {
        return Err("invalid cached frame duration".into());
    }
    let target = rate * offset_ms as u64 / 1000 + lookahead;
    let index = (target / frame_samples).saturating_sub((SEEK_PREROLL_MS / frame_ms).max(1) as u64);
    let skip = target - index * frame_samples;
    let skip = u16::try_from(skip).map_err(|_| "cached seek preroll too large")?;
    let mut replay = header.to_vec();
    replay[26..28].copy_from_slice(&skip.to_le_bytes());
    Ok((Bytes::from(replay), index as usize))
}

pub struct CacheWriter {
    cache: Arc<MemoryCache>,
    retained: bool,
    index: usize,
    finalized: bool,
}
impl CacheWriter {
    pub fn has_readers(&self) -> bool {
        Arc::strong_count(&self.cache) > if self.retained { 2 } else { 1 }
    }
    pub fn write_header(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        if state.header.is_some() {
            return Ok(());
        }
        if bytes.len() > self.cache.max_bytes {
            return Err(io::Error::other("memory cache full"));
        }
        state.header = Some(Bytes::copy_from_slice(bytes));
        state.bytes += bytes.len();
        self.cache.ready.notify_all();
        Ok(())
    }
    pub fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        if !state.producing {
            return Err(io::Error::other("cache producer closed"));
        }
        if self.index < state.frames.len() {
            // A retry reproduces the same canonical fixed rendition from its start.
            if state.frames[self.index].as_ref() != bytes {
                return Err(io::Error::other("cache rendition changed during retry"));
            }
            self.index += 1;
            return Ok(());
        }
        if state.bytes + bytes.len() > self.cache.max_bytes {
            return Err(io::Error::other("memory cache full"));
        }
        state.bytes += bytes.len();
        state.frames.push(Bytes::copy_from_slice(bytes));
        self.index += 1;
        self.cache.ready.notify_all();
        Ok(())
    }
    pub fn fail(&mut self, error: impl Into<String>) {
        let mut state = self.cache.state.lock();
        state.failure = Some(error.into());
        state.producing = false;
        self.finalized = true;
        self.cache.ready.notify_all();
    }
    pub fn finalize(mut self) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        state.complete = true;
        state.producing = false;
        self.finalized = true;
        self.cache.ready.notify_all();
        Ok(())
    }
}
impl Drop for CacheWriter {
    fn drop(&mut self) {
        if !self.finalized {
            self.fail("cache producer stopped before completing range");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: &str) -> CacheKey {
        CacheKey::new(id, 20, TranscodeMode::Fixed, TranscodeQuality::High)
    }
    #[test]
    fn disabled_cache_never_creates_entries() {
        let cache = StreamCache::new(PathBuf::new(), false);
        assert!(cache.writer(key("a")).unwrap().is_none());
        assert!(cache.reader(&key("a")).unwrap().is_none());
    }
    #[test]
    fn concurrent_requests_share_one_range_producer() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let first = cache.writer(key("a")).unwrap().unwrap();
        assert!(cache.writer(key("a")).unwrap().is_none());
        assert!(cache
            .writer(key("a").with_range(CACHE_RANGE_MS))
            .unwrap()
            .is_some());
        drop(first);
        assert!(cache.writer(key("a")).unwrap().is_some());
    }
    #[test]
    fn partial_frames_survive_reader_drop_and_failure_is_not_eos() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let mut writer = cache.writer(key("a")).unwrap().unwrap();
        writer.write_header(&[1, 2]).unwrap();
        writer.write_frame(&[3, 4]).unwrap();
        let reader = cache.reader(&key("a")).unwrap().unwrap();
        drop(reader.guard());
        writer.fail("source read failed");
        assert!(reader.can_start(0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        assert_eq!(reader.stream_to(0, &tx).unwrap_err(), "source read failed");
        assert_eq!(rx.try_recv().unwrap().unwrap(), Bytes::from_static(&[1, 2]));
        assert_eq!(rx.try_recv().unwrap().unwrap(), Bytes::from_static(&[3, 4]));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn completed_range_sends_one_eos_after_its_frames() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let mut writer = cache.writer(key("a")).unwrap().unwrap();
        writer.write_header(&[1]).unwrap();
        writer.write_frame(&[2]).unwrap();
        writer.finalize().unwrap();
        let reader = cache.reader(&key("a")).unwrap().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reader.stream_to(0, &tx).unwrap();
        assert_eq!(rx.try_recv().unwrap().unwrap(), Bytes::from_static(&[1]));
        assert_eq!(rx.try_recv().unwrap().unwrap(), Bytes::from_static(&[2]));
        assert_eq!(rx.try_recv().unwrap().unwrap(), Bytes::from_static(&[0, 0]));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn retry_retains_existing_prefix_without_duplicate_frames() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let mut first = cache.writer(key("a")).unwrap().unwrap();
        first.write_header(&[1]).unwrap();
        first.write_frame(&[2]).unwrap();
        first.fail("temporary");
        let mut retry = cache.writer(key("a")).unwrap().unwrap();
        retry.write_header(&[1]).unwrap();
        retry.write_frame(&[2]).unwrap();
        retry.write_frame(&[3]).unwrap();
        retry.finalize().unwrap();
        let reader = cache.reader(&key("a")).unwrap().unwrap();
        assert_eq!(reader.cache.state.lock().frames.len(), 2);
    }
    #[test]
    fn replay_header_accounts_for_lookahead_preroll_and_subframe_seek() {
        let mut header = vec![0; 28];
        header[..8].copy_from_slice(b"OPUSR01\0");
        header[12..16].copy_from_slice(&48000u32.to_le_bytes());
        header[26..28].copy_from_slice(&312u16.to_le_bytes());
        let (header, index) = replay_header(&Bytes::from(header), 1005, 20).unwrap();
        assert_eq!(index, 46);
        let skip = u16::from_le_bytes(header[26..28].try_into().unwrap()) as u64;
        assert_eq!(index as u64 * 960 + skip, 48000 * 1005 / 1000 + 312);
    }
    #[test]
    fn cache_budget_is_bounded_and_live_ranges_are_not_evicted() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let writers: Vec<_> = (0..MAX_CACHE_ENTRIES)
            .map(|n| cache.writer(key(&n.to_string())).unwrap().unwrap())
            .collect();
        assert!(cache.writer(key("overflow")).unwrap().is_none());
        assert_eq!(MEMORY_CACHE_MAX_BYTES, 256 * 1024 * 1024);
        drop(writers);
        assert!(cache.writer(key("overflow")).unwrap().is_some());
    }
    #[test]
    fn cancelled_reader_wakes_while_waiting_for_a_missing_header() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let _writer = cache.writer(key("waiting")).unwrap().unwrap();
        let reader = cache.reader(&key("waiting")).unwrap().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let task = std::thread::spawn(move || reader.stream_to(0, &tx));
        drop(rx);
        assert_eq!(task.join().unwrap().unwrap_err(), "stream closed");
    }
    #[test]
    fn partial_reader_is_ready_before_completion_and_failure_wakes_its_gap() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let mut writer = cache.writer(key("live")).unwrap().unwrap();
        writer.write_header(&[1]).unwrap();
        writer.write_frame(&[2]).unwrap();
        let reader = cache.reader(&key("live")).unwrap().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let task = std::thread::spawn(move || reader.stream_to(0, &tx));
        assert_eq!(
            rx.blocking_recv().unwrap().unwrap(),
            Bytes::from_static(&[1])
        );
        assert_eq!(
            rx.blocking_recv().unwrap().unwrap(),
            Bytes::from_static(&[2])
        );
        writer.fail("decode failed at gap");
        assert_eq!(task.join().unwrap().unwrap_err(), "decode failed at gap");
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn ephemeral_disabled_cache_uses_and_releases_the_same_memory_budget() {
        let cache = StreamCache::new(PathBuf::new(), false);
        let mut ranges: Vec<_> = (0..MAX_CACHE_ENTRIES)
            .map(|n| cache.transient(&key(&n.to_string())).unwrap())
            .collect();
        assert!(cache.transient(&key("overflow")).is_err());
        drop(ranges.pop());
        assert!(cache.transient(&key("replacement")).is_ok());
    }
    #[test]
    fn retained_and_ephemeral_ranges_share_one_budget() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let _shared: Vec<_> = (0..MAX_CACHE_ENTRIES / 2)
            .map(|n| cache.writer(key(&n.to_string())).unwrap().unwrap())
            .collect();
        let mut temporary: Vec<_> = (0..MAX_CACHE_ENTRIES / 2)
            .map(|n| cache.transient(&key(&format!("temporary-{n}"))).unwrap())
            .collect();
        assert!(cache.transient(&key("overflow")).is_err());
        assert!(cache.writer(key("overflow")).unwrap().is_none());
        drop(temporary.pop());
        assert!(cache.writer(key("replacement")).unwrap().is_some());
    }
    #[test]
    fn cache_writer_enforces_range_memory_limit() {
        let cache = Arc::new(MemoryCache::new(20, 0, 4, None));
        let mut writer = CacheWriter {
            cache,
            retained: true,
            index: 0,
            finalized: false,
        };
        writer.write_header(&[1, 2]).unwrap();
        writer.write_frame(&[3, 4]).unwrap();
        assert_eq!(
            writer.write_frame(&[5]).unwrap_err().to_string(),
            "memory cache full"
        );
    }
    #[test]
    fn queued_producers_observe_cancelled_shared_and_ephemeral_demand() {
        let cache = StreamCache::new(PathBuf::new(), true);
        let range = key("shared");
        let writer = cache.writer(range.clone()).unwrap().unwrap();
        let reader = cache.reader(&range).unwrap().unwrap();
        assert!(writer.has_readers());
        drop(reader);
        assert!(!writer.has_readers());
        let (writer, reader) = cache.transient(&key("temporary")).unwrap();
        assert!(writer.has_readers());
        drop(reader);
        assert!(!writer.has_readers());
    }
}
