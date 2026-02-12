use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use uuid::Uuid;

use crate::transcode::TranscodeQuality;

const SESSION_TTL: Duration = Duration::from_secs(90);
const DOWN_SHIFT_MS: u64 = 2000;
const UP_SHIFT_MS: u64 = 8000;
const UP_STABLE_SECS: u64 = 8;
const CHANGE_COOLDOWN_SECS: u64 = 4;

#[derive(Clone)]
pub struct StreamSessions {
    inner: Arc<RwLock<HashMap<Uuid, StreamSession>>>,
}

#[derive(Clone)]
pub struct StreamSessionHandle {
    pub id: Uuid,
    pub target_bitrate_bps: Arc<AtomicU32>,
}

struct StreamSession {
    target_bitrate_bps: Arc<AtomicU32>,
    quality: StreamQuality,
    last_seen: Instant,
    last_change: Instant,
    high_since: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamQuality {
    Low,
    Medium,
    High,
}

impl StreamSessions {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create_session(&self, initial: TranscodeQuality) -> StreamSessionHandle {
        let id = Uuid::new_v4();
        let quality = StreamQuality::from_transcode(initial);
        let bitrate_bps = quality.bitrate_bps();
        let target_bitrate_bps = Arc::new(AtomicU32::new(bitrate_bps));
        let now = Instant::now();
        let session = StreamSession {
            target_bitrate_bps: Arc::clone(&target_bitrate_bps),
            quality,
            last_seen: now,
            last_change: now,
            high_since: None,
        };
        self.inner.write().insert(id, session);
        StreamSessionHandle {
            id,
            target_bitrate_bps,
        }
    }

    pub fn report_buffer(&self, id: Uuid, buffer_ms: u64) {
        let now = Instant::now();
        let mut guard = self.inner.write();
        guard.retain(|_, session| now.duration_since(session.last_seen) <= SESSION_TTL);
        let Some(session) = guard.get_mut(&id) else {
            return;
        };
        session.last_seen = now;

        if buffer_ms >= UP_SHIFT_MS {
            if session.high_since.is_none() {
                session.high_since = Some(now);
            }
        } else {
            session.high_since = None;
        }

        let since_change = now.duration_since(session.last_change);
        if buffer_ms < DOWN_SHIFT_MS && since_change >= Duration::from_secs(CHANGE_COOLDOWN_SECS) {
            if session.quality.downshift() {
                session.last_change = now;
                session.high_since = None;
                session
                    .target_bitrate_bps
                    .store(session.quality.bitrate_bps(), Ordering::Relaxed);
            }
            return;
        }

        if let Some(since_high) = session.high_since {
            if since_change >= Duration::from_secs(CHANGE_COOLDOWN_SECS)
                && now.duration_since(since_high) >= Duration::from_secs(UP_STABLE_SECS)
            {
                if session.quality.upshift() {
                    session.last_change = now;
                    session
                        .target_bitrate_bps
                        .store(session.quality.bitrate_bps(), Ordering::Relaxed);
                }
            }
        }
    }
}

impl StreamQuality {
    fn from_transcode(value: TranscodeQuality) -> Self {
        match value {
            TranscodeQuality::High => StreamQuality::High,
            TranscodeQuality::Medium => StreamQuality::Medium,
            TranscodeQuality::Low => StreamQuality::Low,
        }
    }

    fn bitrate_bps(self) -> u32 {
        match self {
            StreamQuality::High => 160_000,
            StreamQuality::Medium => 96_000,
            StreamQuality::Low => 48_000,
        }
    }

    fn downshift(&mut self) -> bool {
        match self {
            StreamQuality::High => {
                *self = StreamQuality::Medium;
                true
            }
            StreamQuality::Medium => {
                *self = StreamQuality::Low;
                true
            }
            StreamQuality::Low => false,
        }
    }

    fn upshift(&mut self) -> bool {
        match self {
            StreamQuality::Low => {
                *self = StreamQuality::Medium;
                true
            }
            StreamQuality::Medium => {
                *self = StreamQuality::High;
                true
            }
            StreamQuality::High => false,
        }
    }
}
