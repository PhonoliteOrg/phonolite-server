//! Versioned metadata contract shared by downloads and persisted job snapshots.
//! This module has no HTTP or application-state dependency.
use crate::library_views::{
    build_album_view, build_artist_views, build_track_views, AlbumView, ArtistView, TrackView,
};
use common::Track;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
pub(crate) const OFFLINE_METADATA_SCHEMA_VERSION: u32 = 6;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfflineTrackMetadata {
    pub schema_version: u32,
    pub track: TrackView,
    pub album: AlbumView,
    pub artist: ArtistView,
    #[serde(default)]
    pub artists: Vec<ArtistView>,
}

pub(crate) fn offline_metadata_for_track(
    library: &library::Library,
    track: &Track,
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
) -> Result<OfflineTrackMetadata, String> {
    let album = library
        .get_album(&track.album_id)
        .map_err(|err| format!("library error: {}", err))?
        .ok_or_else(|| "album not found".to_string())?;
    let mut tracks = build_track_views(
        library,
        std::slice::from_ref(track),
        liked_set,
        playlist_set,
    )?;
    let track = tracks.pop().ok_or_else(|| "track not found".to_string())?;
    let album = build_album_view(library, album)?;
    let mut artist_ids = vec![track.artist_id.clone()];
    for credit in album.artist_credits.iter().chain(&track.artist_credits) {
        if credit.is_billed_artist() && !artist_ids.contains(&credit.artist_id) {
            artist_ids.push(credit.artist_id.clone());
        }
    }
    let contributors = artist_ids
        .iter()
        .map(|id| {
            library
                .get_artist(id)
                .map_err(|err| format!("library error: {}", err))?
                .ok_or_else(|| format!("artist not found: {id}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let artists = build_artist_views(library, contributors)?;
    let artist = artists
        .iter()
        .find(|artist| artist.id == track.artist_id)
        .cloned()
        .ok_or_else(|| "artist not found".to_string())?;
    Ok(OfflineTrackMetadata {
        schema_version: OFFLINE_METADATA_SCHEMA_VERSION,
        track,
        album,
        artist,
        artists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_metadata_keeps_json_and_persisted_field_order() {
        let expected = serde_json::json!({
            "schema_version": 6,
            "track": {
                "id": "track-1", "title": "Song", "artist": "Artist", "album": "Album",
                "artist_id": "artist-1", "artist_ids": [], "artist_names": [], "artist_credits": [],
                "album_id": "album-1", "duration_ms": 120000,
                "track_no": 1, "disc_no": null, "genres": ["Jazz"],
                "listening_genres": [],
                "liked": true, "in_playlists": false
            },
            "album": {
                "id": "album-1", "artist_id": "artist-1", "artist_ids": ["artist-1"],
                "artist_name": "Artist", "artist_names": ["Artist"], "artist_credits": [], "title": "Album",
                "year": 2026, "genres": ["Jazz"], "track_count": 1, "summary": null
            },
            "artist": {
                "id": "artist-1", "name": "Artist", "genres": ["Jazz"], "album_count": 1,
                "release_count": null,
                "collab_album_count": 0,
                "appearance_count": 0, "other_credit_count": 0,
                "summary": null, "logo_ref": null, "banner_ref": null
            },
            "artists": []
        });
        let metadata: OfflineTrackMetadata = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(&metadata).unwrap(), expected);
        let bytes = bincode::serialize(&metadata).unwrap();
        assert_eq!(
            bytes,
            bincode::serialize(&(
                6_u32,
                &metadata.track,
                &metadata.album,
                &metadata.artist,
                &metadata.artists
            ))
            .unwrap()
        );
        let restored: OfflineTrackMetadata = bincode::deserialize(&bytes).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), expected);
        assert_eq!(OFFLINE_METADATA_SCHEMA_VERSION, 6);

        let mut legacy = expected;
        legacy["schema_version"] = serde_json::json!(5);
        legacy.as_object_mut().unwrap().remove("artists");
        legacy["artist"]
            .as_object_mut()
            .unwrap()
            .remove("collab_album_count");
        let restored: OfflineTrackMetadata = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.schema_version, 5);
        assert!(restored.artists.is_empty());
        assert_eq!(restored.artist.collab_album_count, 0);
    }
}
