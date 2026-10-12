//! Read the original positional download snapshots without changing their schema.
//! New writes use a tagged JSON record so additive credit fields survive upgrades.
use serde::{Deserialize, Serialize};

use super::{
    DownloadJob, DownloadJobEvent, DownloadJobItemStatus, DownloadJobScope, DownloadJobStatus,
};

#[derive(Serialize, Deserialize)]
struct LegacyArtist {
    id: String,
    name: String,
    genres: Vec<String>,
    album_count: usize,
    summary: Option<String>,
    logo_ref: Option<String>,
    banner_ref: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LegacyAlbum {
    id: String,
    artist_id: String,
    artist_ids: Vec<String>,
    artist_name: String,
    artist_names: Vec<String>,
    title: String,
    year: Option<i32>,
    genres: Vec<String>,
    track_count: usize,
    summary: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LegacyTrack {
    id: String,
    title: String,
    artist: String,
    album: String,
    artist_id: String,
    album_id: String,
    duration_ms: u32,
    track_no: Option<u16>,
    disc_no: Option<u16>,
    genres: Vec<String>,
    liked: bool,
    in_playlists: bool,
}

#[derive(Serialize, Deserialize)]
struct LegacyOfflineMetadata {
    schema_version: u32,
    track: LegacyTrack,
    album: LegacyAlbum,
    artist: LegacyArtist,
}

#[derive(Serialize, Deserialize)]
struct LegacyItem {
    position: usize,
    track_id: String,
    status: DownloadJobItemStatus,
    download_url: Option<String>,
    offline_metadata: Option<LegacyOfflineMetadata>,
    byte_length: Option<u64>,
    content_type: Option<String>,
    etag: Option<String>,
    sha256: Option<String>,
    error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LegacyJob {
    job_id: String,
    user_id: String,
    client_id: String,
    client_request_id: String,
    request_key: String,
    scope_key: String,
    scope: DownloadJobScope,
    status: DownloadJobStatus,
    created_at: u64,
    updated_at: u64,
    total_count: usize,
    ready_count: usize,
    completed_count: usize,
    failed_count: usize,
    event_cursor: u64,
    items: Vec<LegacyItem>,
}

#[derive(Serialize, Deserialize)]
struct LegacyEvent {
    cursor: u64,
    job_id: String,
    kind: String,
    message: String,
    created_at: u64,
    job: Option<LegacyJob>,
}

fn upgrade<T: Serialize, U: for<'de> Deserialize<'de>>(old: T) -> Result<U, String> {
    let value = serde_json::to_value(old).map_err(|err| err.to_string())?;
    serde_json::from_value(value).map_err(|err| err.to_string())
}

pub(super) fn decode_job(bytes: &[u8]) -> Result<DownloadJob, String> {
    let old: LegacyJob = bincode::deserialize(bytes).map_err(|err| err.to_string())?;
    upgrade(old)
}

pub(super) fn decode_event(bytes: &[u8]) -> Result<DownloadJobEvent, String> {
    let old: LegacyEvent = bincode::deserialize(bytes).map_err(|err| err.to_string())?;
    upgrade(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_job() -> LegacyJob {
        serde_json::from_value(serde_json::json!({
            "job_id":"job-1", "user_id":"user-1", "client_id":"client-1",
            "client_request_id":"request-1", "request_key":"request-key", "scope_key":"album:album-1",
            "scope":{"kind":"album","id":"album-1","track_ids":[]}, "status":"paused",
            "created_at":1,"updated_at":2,"total_count":1,"ready_count":1,"completed_count":0,"failed_count":0,"event_cursor":7,
            "items":[{"position":0,"track_id":"song-1","status":"paused","download_url":"/file/song-1",
                "offline_metadata":{"schema_version":4,
                    "track":{"id":"song-1","title":"Paper Moon","artist":"Mira Vale","album":"Riverlight","artist_id":"mira","album_id":"album-1","duration_ms":1000,"track_no":1,"disc_no":null,"genres":[],"liked":true,"in_playlists":true},
                    "album":{"id":"album-1","artist_id":"mira","artist_ids":["mira"],"artist_name":"Mira Vale","artist_names":["Mira Vale"],"title":"Riverlight","year":2025,"genres":[],"track_count":1,"summary":null},
                    "artist":{"id":"mira","name":"Mira Vale","genres":[],"album_count":1,"summary":null,"logo_ref":null,"banner_ref":null}},
                "byte_length":123,"content_type":"audio/flac","etag":"etag-1","sha256":"checksum","error":null}]
        })).unwrap()
    }

    #[test]
    fn old_binary_download_job_retains_metadata_and_can_store_new_credits() {
        let binary = bincode::serialize(&old_job()).unwrap();
        let mut job: DownloadJob = super::super::decode_value(&binary).unwrap();
        assert_eq!(job.status, DownloadJobStatus::Paused);
        assert_eq!(job.event_cursor, 7);
        let metadata = job.items[0].offline_metadata.as_mut().unwrap();
        assert!(metadata.track.liked);
        assert!(metadata.track.in_playlists);
        assert_eq!(metadata.track.id, "song-1");
        metadata.track.artist_credits.push(common::ArtistCredit {
            artist_id: "theo".into(),
            name: "Theo Ellis".into(),
            credited_name: "Theo Ellis".into(),
            role: "featured_artist".into(),
            join_phrase: String::new(),
            source: "tags".into(),
        });
        let updated = super::super::encode_value(&job).unwrap();
        assert!(updated.starts_with(super::super::RECORD_JSON_PREFIX));
        let restored: DownloadJob = super::super::decode_value(&updated).unwrap();
        assert_eq!(
            restored.items[0]
                .offline_metadata
                .as_ref()
                .unwrap()
                .track
                .artist_credits[0]
                .artist_id,
            "theo"
        );
        assert_eq!(restored.items[0].sha256.as_deref(), Some("checksum"));
    }

    #[test]
    fn old_event_with_embedded_job_is_readable() {
        let event = LegacyEvent {
            cursor: 7,
            job_id: "job-1".into(),
            kind: "paused".into(),
            message: "Paused".into(),
            created_at: 2,
            job: Some(old_job()),
        };
        let bytes = bincode::serialize(&event).unwrap();
        let restored: DownloadJobEvent = super::super::decode_value(&bytes).unwrap();
        assert_eq!(restored.cursor, 7);
        assert_eq!(
            restored.job.as_ref().unwrap().items[0]
                .offline_metadata
                .as_ref()
                .unwrap()
                .album
                .title,
            "Riverlight"
        );
    }
}
