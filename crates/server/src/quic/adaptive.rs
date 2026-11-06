//! Auto chooses a fixed rendition for each independently encoded time range.
use std::time::{Duration, Instant};

use crate::transcode::{quality_bitrate, TranscodeQuality};

pub(super) struct AdaptiveQuality {
    quality: TranscodeQuality,
    last_change: Instant,
    high_since: Option<Instant>,
    last_observed: Option<Instant>,
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
            last_observed: None,
        }
    }

    pub fn quality(&self) -> TranscodeQuality {
        self.quality
    }

    pub fn observe(
        &mut self,
        buffered_ms: u32,
        target_ms: u32,
        delivery_metrics: bool,
        delivery_bps: Option<u64>,
        now: Instant,
    ) {
        if delivery_metrics
            && self.last_observed.is_some_and(|previous| {
                now.saturating_duration_since(previous) > Duration::from_secs(3)
            })
        {
            self.high_since = None;
        }
        self.last_observed = Some(now);
        // Paused/prepared clients intentionally keep a small horizon. This is
        // backpressure, not evidence that their network cannot sustain audio.
        if target_ms < 8000 {
            self.high_since = None;
            return;
        }
        let delivery_bps = delivery_bps.filter(|_| delivery_metrics);
        let insufficient_delivery = delivery_bps
            .is_some_and(|rate| rate < u64::from(quality_bitrate(self.quality)) * 5 / 4);
        if buffered_ms < 3000 || insufficient_delivery {
            self.high_since = None;
            if now.duration_since(self.last_change) >= Duration::from_secs(5) {
                let lower = match self.quality {
                    TranscodeQuality::High => TranscodeQuality::Medium,
                    _ => TranscodeQuality::Low,
                };
                self.quality = match delivery_bps {
                    // A severe drop can skip Medium rather than spend another
                    // range exhausting the reserve at an unsustainable rate.
                    Some(rate)
                        if rate < u64::from(quality_bitrate(TranscodeQuality::Medium)) * 5 / 4 =>
                    {
                        TranscodeQuality::Low
                    }
                    _ => lower,
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
        let higher = match self.quality {
            TranscodeQuality::Low => TranscodeQuality::Medium,
            TranscodeQuality::Medium => TranscodeQuality::High,
            TranscodeQuality::High => return,
        };
        // Old adaptive clients retain their buffer-based policy. A measured
        // client must prove headroom; an absent/stale sample is not fast LTE.
        if delivery_metrics
            && !delivery_bps.is_some_and(|rate| rate >= u64::from(quality_bitrate(higher)) * 3 / 2)
        {
            self.high_since = None;
            return;
        }
        let since = *self.high_since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_secs(30) {
            self.quality = higher;
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
        auto.observe(0, 8000, false, None, now);
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(500, 8000, false, None, now + Duration::from_secs(5));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        auto.observe(16000, 20000, false, None, now + Duration::from_secs(6));
        auto.observe(16000, 20000, false, None, now + Duration::from_secs(35));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        auto.observe(16000, 20000, false, None, now + Duration::from_secs(36));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(0, 20000, false, None, now + Duration::from_secs(41));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
    }

    #[test]
    fn paused_prepared_and_interrupted_reserves_cannot_trigger_an_upgrade() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(100, 2000, false, None, now + Duration::from_secs(10));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(9000, 8000, false, None, now + Duration::from_secs(11));
        auto.observe(4000, 8000, false, None, now + Duration::from_secs(40));
        auto.observe(9000, 8000, false, None, now + Duration::from_secs(41));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
    }

    #[test]
    fn delivery_shortfall_reduces_quality_before_a_large_buffer_runs_out() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(
            16_000,
            20_000,
            true,
            Some(75_000),
            now + Duration::from_secs(5),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        // A temporary fast burst cannot immediately reverse the decision.
        auto.observe(
            16_000,
            20_000,
            true,
            Some(500_000),
            now + Duration::from_secs(6),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        for second in 7..=36 {
            auto.observe(
                16_000,
                20_000,
                true,
                Some(500_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        for second in 37..=66 {
            auto.observe(
                16_000,
                20_000,
                true,
                Some(500_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
        auto.observe(
            16_000,
            20_000,
            true,
            Some(150_000),
            now + Duration::from_secs(71),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(
            16_000,
            20_000,
            true,
            Some(70_000),
            now + Duration::from_secs(76),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Low);
    }

    #[test]
    fn stable_buffer_cannot_upgrade_without_fresh_capacity_and_headroom() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(16_000, 20_000, true, Some(400_000), now);
        auto.observe(16_000, 20_000, true, None, now + Duration::from_secs(29));
        auto.observe(
            16_000,
            20_000,
            true,
            Some(400_000),
            now + Duration::from_secs(30),
        );
        auto.observe(
            16_000,
            20_000,
            true,
            Some(190_000),
            now + Duration::from_secs(59),
        );
        auto.observe(
            16_000,
            20_000,
            true,
            Some(400_000),
            now + Duration::from_secs(60),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        for second in 61..=90 {
            auto.observe(
                16_000,
                20_000,
                true,
                Some(400_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
    }

    #[test]
    fn pause_and_preparation_do_not_consume_connection_quality_evidence() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(100, 2000, true, Some(10_000), now + Duration::from_secs(10));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        auto.observe(
            16_000,
            20_000,
            true,
            Some(400_000),
            now + Duration::from_secs(11),
        );
        auto.observe(
            5000,
            5000,
            true,
            Some(400_000),
            now + Duration::from_secs(40),
        );
        auto.observe(
            16_000,
            20_000,
            true,
            Some(400_000),
            now + Duration::from_secs(41),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
    }

    #[test]
    fn measured_client_can_still_downshift_on_buffer_pressure_without_a_rate_sample() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(500, 8000, true, None, now + Duration::from_secs(5));
        assert_eq!(auto.quality(), TranscodeQuality::Low);
    }

    #[test]
    fn a_gap_in_connection_reports_cannot_count_as_sustained_headroom() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(16_000, 20_000, true, Some(400_000), now);
        auto.observe(
            16_000,
            20_000,
            true,
            Some(400_000),
            now + Duration::from_secs(31),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        for second in 32..=61 {
            auto.observe(
                16_000,
                20_000,
                true,
                Some(400_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
    }
}
