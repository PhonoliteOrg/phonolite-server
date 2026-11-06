//! Auto chooses a fixed rendition for each independently encoded time range.
use std::time::{Duration, Instant};

use crate::transcode::{quality_bitrate, TranscodeQuality};

pub(super) struct AdaptiveQuality {
    quality: TranscodeQuality,
    last_change: Instant,
    high_since: Option<Instant>,
    last_observed: Option<Instant>,
    last_diagnostic: Option<Instant>,
    window_resets: u32,
    last_reset_reason: &'static str,
}

pub(super) struct AutoDiagnostic {
    pub reason: &'static str,
    pub quality: TranscodeQuality,
    pub upgrade_buffer_ms: u32,
    pub upgrade_bps: Option<u64>,
    pub healthy_ms: u128,
    pub report_gap: bool,
    pub window_resets: u32,
    pub last_reset_reason: &'static str,
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
            last_diagnostic: None,
            window_resets: 0,
            last_reset_reason: "none",
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
    ) -> Option<AutoDiagnostic> {
        let report_gap = delivery_metrics
            && self.last_observed.is_some_and(|previous| {
                now.saturating_duration_since(previous) > Duration::from_secs(3)
            });
        let was_healthy = self.high_since.is_some();
        if report_gap {
            self.high_since = None;
        }
        self.last_observed = Some(now);
        let reason = self.evaluate(buffered_ms, target_ms, delivery_metrics, delivery_bps, now);
        if was_healthy && (report_gap || self.high_since.is_none()) {
            self.window_resets = self.window_resets.saturating_add(1);
            self.last_reset_reason = if report_gap { "report_gap" } else { reason };
        }
        if self.last_diagnostic.is_some_and(|previous| {
            now.saturating_duration_since(previous) < Duration::from_secs(10)
        }) {
            return None;
        }
        self.last_diagnostic = Some(now);
        let higher = match self.quality {
            TranscodeQuality::Low => Some(TranscodeQuality::Medium),
            TranscodeQuality::Medium => Some(TranscodeQuality::High),
            TranscodeQuality::High => None,
        };
        Some(AutoDiagnostic {
            reason,
            quality: self.quality,
            upgrade_buffer_ms: target_ms.saturating_sub(3000).clamp(5000, 10000),
            upgrade_bps: higher.map(|quality| u64::from(quality_bitrate(quality)) * 3 / 2),
            healthy_ms: self
                .high_since
                .map_or(0, |since| now.saturating_duration_since(since).as_millis()),
            report_gap,
            window_resets: self.window_resets,
            last_reset_reason: self.last_reset_reason,
        })
    }

    fn evaluate(
        &mut self,
        buffered_ms: u32,
        target_ms: u32,
        delivery_metrics: bool,
        delivery_bps: Option<u64>,
        now: Instant,
    ) -> &'static str {
        // Paused/prepared clients intentionally keep a small horizon. This is
        // backpressure, not evidence that their network cannot sustain audio.
        if target_ms < 8000 {
            self.high_since = None;
            // Its intentionally small reserve needs the same refill grace as
            // a cold start when playback becomes active again.
            self.last_change = now;
            return "inactive_horizon";
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
            return if buffered_ms < 3000 {
                "low_reserve"
            } else {
                "low_delivery"
            };
        }

        // Playback must fall below the fetch ceiling to request more audio.
        // Leave a real refill margin instead of requiring an eight-second
        // reserve to remain continuously full against an eight-second target.
        let upgrade_buffer = target_ms.saturating_sub(3000).clamp(5000, 10000);
        if buffered_ms < upgrade_buffer {
            self.high_since = None;
            return "upgrade_reserve";
        }
        let higher = match self.quality {
            TranscodeQuality::Low => TranscodeQuality::Medium,
            TranscodeQuality::Medium => TranscodeQuality::High,
            TranscodeQuality::High => return "highest_quality",
        };
        // Old adaptive clients retain their buffer-based policy. A measured
        // client must prove headroom; an absent/stale sample is not fast LTE.
        if delivery_metrics
            && !delivery_bps.is_some_and(|rate| rate >= u64::from(quality_bitrate(higher)) * 3 / 2)
        {
            self.high_since = None;
            return if delivery_bps.is_none() {
                "missing_delivery_sample"
            } else {
                "upgrade_headroom"
            };
        }
        let since = *self.high_since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_secs(30) {
            self.quality = higher;
            self.last_change = now;
            self.high_since = Some(now);
            return "upgraded";
        }
        "healthy_window"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_bounded_without_throttling_quality_decisions() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        let initial = auto
            .observe(8500, 8000, true, Some(1_000_000), now)
            .unwrap();
        assert_eq!(initial.reason, "healthy_window");
        assert_eq!(initial.upgrade_buffer_ms, 5000);
        assert_eq!(initial.upgrade_bps, Some(240_000));
        for second in 1..10 {
            assert!(auto
                .observe(
                    500,
                    8000,
                    true,
                    Some(70_000),
                    now + Duration::from_secs(second)
                )
                .is_none());
        }
        assert_eq!(auto.quality(), TranscodeQuality::Low);
        let sample = auto
            .observe(
                8500,
                8000,
                true,
                Some(1_000_000),
                now + Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(sample.quality, TranscodeQuality::Low);
        assert_eq!(sample.last_reset_reason, "low_reserve");
        assert_eq!(sample.window_resets, 1);
    }

    #[test]
    fn diagnostic_distinguishes_unavailable_capacity_from_insufficient_headroom() {
        let now = Instant::now();
        for (rate, reason) in [
            (None, "missing_delivery_sample"),
            (Some(190_000), "upgrade_headroom"),
        ] {
            let mut auto = AdaptiveQuality::new(now);
            let sample = auto.observe(8500, 8000, true, rate, now).unwrap();
            assert_eq!(sample.reason, reason);
            assert_eq!(sample.healthy_ms, 0);
            assert_eq!(sample.quality, TranscodeQuality::Medium);
        }
    }

    #[test]
    fn diagnostic_retains_brief_window_resets_between_log_intervals() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        let _ = auto.observe(8500, 8000, true, Some(1_000_000), now);
        for second in 1..=10 {
            let rate = if second == 5 { None } else { Some(1_000_000) };
            let sample = auto.observe(8500, 8000, true, rate, now + Duration::from_secs(second));
            if second == 10 {
                let sample = sample.unwrap();
                assert_eq!(sample.reason, "healthy_window");
                assert_eq!(sample.healthy_ms, 4000);
                assert_eq!(sample.window_resets, 1);
                assert_eq!(sample.last_reset_reason, "missing_delivery_sample");
            }
        }
    }

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
    fn normal_eight_second_read_ahead_can_upgrade_across_healthy_refill_cycles() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        for second in 0..=30 {
            // Playback consumes some reserve before the next fixed range
            // replenishes it. Crossing below the fetch ceiling is normal.
            let reserve = if second % 5 == 0 { 6500 } else { 8500 };
            auto.observe(
                reserve,
                8000,
                true,
                Some(1_000_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
    }

    #[test]
    fn a_prepared_track_gets_the_normal_refill_grace_when_it_becomes_active() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.observe(2000, 2000, true, None, now + Duration::from_secs(120));
        auto.observe(
            2000,
            8000,
            true,
            Some(1_000_000),
            now + Duration::from_secs(121),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        // Genuine pressure after the normal grace still lowers the rendition.
        auto.observe(
            500,
            8000,
            true,
            Some(75_000),
            now + Duration::from_secs(126),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Low);
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
