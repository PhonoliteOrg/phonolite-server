//! Provider capabilities and persisted, source-specific retry eligibility.
use std::time::Duration;

use library::{Library, MetadataAttempt};

use crate::external::{ExternalConfig, ExternalSource, Provider};

#[derive(Clone, Copy, Default)]
pub struct MissingFields {
    pub description: bool,
    pub genres: bool,
    pub artwork: bool,
}

impl MissingFields {
    fn supported_by(self, provider: Provider) -> bool {
        match provider {
            Provider::TheAudioDb => self.description || self.genres || self.artwork,
            Provider::MusicBrainz => self.genres,
        }
    }
}

pub struct Lookup<'a> {
    pub library: &'a Library,
    pub kind: &'a str,
    pub id: &'a str,
    pub fingerprint: &'a str,
    pub interval: Duration,
    pub replace: bool,
    attempts: parking_lot::Mutex<Vec<(MetadataAttempt, &'static str)>>,
}

impl<'a> Lookup<'a> {
    pub fn new(
        library: &'a Library,
        kind: &'a str,
        id: &'a str,
        fingerprint: &'a str,
        interval: Duration,
        replace: bool,
    ) -> Self {
        Self {
            library,
            kind,
            id,
            fingerprint,
            interval,
            replace,
            attempts: Default::default(),
        }
    }

    pub fn key(&self, source: &ExternalSource) -> String {
        // Credentials affect eligibility, but only their digest is persisted.
        let identity = format!(
            "{:?}\0{}\0{}",
            source.provider,
            source.api_key.as_deref().unwrap_or_default(),
            source.user_agent.as_deref().unwrap_or_default()
        );
        serde_json::to_string(&(
            self.kind,
            self.id,
            blake3::hash(identity.as_bytes()).to_hex().to_string(),
        ))
        .unwrap()
    }

    pub fn select(&self, config: &ExternalConfig, missing: MissingFields) -> ExternalConfig {
        ExternalConfig {
            sources: config
                .sources
                .iter()
                .filter(|source| {
                    let supported = self.replace || missing.supported_by(source.provider);
                    let (eligible, reason) = if !supported {
                        (false, "provider_does_not_support_missing_fields")
                    } else {
                        match self.library.metadata_refresh_due_with_interval(
                            &self.key(source),
                            self.fingerprint,
                            self.refresh(),
                            self.interval,
                        ) {
                            Ok(true) => (true, "eligible"),
                            Ok(false) => (false, "refresh_not_due"),
                            Err(err) => {
                                tracing::warn!(target: "metadata_external",
                                    event = "metadata_lookup_eligibility_failed",
                                    kind = self.kind, id = self.id, provider = ?source.provider,
                                    error = %err, "Cannot determine metadata lookup eligibility");
                                (false, "eligibility_error")
                            }
                        }
                    };
                    tracing::debug!(target: "metadata_external",
                        event = "metadata_lookup_eligibility",
                        kind = self.kind, id = self.id, provider = ?source.provider,
                        eligible, reason, replace = self.replace,
                        missing_description = missing.description,
                        missing_genres = missing.genres, missing_artwork = missing.artwork,
                        min_interval_secs = self.interval.as_secs(),
                        "Metadata provider eligibility checked");
                    eligible
                })
                .cloned()
                .collect(),
        }
    }

    pub fn claim(&self, source: &ExternalSource) -> Result<Option<MetadataAttempt>, String> {
        let result = self
            .library
            .claim_metadata_refresh(
                &self.key(source),
                self.fingerprint,
                self.interval,
                self.refresh(),
            )
            .map_err(|err| err.to_string());
        tracing::debug!(target: "metadata_external",
            event = "metadata_lookup_reservation",
            kind = self.kind, id = self.id, provider = ?source.provider,
            outcome = match &result {
                Ok(Some(_)) => "claimed",
                Ok(None) => "refresh_not_due_or_in_progress",
                Err(_) => "error",
            },
            "Metadata provider lookup reservation checked");
        result
    }

    fn refresh(&self) -> Option<u64> {
        self.replace.then(|| {
            crate::metadata_coordinator::ApplyContext::current()
                .and_then(|context| context.refresh)
                .unwrap_or(0)
        })
    }

    pub fn received(&self, attempt: MetadataAttempt, outcome: &'static str) {
        tracing::debug!(target: "metadata_external",
            event = "metadata_lookup_received", kind = self.kind, id = self.id,
            outcome, "Metadata provider result awaits persistence");
        self.attempts.lock().push((attempt, outcome));
    }

    /// Complete receipts only after the caller has handled/persisted the result.
    /// Cancellation drops reservations and keeps unfinished work retryable.
    pub fn finish(&self) {
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // Dropping these guards only queues optional cancellation receipts.
        // In particular, do not start a new database write during shutdown.
        if self.library.is_cancelled() {
            return;
        }
        for (attempt, outcome) in attempts {
            if let Err(err) = attempt.finish(outcome) {
                tracing::warn!(target: "metadata_external", "Metadata retry receipt failed: {}", err);
            } else {
                tracing::debug!(target: "metadata_external",
                    event = "metadata_lookup_finished", kind = self.kind, id = self.id,
                    outcome, "Metadata provider retry receipt persisted");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_and_biography_work_excludes_genre_only_provider() {
        let artwork = MissingFields {
            artwork: true,
            ..MissingFields::default()
        };
        assert!(artwork.supported_by(Provider::TheAudioDb));
        assert!(!artwork.supported_by(Provider::MusicBrainz));
        assert!(!MissingFields {
            description: true,
            ..MissingFields::default()
        }
        .supported_by(Provider::MusicBrainz));
        assert!(MissingFields {
            genres: true,
            ..MissingFields::default()
        }
        .supported_by(Provider::MusicBrainz));
    }

    #[test]
    fn provider_receipts_and_configuration_changes_control_eligibility() {
        let db = std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let library = Library::load_or_scan_with_db(Vec::new(), db).unwrap().0;
        let source = ExternalSource {
            provider: Provider::MusicBrainz,
            api_key: None,
            user_agent: Some("test-agent".into()),
            timeout: Duration::from_secs(1),
        };
        let config = ExternalConfig {
            sources: vec![source.clone()],
        };
        let fields = MissingFields {
            genres: true,
            ..MissingFields::default()
        };
        let lookup = Lookup::new(
            &library,
            "artist",
            "artist",
            "original",
            Duration::from_secs(3600),
            false,
        );
        assert_eq!(lookup.select(&config, fields).sources.len(), 1);
        lookup.received(
            lookup.claim(&source).unwrap().unwrap(),
            "no_usable_metadata",
        );
        lookup.finish();
        assert!(lookup.select(&config, fields).sources.is_empty());
        let changed = Lookup::new(
            &library,
            "artist",
            "artist",
            "changed",
            Duration::from_secs(3600),
            false,
        );
        assert_eq!(changed.select(&config, fields).sources.len(), 1);
        let mut changed_config = config;
        changed_config.sources[0].user_agent = Some("new-agent".into());
        assert_eq!(lookup.select(&changed_config, fields).sources.len(), 1);
        assert!(!lookup.key(&source).contains("test-agent"));
    }
}
