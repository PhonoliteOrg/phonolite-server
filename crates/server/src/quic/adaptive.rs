//! Auto chooses a fixed rendition for each independently encoded time range.
use std::time::{Duration, Instant};

use crate::transcode::TranscodeQuality;

pub(super) struct AdaptiveQuality {
    quality: TranscodeQuality,
    last_change: Instant,
    high_since: Option<Instant>,
}

impl AdaptiveQuality {
    pub fn negotiate(mode: Option<&str>, requested: bool, now: Instant) -> Option<Self> {
        (requested && mode == Some("auto")).then(|| Self::new(now))
    }

    pub fn new(now: Instant) -> Self {
        Self {
            quality: TranscodeQuality::Medium,
            last_change: now,
            high_since: None,
        }
    }

    pub fn quality(&self) -> TranscodeQuality {
        self.quality
    }

    pub fn observe(&mut self, buffered_ms: u32, target_ms: u32, now: Instant) {
        // Paused/prepared clients intentionally keep a small horizon. This is
        // backpressure, not evidence that their network cannot sustain audio.
        if target_ms < 8000 {
            self.high_since = None;
            return;
        }
        if buffered_ms < 3000 {
            self.high_since = None;
            if now.duration_since(self.last_change) >= Duration::from_secs(5) {
                self.quality = match self.quality {
                    TranscodeQuality::High => TranscodeQuality::Medium,
                    _ => TranscodeQuality::Low,
                };
                self.last_change = now;
            }
            return;
        }

        let upgrade_buffer = target_ms.saturating_sub(1000).clamp(8000, 12000);
        if buffered_ms < upgrade_buffer {
            self.high_since = None;
            return;
        }
        let since = *self.high_since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_secs(30) {
            self.quality = match self.quality {
                TranscodeQuality::Low => TranscodeQuality::Medium,
                _ => TranscodeQuality::High,
            };
            self.last_change = now;
            self.high_since = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptation_requires_both_auto_and_explicit_client_support() {
        let now = Instant::now();
        assert!(AdaptiveQuality::negotiate(Some("auto"), false, now).is_none());
        assert!(AdaptiveQuality::negotiate(Some("fixed"), true, now).is_none());
        assert!(AdaptiveQuality::negotiate(Some("original"), true, now).is_none());
        assert!(AdaptiveQuality::negotiate(None, true, now).is_none());
        assert!(AdaptiveQuality::negotiate(Some("auto"), true, now).is_some());
    }

    #[test]
    fn auto_downshifts_quickly_and_requires_sustained_reserve_to_upgrade() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(0, 8000, now);
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(500, 8000, now + Duration::from_secs(5));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        auto.observe(16000, 20000, now + Duration::from_secs(6));
        auto.observe(16000, 20000, now + Duration::from_secs(35));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        auto.observe(16000, 20000, now + Duration::from_secs(36));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(0, 20000, now + Duration::from_secs(41));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
    }

    #[test]
    fn paused_prepared_and_interrupted_reserves_cannot_trigger_an_upgrade() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(100, 2000, now + Duration::from_secs(10));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(9000, 8000, now + Duration::from_secs(11));
        auto.observe(4000, 8000, now + Duration::from_secs(40));
        auto.observe(9000, 8000, now + Duration::from_secs(41));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
    }
}
