//! Verified provider identity, stored independently of positional media records.
//!
//! The caller verifies release identity against local source context before
//! writing and supplies the same context fingerprint when reading. This cache
//! does not establish new artists or change the catalog's local media identity.

use redb::{ReadableTable, TableError};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::storage::ALBUM_SOURCE_IDENTITY_TABLE;
use crate::{Library, LibraryError};

pub const ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION: u32 = 1;

/// A MusicBrainz release verified against one album's local source context.
/// Artist names and MusicBrainz artist IDs are parallel, ordered billing lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumSourceIdentity {
    pub schema_version: u32,
    pub context_fingerprint: String,
    pub musicbrainz_release_id: String,
    pub musicbrainz_release_group_id: String,
    pub canonical_title: String,
    pub artist_names: Vec<String>,
    pub artist_ids: Vec<String>,
    pub verified_at_ms: u64,
    pub evidence: String,
}

impl AlbumSourceIdentity {
    fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION {
            return Err("unsupported schema version");
        }
        if self.context_fingerprint.trim().is_empty() {
            return Err("missing local context fingerprint");
        }
        if !is_musicbrainz_id(&self.musicbrainz_release_id) {
            return Err("invalid MusicBrainz release ID");
        }
        if !is_musicbrainz_id(&self.musicbrainz_release_group_id) {
            return Err("invalid MusicBrainz release-group ID");
        }
        if self.canonical_title.trim().is_empty() {
            return Err("missing canonical title");
        }
        if self.artist_names.is_empty()
            || self.artist_names.len() != self.artist_ids.len()
            || self.artist_names.iter().any(|name| name.trim().is_empty())
        {
            return Err("missing or unpaired artist names and IDs");
        }
        if self.artist_ids.iter().any(|id| !is_musicbrainz_id(id)) {
            return Err("invalid MusicBrainz artist ID");
        }
        if self.verified_at_ms == 0 {
            return Err("missing verification timestamp");
        }
        if self.evidence.trim().is_empty() {
            return Err("missing verification evidence");
        }
        Ok(())
    }
}

impl Library {
    /// Return a verified identity only while its schema and local source
    /// fingerprint still match. Missing or damaged optional cache entries are
    /// misses; they never prevent reading existing albums or rebuilding proof.
    pub fn get_album_source_identity(
        &self,
        album_id: &str,
        expected_fingerprint: &str,
    ) -> Result<Option<AlbumSourceIdentity>, LibraryError> {
        if album_id.trim().is_empty() || expected_fingerprint.trim().is_empty() {
            return Ok(None);
        }
        let read = self.db.begin_read()?;
        let table = match read.open_table(ALBUM_SOURCE_IDENTITY_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let Some(value) = table.get(album_id)? else {
            return Ok(None);
        };
        let identity: AlbumSourceIdentity = match serde_json::from_slice(value.value()) {
            Ok(identity) => identity,
            Err(_) => {
                warn!(
                    event = "metadata_source_identity_cache_invalid",
                    album_id,
                    reason = "malformed_json_record",
                    "Ignoring invalid optional album source identity cache"
                );
                return Ok(None);
            }
        };
        if identity.schema_version != ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION
            || identity.context_fingerprint != expected_fingerprint
        {
            return Ok(None);
        }
        if let Err(reason) = identity.validate() {
            warn!(
                event = "metadata_source_identity_cache_invalid",
                album_id, reason, "Ignoring invalid optional album source identity cache"
            );
            return Ok(None);
        }
        Ok(Some(identity))
    }

    /// Persist caller-verified release proof without modifying Album/Track
    /// records, enrichment counts, or the index version. Scans retain this
    /// sidecar; a changed source fingerprint invalidates reuse on the next read.
    pub fn set_album_source_identity(
        &self,
        album_id: &str,
        identity: &AlbumSourceIdentity,
    ) -> Result<(), LibraryError> {
        if album_id.trim().is_empty() {
            return Err(LibraryError::InvalidSourceIdentity(
                "missing album ID".into(),
            ));
        }
        identity
            .validate()
            .map_err(|reason| LibraryError::InvalidSourceIdentity(reason.into()))?;
        let bytes = serde_json::to_vec(identity)
            .map_err(|err| LibraryError::InvalidSourceIdentity(err.to_string()))?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(ALBUM_SOURCE_IDENTITY_TABLE)?;
            table.insert(album_id, bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }
}

// MusicBrainz IDs use the UUID's hyphenated wire format. Keep this small
// validation independent of provider HTTP code and reject nil placeholders.
fn is_musicbrainz_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && bytes
            .iter()
            .any(|byte| byte.is_ascii_hexdigit() && *byte != b'0')
}

#[cfg(test)]
#[path = "source_identity_tests.rs"]
mod tests;
