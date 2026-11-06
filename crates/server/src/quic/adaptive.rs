//! Auto chooses a fixed rendition for each independently encoded time range.
use std::time::{Duration, Instant};

use crate::transcode::{quality_bitrate, TranscodeQuality};

pub(super) struct AdaptiveQuality {
    quality: TranscodeQuality,
    last_change: Instant,
    high_since: Option<Instant>,
    fast_since: Option<Instant>,
    pressure_since: Option<(Instant, u32)>,
    established: bool,
    active_reported: bool,
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
            fast_since: None,
            pressure_since: None,
            established: false,
            active_reported: false,
            last_observed: None,
            last_diagnostic: None,
            window_resets: 0,
            last_reset_reason: "none",
        }
    }

    pub fn quality(&self) -> TranscodeQuality {
        self.quality
    }

    pub fn resume(&mut self, quality: TranscodeQuality) {
        self.quality = quality;
    }

    pub fn resume_first_active(&mut self, quality: TranscodeQuality, target_ms: u32) {
        if target_ms >= 8000 && !self.active_reported {
            self.quality = quality;
            self.active_reported = true;
        }
    }

    pub fn remembered_quality(&self, target_ms: u32) -> Option<TranscodeQuality> {
        (self.established && target_ms >= 8000).then_some(self.quality)
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
            self.fast_since = None;
            self.pressure_since = None;
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
            upgrade_bps: higher.map(|quality| u64::from(quality_bitrate(quality)) * 5 / 4),
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
            self.fast_since = None;
            self.pressure_since = None;
            // Its intentionally small reserve needs the same refill grace as
            // a cold start when playback becomes active again.
            self.last_change = now;
            return "inactive_horizon";
        }
        self.active_reported = true;
        self.established |= buffered_ms >= 3000;
        let delivery_bps = delivery_bps.filter(|_| delivery_metrics);
        let insufficient_delivery =
            delivery_bps.is_some_and(|rate| rate < u64::from(quality_bitrate(self.quality)));
        let sustained_pressure = if insufficient_delivery {
            let (since, initial_reserve) = *self.pressure_since.get_or_insert((now, buffered_ms));
            now.saturating_duration_since(since) >= Duration::from_secs(4)
                && buffered_ms.saturating_add(500) <= initial_reserve
        } else {
            self.pressure_since = None;
            false
        };
        if buffered_ms < 3000 || insufficient_delivery {
            self.high_since = None;
            self.fast_since = None;
            if (buffered_ms < 3000 || sustained_pressure)
                && now.duration_since(self.last_change) >= Duration::from_secs(5)
            {
                let lower = match self.quality {
                    TranscodeQuality::High => TranscodeQuality::Medium,
                    _ => TranscodeQuality::Low,
                };
                self.quality = match delivery_bps {
                    // A severe drop can skip Medium rather than spend another
                    // range exhausting the reserve at an unsustainable rate.
                    Some(rate) if rate < u64::from(quality_bitrate(TranscodeQuality::Medium)) => {
                        TranscodeQuality::Low
                    }
                    _ => lower,
                };
                self.last_change = now;
                self.established = true;
                self.pressure_since = None;
            }
            return if buffered_ms < 3000 {
                "low_reserve"
            } else {
                "delivery_pressure"
            };
        }

        // Playback must fall below the fetch ceiling to request more audio.
        // Leave a real refill margin instead of requiring an eight-second
        // reserve to remain continuously full against an eight-second target.
        let upgrade_buffer = target_ms.saturating_sub(3000).clamp(5000, 10000);
        if buffered_ms < upgrade_buffer {
            self.high_since = None;
            self.fast_since = None;
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
            && !delivery_bps.is_some_and(|rate| rate >= u64::from(quality_bitrate(higher)) * 5 / 4)
        {
            self.high_since = None;
            self.fast_since = None;
            return if delivery_bps.is_none() {
                "missing_delivery_sample"
            } else {
                "upgrade_headroom"
            };
        }
        let since = *self.high_since.get_or_insert(now);
        let fast = delivery_metrics
            && delivery_bps.is_some_and(|rate| rate >= u64::from(quality_bitrate(higher)) * 2);
        if fast {
            self.fast_since.get_or_insert(now);
        } else {
            self.fast_since = None;
        }
        let fast_ready = self
            .fast_since
            .is_some_and(|since| now.duration_since(since) >= Duration::from_secs(10));
        let healthy_window = Duration::from_secs(if delivery_metrics { 20 } else { 30 });
        if now.duration_since(since) >= healthy_window || fast_ready {
            self.quality = higher;
            self.last_change = now;
            self.high_since = Some(now);
            self.fast_since = None;
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
        assert_eq!(initial.upgrade_bps, Some(200_000));
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
    fn delivery_shortfall_requires_sustained_pressure_and_a_draining_reserve() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.resume(TranscodeQuality::High);
        for second in 0..=10 {
            // Content delivery and backpressure can report less than nominal
            // without starving playback. A stable reserve must keep HQ.
            auto.observe(
                16000,
                20000,
                true,
                Some(150_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
        auto.observe(
            15000,
            20000,
            true,
            Some(150_000),
            now + Duration::from_secs(11),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        // A real capacity collapse must lower quality before a large reserve empties.
        for second in 12..=16 {
            auto.observe(
                15000 - (second as u32 - 12) * 250,
                20000,
                true,
                Some(70_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::Low);
    }

    #[test]
    fn hq_at_modest_headroom_and_a_single_poor_sample_do_not_downgrade() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.resume(TranscodeQuality::High);
        for second in 0..=60 {
            let rate = if second == 20 { 60_000 } else { 180_000 };
            auto.observe(
                7000,
                8000,
                true,
                Some(rate),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
        // Buffer pressure remains an urgent override, even without a rate sample.
        auto.observe(2000, 8000, true, None, now + Duration::from_secs(61));
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
    }

    #[test]
    fn missing_samples_report_gaps_and_pauses_cannot_extend_delivery_pressure() {
        let now = Instant::now();
        for (target, rate, gap) in [
            (8000, None, 1),
            (2000, Some(10_000), 1),
            (8000, Some(10_000), 10),
        ] {
            let mut auto = AdaptiveQuality::new(now);
            auto.resume(TranscodeQuality::High);
            auto.observe(8000, 8000, true, Some(10_000), now + Duration::from_secs(5));
            auto.observe(7000, target, true, rate, now + Duration::from_secs(5 + gap));
            auto.observe(
                6000,
                8000,
                true,
                Some(10_000),
                now + Duration::from_secs(6 + gap),
            );
            assert_eq!(auto.quality(), TranscodeQuality::High);
        }
    }

    #[test]
    fn fresh_headroom_upgrades_in_twenty_seconds_or_ten_with_double_capacity() {
        let now = Instant::now();
        for (rate, window) in [(210_000, 20), (400_000, 10)] {
            let mut auto = AdaptiveQuality::new(now);
            for second in 0..window {
                auto.observe(
                    6500,
                    8000,
                    true,
                    Some(rate),
                    now + Duration::from_secs(second),
                );
            }
            assert_eq!(auto.quality(), TranscodeQuality::Medium);
            auto.observe(
                6500,
                8000,
                true,
                Some(rate),
                now + Duration::from_secs(window),
            );
            assert_eq!(auto.quality(), TranscodeQuality::High);
        }
    }

    #[test]
    fn one_fast_burst_does_not_shorten_the_normal_upgrade_window() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        for second in 0..=15 {
            auto.observe(
                6500,
                8000,
                true,
                Some(210_000),
                now + Duration::from_secs(second),
            );
        }
        auto.observe(
            6500,
            8000,
            true,
            Some(1_000_000),
            now + Duration::from_secs(16),
        );
        assert_eq!(auto.quality(), TranscodeQuality::Medium);
        for second in 17..=20 {
            auto.observe(
                6500,
                8000,
                true,
                Some(210_000),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(auto.quality(), TranscodeQuality::High);
    }

    #[test]
    fn cold_or_prepared_reserves_do_not_overwrite_recent_audible_quality() {
        let now = Instant::now();
        let mut auto = AdaptiveQuality::new(now);
        auto.resume(TranscodeQuality::High);
        auto.observe(0, 8000, true, None, now);
        assert!(auto.remembered_quality(8000).is_none());
        auto.observe(5000, 5000, true, Some(10_000), now + Duration::from_secs(1));
        assert!(auto.remembered_quality(5000).is_none());
        // Refresh at handoff if the preceding track changed quality after preparation.
        auto.resume_first_active(TranscodeQuality::Low, 8000);
        assert_eq!(auto.quality(), TranscodeQuality::High); // already reported active
        let mut prepared = AdaptiveQuality::new(now);
        prepared.observe(2000, 2000, true, None, now);
        prepared.resume_first_active(TranscodeQuality::High, 8000);
        assert_eq!(prepared.quality(), TranscodeQuality::High);
        prepared.observe(
            6000,
            8000,
            true,
            Some(200_000),
            now + Duration::from_secs(2),
        );
        assert_eq!(
            prepared.remembered_quality(8000),
            Some(TranscodeQuality::High)
        );
        assert!(prepared.remembered_quality(2000).is_none());
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
