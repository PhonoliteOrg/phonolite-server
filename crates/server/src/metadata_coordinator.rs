//! One queue owns metadata and cover work for this server. Requests are merged
//! before execution; scans cancel old futures before publishing new sources.
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

const MAX_QUEUED_IDS: usize = 256;
const INTERACTIVE_BATCH_IDS: usize = 16;
const INTERACTIVE_BURST: usize = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    #[default]
    None,
    Ids(BTreeSet<String>),
    All,
}

impl Selection {
    pub fn ids(ids: impl IntoIterator<Item = String>) -> Self {
        let ids: BTreeSet<_> = ids.into_iter().collect();
        if ids.is_empty() {
            Self::None
        } else {
            Self::Ids(ids)
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        match self {
            Self::None => false,
            Self::Ids(ids) => ids.contains(id),
            Self::All => true,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn total(&self, catalog_total: usize) -> usize {
        match self {
            Self::None => 0,
            Self::Ids(ids) => ids.len(),
            Self::All => catalog_total,
        }
    }

    fn merge(&mut self, other: Self) {
        match (&mut *self, other) {
            (_, Self::None) | (Self::All, _) => {}
            (current, Self::All) => *current = Self::All,
            (Self::Ids(current), Self::Ids(other)) => current.extend(other),
            (current, other) => *current = other,
        }
    }

    // Only explicit targets can be absorbed here. A catalog sweep has a budget
    // and may not reach a requested artist; leave that request queued.
    fn subtract_targets(&mut self, active: &Self) {
        if matches!((&*self, active), (Self::All, Self::All)) {
            *self = Self::None;
            return;
        }
        if let (Self::Ids(pending), Self::Ids(active)) = (&mut *self, active) {
            pending.retain(|id| !active.contains(id));
            if pending.is_empty() {
                *self = Self::None;
            }
        }
    }

    fn bound_ids(&mut self, remaining: &mut usize) {
        if let Self::Ids(ids) = self {
            let keep = (*remaining).min(ids.len());
            *remaining -= keep;
            let mut index = 0;
            ids.retain(|_| {
                index += 1;
                index <= keep
            });
            if ids.is_empty() {
                *self = Self::None;
            }
        }
    }

    fn take_targets(&mut self, remaining: &mut usize) -> Self {
        match self {
            Self::Ids(ids) => {
                let count = ids.len().min(*remaining);
                let selected: BTreeSet<_> = ids.iter().take(count).cloned().collect();
                ids.retain(|id| !selected.contains(id));
                *remaining -= count;
                if ids.is_empty() {
                    *self = Self::None;
                }
                if selected.is_empty() {
                    Self::None
                } else {
                    Self::Ids(selected)
                }
            }
            Self::All if *remaining > 0 => std::mem::take(self),
            _ => Self::None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub artists: Selection,
    pub credits: Selection,
    pub albums: Selection,
    pub covers: Selection,
    pub replace: bool,
    pub interactive: bool,
    pub refresh: Option<u64>,
}

impl Request {
    fn log_provider_work(&self, event: &'static str) {
        if self.artists.is_empty() && self.credits.is_empty() && self.albums.is_empty() {
            return;
        }
        tracing::debug!(target: "metadata_external",
            event, interactive = self.interactive, replace = self.replace,
            artists_all = matches!(&self.artists, Selection::All),
            artist_ids = self.artists.total(0),
            credits_all = matches!(&self.credits, Selection::All),
            credit_ids = self.credits.total(0),
            albums_all = matches!(&self.albums, Selection::All),
            album_ids = self.albums.total(0),
            "External metadata queue lifecycle");
    }

    pub fn sweep(replace: bool) -> Self {
        Self {
            artists: Selection::All,
            credits: Selection::All,
            albums: Selection::All,
            replace,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.artists.is_empty()
            && self.credits.is_empty()
            && self.albums.is_empty()
            && self.covers.is_empty()
    }

    fn merge(&mut self, other: Self) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            *self = other;
            return;
        }
        self.artists.merge(other.artists);
        self.credits.merge(other.credits);
        self.albums.merge(other.albums);
        self.covers.merge(other.covers);
        self.replace |= other.replace;
        self.interactive |= other.interactive;
        self.refresh = other.refresh.or(self.refresh);
    }

    fn remaining(mut self, completed: &Self, retain_all: bool) -> Self {
        for (pending, done) in [
            (&mut self.artists, &completed.artists),
            (&mut self.credits, &completed.credits),
            (&mut self.albums, &completed.albums),
            (&mut self.covers, &completed.covers),
        ] {
            if matches!(pending, Selection::All) && !retain_all {
                *pending = Selection::None;
            }
            pending.subtract_targets(done);
        }
        self
    }

    fn bound_ids(&mut self, remaining: &mut usize) {
        for selection in [
            &mut self.artists,
            &mut self.credits,
            &mut self.albums,
            &mut self.covers,
        ] {
            selection.bound_ids(remaining);
        }
    }

    fn take_batch(&mut self) -> Self {
        let mut remaining = INTERACTIVE_BATCH_IDS;
        Self {
            artists: self.artists.take_targets(&mut remaining),
            credits: self.credits.take_targets(&mut remaining),
            albums: self.albums.take_targets(&mut remaining),
            covers: self.covers.take_targets(&mut remaining),
            replace: self.replace,
            interactive: self.interactive,
            refresh: self.refresh,
        }
    }
}

#[derive(Default)]
struct Queue {
    pending: Request,
    priority: Request,
    deferred: Request,
    active: Option<(Request, CancellationToken, Arc<Mutex<Request>>)>,
    running: bool,
    pauses: usize,
    interactive_burst: usize,
    background_protected: bool,
}

impl Queue {
    fn bound_targets(&mut self) {
        let mut remaining = MAX_QUEUED_IDS;
        self.priority.bound_ids(&mut remaining);
        self.pending.bound_ids(&mut remaining);
        self.deferred.bound_ids(&mut remaining);
    }
}

#[derive(Default)]
pub struct MetadataCoordinator {
    queue: Mutex<Queue>,
    billed: Mutex<Option<(u64, Arc<HashSet<String>>)>>,
    apply: Arc<Mutex<()>>,
}

tokio::task_local! { pub static APPLY_CONTEXT: ApplyContext; }

#[derive(Clone)]
pub struct ApplyContext {
    gate: Arc<Mutex<()>>,
    cancellation: CancellationToken,
    pub refresh: Option<u64>,
    completed: Arc<Mutex<Request>>,
    tracked: u8,
}

impl ApplyContext {
    pub fn current() -> Option<Self> {
        APPLY_CONTEXT.try_with(Clone::clone).ok()
    }

    pub fn guard(&self) -> Result<parking_lot::MutexGuard<'_, ()>, library::LibraryError> {
        let guard = self.gate.lock();
        if self.cancellation.is_cancelled() {
            return Err(library::LibraryError::Cancelled);
        }
        Ok(guard)
    }

    pub fn apply<T>(
        &self,
        work: impl FnOnce() -> Result<T, library::LibraryError>,
    ) -> Result<T, library::LibraryError> {
        let _guard = self.guard()?;
        work()
    }
}

pub fn apply_metadata<T>(
    work: impl FnOnce() -> Result<T, library::LibraryError>,
) -> Result<T, library::LibraryError> {
    match ApplyContext::current() {
        Some(context) => context.apply(work),
        None => work(),
    }
}

pub struct WorkItem {
    context: Option<ApplyContext>,
    kind: &'static str,
    id: String,
}

pub fn work_item(kind: &'static str, id: &str) -> WorkItem {
    WorkItem {
        context: ApplyContext::current(),
        kind,
        id: id.into(),
    }
}

impl Drop for WorkItem {
    fn drop(&mut self) {
        if let Some(context) = &self.context {
            if context.cancellation.is_cancelled() {
                return;
            }
            let bit = match self.kind {
                "artists" => 1,
                "credits" => 2,
                "albums" => 4,
                "covers" => 8,
                _ => 0,
            };
            if context.tracked & bit == 0 {
                return;
            }
            let mut completed = context.completed.lock();
            let selection = match self.kind {
                "artists" => &mut completed.artists,
                "credits" => &mut completed.credits,
                "albums" => &mut completed.albums,
                "covers" => &mut completed.covers,
                _ => return,
            };
            selection.merge(Selection::ids([self.id.clone()]));
        }
    }
}

impl MetadataCoordinator {
    pub fn context(&self, cancellation: CancellationToken, refresh: Option<u64>) -> ApplyContext {
        let queue = self.queue.lock();
        let completed = queue
            .active
            .as_ref()
            .map(|active| active.2.clone())
            .unwrap_or_default();
        let tracked = queue.active.as_ref().map_or(0, |active| {
            [
                &active.0.artists,
                &active.0.credits,
                &active.0.albums,
                &active.0.covers,
            ]
            .iter()
            .enumerate()
            .fold(0, |mask, (index, selection)| {
                mask | if matches!(selection, Selection::Ids(_)) {
                    1 << index
                } else {
                    0
                }
            })
        });
        ApplyContext {
            gate: self.apply.clone(),
            cancellation,
            refresh,
            completed,
            tracked,
        }
    }
    pub fn billed_artists(
        &self,
        library: &library::Library,
    ) -> Result<Arc<HashSet<String>>, library::LibraryError> {
        let mut cached = self.billed.lock();
        let generation = library.catalog_generation();
        if let Some((saved, ids)) = &*cached {
            if *saved == generation {
                return Ok(ids.clone());
            }
        }
        let ids = Arc::new(library.billed_artist_ids()?);
        *cached = Some((generation, ids.clone()));
        Ok(ids)
    }
    /// True grants the caller ownership of the sole drain loop.
    pub fn submit(&self, mut request: Request) -> bool {
        request.log_provider_work("metadata_queue_submit");
        if request.replace && request.refresh.is_none() {
            static NEXT_REFRESH: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(1);
            request.refresh = Some(NEXT_REFRESH.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        }
        let mut queue = self.queue.lock();
        if let Some((active, cancellation, _)) = &queue.active {
            if !request.replace
                && !cancellation.is_cancelled()
                && (!request.interactive || active.interactive)
            {
                request.artists.subtract_targets(&active.artists);
                request.credits.subtract_targets(&active.credits);
                request.albums.subtract_targets(&active.albums);
                request.covers.subtract_targets(&active.covers);
                if *active == request {
                    request = Request::default();
                }
            }
        }
        if !request.artists.is_empty() || !request.credits.is_empty() || !request.albums.is_empty()
        {
            let deferred = std::mem::take(&mut queue.deferred);
            if deferred.interactive {
                queue.priority.merge(deferred);
            } else {
                queue.pending.merge(deferred);
            }
        }
        if request.interactive {
            queue.priority.merge(request);
        } else {
            queue.pending.merge(request);
        }
        queue.bound_targets();
        // Interactive targets may interrupt a background provider await. Its
        // unfinished source selection is retained, while completed commits and
        // provider receipts keep their existing cancellation semantics.
        if !queue.priority.is_empty()
            && queue.interactive_burst < INTERACTIVE_BURST
            && !queue.background_protected
        {
            if let Some((active, cancellation, completed)) = queue.active.clone() {
                if !active.interactive && !cancellation.is_cancelled() {
                    active.log_provider_work("metadata_queue_preempted");
                    cancellation.cancel();
                    queue
                        .pending
                        .merge(active.remaining(&completed.lock(), true));
                    queue.bound_targets();
                }
            }
        }
        if queue.running
            || queue.pauses > 0
            || (queue.pending.is_empty() && queue.priority.is_empty())
        {
            return false;
        }
        queue.running = true;
        true
    }

    pub fn next(&self) -> Option<(Request, CancellationToken)> {
        let mut queue = self.queue.lock();
        if queue.pauses > 0 || (queue.pending.is_empty() && queue.priority.is_empty()) {
            queue.running = false;
            return None;
        }
        let request = if queue.priority.is_empty()
            || (!queue.pending.is_empty() && queue.interactive_burst >= INTERACTIVE_BURST)
        {
            // A background turn after a foreground burst must complete its
            // configured phase budget, rather than being immediately cancelled
            // by the next foreground arrival and restarting at the first ID.
            queue.background_protected = !queue.priority.is_empty();
            queue.interactive_burst = 0;
            std::mem::take(&mut queue.pending)
        } else {
            queue.background_protected = false;
            queue.interactive_burst += 1;
            queue.priority.take_batch()
        };
        let token = CancellationToken::new();
        request.log_provider_work("metadata_queue_started");
        queue.active = Some((request.clone(), token.clone(), Default::default()));
        Some((request, token))
    }

    pub fn complete(&self) {
        let mut queue = self.queue.lock();
        if let Some((active, cancellation, completed)) = queue.active.take() {
            active.log_provider_work(if cancellation.is_cancelled() {
                "metadata_queue_cancelled"
            } else {
                "metadata_queue_completed"
            });
            if !cancellation.is_cancelled() {
                queue
                    .deferred
                    .merge(active.remaining(&completed.lock(), false));
                queue.bound_targets();
            }
        }
    }

    /// Preserve unfinished discovery on incremental scans. A full reset makes
    /// previous requests obsolete and will submit a fresh replacement sweep.
    pub fn pause(&self, reset: bool) {
        tracing::debug!(target: "metadata_external", event = "metadata_queue_paused", reset,
            "External metadata queue paused for local discovery");
        let _apply = self.apply.lock();
        let mut queue = self.queue.lock();
        queue.pauses += 1;
        if reset {
            queue.pending = Request::default();
            queue.priority = Request::default();
            queue.deferred = Request::default();
        }
        if let Some((active, token, completed)) = queue.active.clone() {
            token.cancel();
            if !reset {
                let active = active.remaining(&completed.lock(), true);
                if active.interactive {
                    queue.priority.merge(active);
                } else {
                    queue.pending.merge(active);
                }
            }
        }
        queue.bound_targets();
    }

    pub fn resume(&self) -> bool {
        let mut queue = self.queue.lock();
        queue.pauses = queue.pauses.saturating_sub(1);
        tracing::debug!(target: "metadata_external", event = "metadata_queue_resumed",
            remaining_pauses = queue.pauses, "External metadata queue resume checked");
        if queue.running
            || queue.pauses > 0
            || (queue.pending.is_empty() && queue.priority.is_empty())
        {
            return false;
        }
        queue.running = true;
        true
    }

    /// Provider changes invalidate captured credentials and results, but keep
    /// unfinished discovery available under the new configuration.
    pub fn reconfigure(&self, update: impl FnOnce()) {
        tracing::debug!(target: "metadata_external", event = "metadata_queue_reconfigured",
            "External metadata provider configuration changed");
        let _apply = self.apply.lock();
        let mut queue = self.queue.lock();
        if let Some((active, token, completed)) = queue.active.clone() {
            token.cancel();
            let active = active.remaining(&completed.lock(), true);
            if active.interactive {
                queue.priority.merge(active);
            } else {
                queue.pending.merge(active);
            }
        }
        queue.bound_targets();
        update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist(id: &str) -> Request {
        Request {
            artists: Selection::ids([id.into()]),
            ..Request::default()
        }
    }

    #[test]
    fn targeted_queue_is_bounded_and_interactive_batches_yield_to_background() {
        let queue = MetadataCoordinator::default();
        queue.submit(Request::sweep(false));
        queue.submit(Request {
            artists: Selection::ids((0..1000).map(|id| format!("artist-{id:04}"))),
            interactive: true,
            ..Default::default()
        });
        {
            let state = queue.queue.lock();
            assert_eq!(state.priority.artists.total(0), MAX_QUEUED_IDS);
        }
        for _ in 0..INTERACTIVE_BURST {
            let (request, _) = queue.next().unwrap();
            assert!(request.interactive);
            assert_eq!(request.artists.total(0), INTERACTIVE_BATCH_IDS);
            queue.complete();
        }
        assert_eq!(queue.next().unwrap().0, Request::sweep(false));
    }

    #[test]
    fn interactive_work_preempts_and_retains_an_active_background_sweep() {
        let queue = MetadataCoordinator::default();
        queue.submit(Request::sweep(false));
        let (_, cancellation) = queue.next().unwrap();
        let mut foreground = artist("visible");
        foreground.interactive = true;
        queue.submit(foreground.clone());
        assert!(cancellation.is_cancelled());
        queue.complete();
        assert_eq!(queue.next().unwrap().0, foreground);
        queue.complete();
        assert_eq!(queue.next().unwrap().0, Request::sweep(false));
    }

    #[test]
    fn repeated_requests_have_one_worker_and_one_target() {
        let queue = MetadataCoordinator::default();
        assert!(queue.submit(artist("a")));
        assert!(!queue.submit(artist("a")));
        let (request, _) = queue.next().unwrap();
        assert_eq!(request.artists.total(100), 1);
        assert!(!queue.submit(artist("a")));
        assert!(!queue.submit(artist("b")));
        queue.complete();
        assert_eq!(queue.next().unwrap().0, artist("b"));
        queue.complete();
        assert!(queue.next().is_none());
        assert!(queue.submit(artist("c")));
    }

    #[test]
    fn scan_cancels_old_fetch_and_keeps_new_changes() {
        let queue = MetadataCoordinator::default();
        queue.submit(artist("a"));
        let (_, token) = queue.next().unwrap();
        queue.pause(false);
        assert!(token.is_cancelled());
        queue.submit(artist("b"));
        queue.complete();
        assert!(queue.next().is_none());
        assert!(queue.resume());
        let request = queue.next().unwrap().0;
        assert!(request.artists.contains("a"));
        assert!(request.artists.contains("b"));
    }

    #[test]
    fn replacement_clears_old_work_and_preserves_force() {
        let queue = MetadataCoordinator::default();
        queue.submit(artist("obsolete"));
        let (_, token) = queue.next().unwrap();
        queue.pause(true);
        queue.submit(Request::sweep(true));
        queue.complete();
        assert!(queue.next().is_none());
        queue.resume();
        assert!(token.is_cancelled());
        let request = queue.next().unwrap().0;
        assert!(request.replace);
        assert!(request.refresh.is_some());
        assert_eq!(request.artists, Selection::All);
    }

    #[test]
    fn cancelled_context_cannot_commit_after_scan_or_configuration_change() {
        let queue = MetadataCoordinator::default();
        queue.submit(artist("a"));
        let (_, token) = queue.next().unwrap();
        let context = queue.context(token, None);
        assert!(context.apply(|| Ok(())).is_ok());
        queue.reconfigure(|| {});
        let mut wrote = false;
        assert!(context
            .apply(|| {
                wrote = true;
                Ok(())
            })
            .is_err());
        assert!(!wrote);
    }

    #[test]
    fn interactive_requests_keep_priority_without_expanding_scan_budget() {
        let queue = MetadataCoordinator::default();
        queue.submit(Request::sweep(false));
        let mut request = artist("requested");
        request.interactive = true;
        queue.submit(request.clone());
        assert_eq!(queue.next().unwrap().0, request);
        queue.complete();
        assert_eq!(queue.next().unwrap().0, Request::sweep(false));
    }

    #[test]
    fn fresh_requests_are_not_absorbed_by_cancelled_catalog_work() {
        let queue = MetadataCoordinator::default();
        queue.submit(Request::sweep(false));
        queue.next().unwrap();
        queue.pause(true);
        queue.submit(Request::sweep(false));
        queue.complete();
        queue.next();
        queue.resume();
        assert_eq!(queue.next().unwrap().0, Request::sweep(false));
    }

    #[tokio::test]
    async fn bounded_targeted_work_is_retained_for_the_next_trigger() {
        let queue = MetadataCoordinator::default();
        queue.submit(Request {
            artists: Selection::ids(["a".into(), "b".into()]),
            ..Request::default()
        });
        let (_, token) = queue.next().unwrap();
        APPLY_CONTEXT
            .scope(queue.context(token, None), async {
                drop(work_item("artists", "a"));
            })
            .await;
        queue.complete();
        assert!(
            queue.next().is_none(),
            "a phase budget must not immediately start another phase budget"
        );
        assert!(queue.submit(artist("c")));
        let next = queue.next().unwrap().0;
        assert!(!next.artists.contains("a"));
        assert!(next.artists.contains("b"));
        assert!(next.artists.contains("c"));
        assert!(!next.replace);
    }
}
