use std::time::{Duration, Instant};

use parking_lot::Mutex;

const MUSICBRAINZ_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

static NEXT_REQUEST_AT: Mutex<Option<Instant>> = Mutex::new(None);

pub async fn wait_for_slot() {
    let delay = {
        let mut guard = NEXT_REQUEST_AT.lock();
        let now = Instant::now();
        let reserved = guard.unwrap_or(now).max(now);
        *guard = Some(reserved + MUSICBRAINZ_REQUEST_INTERVAL);
        reserved.saturating_duration_since(now)
    };

    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}
