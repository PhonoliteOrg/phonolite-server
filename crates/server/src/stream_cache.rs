use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use bytes::Bytes;
use parking_lot::{Condvar, Mutex};

use crate::transcode::{TranscodeMode, TranscodeQuality};

const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;
pub const MEMORY_CACHE_MAX_BYTES: usize = MAX_CACHE_BYTES;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub track_id: String,
    pub frame_ms: u32,
    pub mode: TranscodeMode,
    pub quality: TranscodeQuality,
}

impl CacheKey {
    pub fn new(
        track_id: &str,
        frame_ms: u32,
        mode: TranscodeMode,
        quality: TranscodeQuality,
    ) -> Self {
        Self {
            track_id: track_id.to_string(),
            frame_ms,
            mode,
            quality,
        }
    }
}

#[derive(Clone)]
pub struct StreamCache {
    enabled: bool,
    entries: Arc<Mutex<HashMap<CacheKey, Weak<MemoryCache>>>>,
}

impl StreamCache {
    pub fn new(_root: PathBuf, enabled: bool) -> Self {
        Self {
            enabled,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn reader(&self, key: &CacheKey) -> io::Result<Option<CacheReader>> {
        if !self.enabled {
            return Ok(None);
        }
        let mut guard = self.entries.lock();
        if let Some(existing) = guard.get(key) {
            if let Some(cache) = existing.upgrade() {
                return Ok(Some(CacheReader { cache }));
            }
            guard.remove(key);
        }
        Ok(None)
    }

    pub fn writer(&self, key: CacheKey) -> io::Result<Option<CacheWriter>> {
        if !self.enabled {
            return Ok(None);
        }
        let mut guard = self.entries.lock();
        if let Some(existing) = guard.get(&key) {
            if existing.upgrade().is_some() {
                return Ok(None);
            }
            guard.remove(&key);
        }
        let cache = Arc::new(MemoryCache::new(key.frame_ms, MAX_CACHE_BYTES));
        guard.insert(key, Arc::downgrade(&cache));
        Ok(Some(CacheWriter { cache }))
    }
}

struct MemoryCache {
    frame_ms: u32,
    max_bytes: usize,
    state: Mutex<CacheState>,
    ready: Condvar,
    active_streams: AtomicUsize,
    cancelled: AtomicBool,
}

struct CacheState {
    header: Option<Bytes>,
    frames: Vec<Bytes>,
    bytes: usize,
    complete: bool,
    aborted: bool,
}

impl MemoryCache {
    fn new(frame_ms: u32, max_bytes: usize) -> Self {
        Self {
            frame_ms,
            max_bytes,
            state: Mutex::new(CacheState {
                header: None,
                frames: Vec::new(),
                bytes: 0,
                complete: false,
                aborted: false,
            }),
            ready: Condvar::new(),
            active_streams: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    fn is_complete(&self) -> bool {
        self.state.lock().complete
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock();
        if !state.complete {
            state.aborted = true;
        }
        self.ready.notify_all();
    }
}

pub struct CacheGuard {
    cache: Arc<MemoryCache>,
}

impl CacheGuard {
    fn new(cache: Arc<MemoryCache>) -> Self {
        cache.active_streams.fetch_add(1, Ordering::SeqCst);
        Self { cache }
    }
}

impl Drop for CacheGuard {
    fn drop(&mut self) {
        let remaining = self.cache.active_streams.fetch_sub(1, Ordering::SeqCst) - 1;
        if remaining == 0 && !self.cache.is_complete() {
            self.cache.cancel();
        }
    }
}

pub struct CacheReader {
    cache: Arc<MemoryCache>,
}

impl CacheReader {
    pub fn guard(&self) -> CacheGuard {
        CacheGuard::new(Arc::clone(&self.cache))
    }

    pub fn is_complete(&self) -> bool {
        self.cache.is_complete()
    }

    pub fn can_start(&self, start_ms: u32) -> bool {
        let state = self.cache.state.lock();
        if state.header.is_none() {
            return false;
        }
        let start_frame = (start_ms / self.cache.frame_ms) as usize;
        if start_frame < state.frames.len() {
            return true;
        }
        false
    }

    pub fn stream_to(
        &self,
        start_ms: u32,
        tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    ) -> Result<(), String> {
        let header = {
            let mut state = self.cache.state.lock();
            loop {
                if let Some(header) = state.header.clone() {
                    break header;
                }
                if state.aborted {
                    return Err("cache aborted".to_string());
                }
                self.cache.ready.wait(&mut state);
            }
        };
        tx.blocking_send(Ok(header))
            .map_err(|_| "stream closed".to_string())?;

        let start_frame = (start_ms / self.cache.frame_ms) as usize;
        let mut index = start_frame;
        loop {
            let frame = {
                let mut state = self.cache.state.lock();
                loop {
                    if index < state.frames.len() {
                        let data = state.frames[index].clone();
                        index += 1;
                        break Some(data);
                    }
                    if state.complete || state.aborted {
                        break None;
                    }
                    self.cache.ready.wait(&mut state);
                }
            };

            match frame {
                Some(data) => {
                    tx.blocking_send(Ok(data))
                        .map_err(|_| "stream closed".to_string())?;
                }
                None => {
                    let eos = (0u16).to_le_bytes().to_vec();
                    tx.blocking_send(Ok(Bytes::from(eos)))
                        .map_err(|_| "stream closed".to_string())?;
                    return Ok(());
                }
            }
        }
    }
}

pub struct CacheWriter {
    cache: Arc<MemoryCache>,
}

impl CacheWriter {
    pub fn guard(&self) -> CacheGuard {
        CacheGuard::new(Arc::clone(&self.cache))
    }

    pub fn is_cancelled(&self) -> bool {
        self.cache.is_cancelled()
    }

    pub fn write_header(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        if self.cache.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Other, "cache cancelled"));
        }
        if state.header.is_some() {
            return Ok(());
        }
        let len = bytes.len();
        if state.bytes + len > self.cache.max_bytes {
            state.aborted = true;
            self.cache.ready.notify_all();
            return Err(io::Error::new(io::ErrorKind::Other, "memory cache full"));
        }
        state.header = Some(Bytes::copy_from_slice(bytes));
        state.bytes = state.bytes.saturating_add(len);
        self.cache.ready.notify_all();
        Ok(())
    }

    pub fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        if self.cache.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Other, "cache cancelled"));
        }
        if state.complete || state.aborted {
            return Err(io::Error::new(io::ErrorKind::Other, "cache closed"));
        }
        let len = bytes.len();
        if state.bytes + len > self.cache.max_bytes {
            state.aborted = true;
            self.cache.ready.notify_all();
            return Err(io::Error::new(io::ErrorKind::Other, "memory cache full"));
        }
        state.frames.push(Bytes::copy_from_slice(bytes));
        state.bytes = state.bytes.saturating_add(len);
        self.cache.ready.notify_all();
        Ok(())
    }

    pub fn finalize(mut self) -> io::Result<()> {
        let mut state = self.cache.state.lock();
        if !state.aborted && !self.cache.is_cancelled() {
            state.complete = true;
        }
        self.cache.ready.notify_all();
        Ok(())
    }
}
