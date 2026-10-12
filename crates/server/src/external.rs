use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, Instant},
};

use futures_util::future::BoxFuture;
use library::AlbumSourceIdentity;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;

use crate::{musicbrainz_http, musicbrainz_rate_limit, performance};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    TheAudioDb,
    MusicBrainz,
}

#[derive(Clone, Debug)]
pub struct ExternalSource {
    pub provider: Provider,
    pub api_key: Option<String>,
    pub user_agent: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct ExternalConfig {
    pub sources: Vec<ExternalSource>,
}

#[derive(Clone, Debug, Default)]
pub struct ExternalMetadata {
    pub summary: Option<String>,
    pub genres: Vec<String>,
    pub logo_url: Option<String>,
    pub banner_url: Option<String>,
}

pub fn provider_from_str(value: &str) -> Option<Provider> {
    match value.trim().to_ascii_lowercase().as_str() {
        "theaudiodb" | "audio_db" | "audiodb" => Some(Provider::TheAudioDb),
        "musicbrainz" | "music_brainz" | "mb" => Some(Provider::MusicBrainz),
        _ => None,
    }
}

pub async fn fetch_artist(
    client: &Client,
    config: &ExternalConfig,
    artist_name: &str,
) -> Result<Option<ExternalMetadata>, String> {
    if !known_artist_name(artist_name) {
        info!(
            event = "metadata_lookup_skipped",
            kind = "artist",
            artist = artist_name,
            reason = "unknown_artist_identity"
        );
        return Ok(None);
    }
    let mut combined = ExternalMetadata::default();
    let mut found = false;
    let mut last_error: Option<String> = None;
    let results = fetch_sources(&config.sources, |source| {
        Box::pin(fetch_artist_source(client, source, artist_name))
    })
    .await;
    for (source, result) in config.sources.iter().zip(results) {
        log_source_result(source.provider, "artist", artist_name, None, &result);
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    event = "metadata_source_failed", provider = provider_name(source.provider),
                    kind = "artist", artist = artist_name, error = %err,
                    "External artist metadata source failed"
                );
                last_error = Some(err);
                continue;
            }
        };
        if let Some(metadata) = result {
            merge_metadata(&mut combined, metadata, source.provider);
            found = true;
        }
    }
    if found {
        Ok(Some(combined))
    } else if let Some(err) = last_error {
        Err(err)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
async fn fetch_album(
    client: &Client,
    config: &ExternalConfig,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    fetch_album_with_identity(client, config, artist_name, album_title, None).await
}

pub async fn fetch_album_with_identity(
    client: &Client,
    config: &ExternalConfig,
    artist_name: &str,
    album_title: &str,
    identity: Option<&AlbumSourceIdentity>,
) -> Result<Option<ExternalMetadata>, String> {
    if !known_artist_name(artist_name) || normalized_identity(album_title).is_empty() {
        info!(
            event = "metadata_lookup_skipped",
            kind = "album",
            artist = artist_name,
            album = album_title,
            reason = "missing_album_or_artist_identity"
        );
        return Ok(None);
    }
    let mut combined = ExternalMetadata::default();
    let mut found = false;
    let mut last_error: Option<String> = None;
    if config.sources.is_empty() {
        info!(
            event = "metadata_lookup_skipped",
            kind = "album",
            artist = artist_name,
            album = album_title,
            reason = "no_sources"
        );
    }
    let results = fetch_sources(&config.sources, |source| {
        Box::pin(async move {
            match identity {
                Some(identity) => {
                    fetch_album_source_by_identity(
                        client,
                        source,
                        artist_name,
                        album_title,
                        identity,
                    )
                    .await
                }
                None => fetch_album_source(client, source, artist_name, album_title).await,
            }
        })
    })
    .await;
    for (source, result) in config.sources.iter().zip(results) {
        log_source_result(
            source.provider,
            "album",
            artist_name,
            Some(album_title),
            &result,
        );
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    event = "metadata_source_failed", provider = provider_name(source.provider),
                    kind = "album", artist = artist_name, album = album_title, error = %err,
                    "External album metadata source failed"
                );
                last_error = Some(err);
                continue;
            }
        };
        if let Some(metadata) = result {
            merge_metadata(&mut combined, metadata, source.provider);
            found = true;
        }
    }
    if found {
        info!(
            event = "metadata_album_merge", artist = artist_name, album = album_title,
            description_present = combined.summary.is_some(), genre_count = combined.genres.len(),
            complete = combined.summary.is_some() && !combined.genres.is_empty(),
            provider_error = last_error.is_some(),
            "Combined album metadata fields; completeness here describes the response before storage"
        );
        Ok(Some(combined))
    } else if let Some(err) = last_error {
        Err(err)
    } else {
        Ok(None)
    }
}

type SourceResult = Result<Option<ExternalMetadata>, String>;

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::TheAudioDb => "theaudiodb",
        Provider::MusicBrainz => "musicbrainz",
    }
}

fn log_source_result(
    provider: Provider,
    kind: &'static str,
    artist: &str,
    album: Option<&str>,
    result: &SourceResult,
) {
    let (outcome, description_present, genre_count, logo_present, banner_present) = match result {
        Ok(Some(metadata)) => (
            "matched",
            metadata.summary.is_some(),
            metadata.genres.len(),
            metadata.logo_url.is_some(),
            metadata.banner_url.is_some(),
        ),
        Ok(None) => ("no_usable_metadata", false, 0, false, false),
        Err(_) => ("request_failed", false, 0, false, false),
    };
    info!(
        event = "metadata_source_result",
        provider = provider_name(provider),
        kind,
        artist,
        album = album.unwrap_or_default(),
        outcome,
        description_present,
        genre_count,
        description_supported = matches!(provider, Provider::TheAudioDb),
        logo_present,
        banner_present,
        "Metadata provider outcome"
    );
}

// TheAudioDB puts its credential in the URL path. Never propagate that URL in
// reqwest errors that are displayed or written to logs.
fn request_error(stage: &'static str, error: reqwest::Error) -> String {
    let reason = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection"
    } else if error.is_decode() {
        "decode"
    } else if error.is_body() {
        "response_body"
    } else {
        "request"
    };
    format!("{stage}: {reason}: {}", error.without_url())
}

// The two independent providers can overlap. Entries for the same provider
// remain sequential, and callers still merge every result in configured order.
async fn fetch_sources<'a, F>(sources: &'a [ExternalSource], fetch: F) -> Vec<SourceResult>
where
    F: Fn(&'a ExternalSource) -> BoxFuture<'a, SourceResult>,
{
    async fn fetch_provider<'a, F>(
        sources: &'a [ExternalSource],
        provider: Provider,
        fetch: &F,
    ) -> Vec<(usize, SourceResult)>
    where
        F: Fn(&'a ExternalSource) -> BoxFuture<'a, SourceResult>,
    {
        let mut results = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            if source.provider == provider {
                results.push((index, fetch(source).await));
            }
        }
        results
    }

    let (audio_db, musicbrainz) = tokio::join!(
        fetch_provider(sources, Provider::TheAudioDb, &fetch),
        fetch_provider(sources, Provider::MusicBrainz, &fetch),
    );
    let mut results: Vec<_> = audio_db.into_iter().chain(musicbrainz).collect();
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

async fn fetch_artist_source(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
) -> SourceResult {
    let started = performance::enabled().then(Instant::now);
    let result = match source.provider {
        Provider::TheAudioDb => fetch_theaudiodb_artist(client, source, artist_name).await,
        Provider::MusicBrainz => fetch_musicbrainz_artist(client, source, artist_name).await,
    };
    record_source_timing(source.provider, "artist", started, &result);
    result
}

async fn fetch_album_source(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
    album_title: &str,
) -> SourceResult {
    let started = performance::enabled().then(Instant::now);
    let result = match source.provider {
        Provider::TheAudioDb => {
            fetch_theaudiodb_album(client, source, artist_name, album_title).await
        }
        Provider::MusicBrainz => {
            fetch_musicbrainz_album(client, source, artist_name, album_title).await
        }
    };
    record_source_timing(source.provider, "album", started, &result);
    result
}

async fn fetch_album_source_by_identity(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
    album_title: &str,
    identity: &AlbumSourceIdentity,
) -> SourceResult {
    let started = performance::enabled().then(Instant::now);
    let result = match source.provider {
        Provider::TheAudioDb => fetch_theaudiodb_album_by_identity(client, source, identity).await,
        Provider::MusicBrainz => {
            fetch_musicbrainz_album_by_identity(client, source, identity).await
        }
    };
    info!(
        event = "metadata_album_identity_lookup",
        provider = provider_name(source.provider),
        artist = artist_name,
        album = album_title,
        release_group_id = identity.musicbrainz_release_group_id,
        evidence = identity.evidence,
        matched = matches!(&result, Ok(Some(_))),
        "Album metadata lookup uses a locally verified release-group identity"
    );
    record_source_timing(source.provider, "album", started, &result);
    result
}

fn record_source_timing(
    provider: Provider,
    kind: &'static str,
    started: Option<Instant>,
    result: &SourceResult,
) {
    let Some(started) = started else { return };
    let provider = match provider {
        Provider::TheAudioDb => "theaudiodb",
        Provider::MusicBrainz => "musicbrainz",
    };
    let outcome = match result {
        Ok(Some(_)) => "found",
        Ok(None) => "not_found",
        Err(_) => "error",
    };
    tracing::info!(
        target: "performance",
        event = "metadata_source_fetch",
        provider,
        kind,
        outcome,
        elapsed_ms = performance::milliseconds(started.elapsed()),
    );
}

pub async fn test_source(client: &Client, source: &ExternalSource) -> Result<(), String> {
    match source.provider {
        Provider::TheAudioDb => {
            let api_key = source.api_key.as_deref().unwrap_or("");
            if api_key.trim().is_empty() {
                return Err("api key is required".to_string());
            }
            let url = format!(
                "https://www.theaudiodb.com/api/v1/json/{}/search.php?s=radiohead",
                api_key.trim()
            );
            let response = client
                .get(&url)
                .timeout(source.timeout)
                .send()
                .await
                .map_err(|err| request_error("source_test_send", err))?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("http {}", response.status()))
            }
        }
        Provider::MusicBrainz => {
            let user_agent = source.user_agent.as_deref().unwrap_or("");
            if user_agent.trim().is_empty() {
                return Err("user_agent is required".to_string());
            }
            let url =
                "https://musicbrainz.org/ws/2/artist/?query=artist:radiohead&fmt=json&limit=1";
            musicbrainz_rate_limit::wait_for_slot().await;
            let response = client
                .get(url)
                .timeout(source.timeout)
                .header("User-Agent", user_agent.trim())
                .send()
                .await
                .map_err(|err| request_error("source_test_send", err))?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("http {}", response.status()))
            }
        }
    }
}

#[derive(Deserialize)]
struct TheAudioDbArtistResponse {
    artists: Option<Vec<TheAudioDbArtist>>,
}

impl TheAudioDbArtistResponse {
    fn into_match(self, artist_name: &str) -> Option<TheAudioDbArtist> {
        diagnosed_unique_match(
            self.artists.unwrap_or_default(),
            Provider::TheAudioDb,
            "artist",
            artist_name,
            None,
            |artist| CandidateCheck {
                artist: identity_check(artist_name, artist.name.as_deref().unwrap_or_default()),
                title: None,
            },
            |artist| {
                format!(
                    "artist={:?}",
                    diagnostic_text(artist.name.as_deref().unwrap_or_default())
                )
            },
        )
    }
}

#[derive(Deserialize)]
struct TheAudioDbArtist {
    #[serde(rename = "strArtist")]
    name: Option<String>,
    #[serde(rename = "strBiographyEN")]
    bio: Option<String>,
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strStyle")]
    style: Option<String>,
    #[serde(rename = "strArtistLogo")]
    logo: Option<String>,
    #[serde(rename = "strArtistClearart")]
    clearart: Option<String>,
    #[serde(rename = "strArtistCutout")]
    cutout: Option<String>,
    #[serde(rename = "strArtistBanner")]
    banner: Option<String>,
    #[serde(rename = "strArtistThumb")]
    thumb: Option<String>,
    #[serde(rename = "strArtistWideThumb")]
    wide_thumb: Option<String>,
    #[serde(rename = "strArtistFanart")]
    fanart: Option<String>,
    #[serde(rename = "strArtistFanart2")]
    fanart2: Option<String>,
    #[serde(rename = "strArtistFanart3")]
    fanart3: Option<String>,
    #[serde(rename = "strArtistFanart4")]
    fanart4: Option<String>,
    #[serde(flatten)]
    fields: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct TheAudioDbAlbumResponse {
    album: Option<Vec<TheAudioDbAlbum>>,
}

impl TheAudioDbAlbumResponse {
    fn into_match(self, artist_name: &str, album_title: &str) -> Option<TheAudioDbAlbum> {
        diagnosed_unique_match(
            self.album.unwrap_or_default(),
            Provider::TheAudioDb,
            "album",
            artist_name,
            Some(album_title),
            |album| CandidateCheck {
                artist: identity_check(
                    artist_name,
                    album.artist_name.as_deref().unwrap_or_default(),
                ),
                title: Some(identity_check(
                    album_title,
                    album.title.as_deref().unwrap_or_default(),
                )),
            },
            |album| {
                format!(
                    "artist={:?}, title={:?}",
                    diagnostic_text(album.artist_name.as_deref().unwrap_or_default()),
                    diagnostic_text(album.title.as_deref().unwrap_or_default())
                )
            },
        )
    }
}

#[derive(Deserialize)]
struct TheAudioDbAlbum {
    #[serde(rename = "strArtist")]
    artist_name: Option<String>,
    #[serde(rename = "strAlbum")]
    title: Option<String>,
    #[serde(rename = "strDescriptionEN")]
    description: Option<String>,
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strStyle")]
    style: Option<String>,
    #[serde(flatten)]
    fields: HashMap<String, Value>,
}

async fn fetch_theaudiodb_artist(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let api_key = source.api_key.as_deref().unwrap_or("").trim();
    if api_key.is_empty() {
        info!(
            event = "metadata_source_skipped",
            provider = "theaudiodb",
            kind = "artist",
            artist = artist_name,
            reason = "missing_api_key"
        );
        return Ok(None);
    }
    let url = format!(
        "https://www.theaudiodb.com/api/v1/json/{}/search.php?s={}",
        api_key,
        url_escape(artist_name.trim())
    );
    let response = client
        .get(&url)
        .timeout(source.timeout)
        .send()
        .await
        .map_err(|err| request_error("artist_search_send", err))?;
    if !response.status().is_success() {
        return Err(format!("artist_search_http: http {}", response.status()));
    }
    let payload = response
        .json::<TheAudioDbArtistResponse>()
        .await
        .map_err(|err| request_error("artist_search_decode", err))?;
    let artist = match payload.into_match(artist_name) {
        Some(artist) => artist,
        None => return Ok(None),
    };

    let summary = clean_text_field(artist.bio, &artist.fields, "strBiography");
    if summary.is_none() {
        warn!(
            event = "metadata_fields_missing",
            provider = "theaudiodb",
            kind = "artist",
            artist = artist_name,
            reason = "matched_record_missing_description",
            "TheAudioDB verified artist has no description"
        );
    }
    let genres = collect_genres(&[artist.genre, artist.style]);
    let thumb = artist.thumb.clone();
    let logo_url = clean_url(artist.logo)
        .or_else(|| clean_url(artist.clearart))
        .or_else(|| clean_url(artist.cutout))
        .or_else(|| clean_url(thumb.clone()));
    let banner_url = clean_url(artist.fanart)
        .or_else(|| clean_url(artist.fanart2))
        .or_else(|| clean_url(artist.fanart3))
        .or_else(|| clean_url(artist.fanart4))
        .or_else(|| clean_url(artist.wide_thumb))
        .or_else(|| clean_url(artist.banner))
        .or_else(|| clean_url(thumb));
    Ok(Some(ExternalMetadata {
        summary,
        genres,
        logo_url,
        banner_url,
    }))
}

async fn fetch_theaudiodb_album(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let api_key = source.api_key.as_deref().unwrap_or("").trim();
    if api_key.is_empty() {
        info!(
            event = "metadata_source_skipped",
            provider = "theaudiodb",
            kind = "album",
            artist = artist_name,
            album = album_title,
            reason = "missing_api_key"
        );
        return Ok(None);
    }
    let mut best = None;
    let candidates = album_title_candidates(album_title);
    info!(event = "metadata_album_lookup", provider = "theaudiodb", artist = artist_name, album = album_title, attempted_titles = ?candidates);
    for (index, candidate) in candidates.iter().enumerate() {
        let metadata =
            fetch_theaudiodb_album_candidate(client, source, api_key, artist_name, candidate)
                .await
                .map_err(|error| {
                    format!(
                        "album_title_candidate={:?}: {error}",
                        diagnostic_text(candidate)
                    )
                })?;
        info!(
            event = "metadata_album_title_attempt",
            provider = "theaudiodb",
            artist = artist_name,
            album = album_title,
            attempted_title = candidate,
            attempt = index + 1,
            title_candidates = candidates.len(),
            matched = metadata.is_some(),
            description_present = metadata
                .as_ref()
                .is_some_and(|metadata| metadata.summary.is_some()),
            genre_count = metadata
                .as_ref()
                .map_or(0, |metadata| metadata.genres.len())
        );
        let Some(metadata) = metadata else {
            continue;
        };
        if metadata.summary.is_some() {
            return Ok(Some(metadata));
        }
        if best.is_none() {
            best = Some(metadata);
        }
    }
    log_theaudiodb_album_incomplete(artist_name, album_title, &candidates, best.as_ref());
    Ok(best)
}

fn log_theaudiodb_album_incomplete(
    artist_name: &str,
    album_title: &str,
    candidates: &[String],
    best: Option<&ExternalMetadata>,
) {
    let reason = if best.is_some() {
        "matched_record_missing_description"
    } else {
        "no_verified_album_match"
    };
    warn!(
        event = "metadata_album_lookup_incomplete", provider = "theaudiodb",
        artist = artist_name, album = album_title, reason,
        attempted_titles = ?candidates,
        "TheAudioDB album lookup could not supply a description"
    );
}

async fn fetch_theaudiodb_album_candidate(
    client: &Client,
    source: &ExternalSource,
    api_key: &str,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let url = format!(
        "https://www.theaudiodb.com/api/v1/json/{}/searchalbum.php?s={}&a={}",
        api_key,
        url_escape(artist_name.trim()),
        url_escape(album_title)
    );
    let response = client
        .get(&url)
        .timeout(source.timeout)
        .send()
        .await
        .map_err(|err| request_error("album_search_send", err))?;
    if !response.status().is_success() {
        return Err(format!("album_search_http: http {}", response.status()));
    }
    let payload = response
        .json::<TheAudioDbAlbumResponse>()
        .await
        .map_err(|err| request_error("album_search_decode", err))?;
    let album = match payload.into_match(artist_name, album_title) {
        Some(album) => album,
        None => return Ok(None),
    };

    Ok(Some(theaudiodb_album_metadata(album)))
}

fn theaudiodb_album_metadata(album: TheAudioDbAlbum) -> ExternalMetadata {
    ExternalMetadata {
        summary: clean_text_field(album.description, &album.fields, "strDescription"),
        genres: collect_genres(&[album.genre, album.style]),
        logo_url: None,
        banner_url: None,
    }
}

async fn fetch_theaudiodb_album_by_identity(
    client: &Client,
    source: &ExternalSource,
    identity: &AlbumSourceIdentity,
) -> SourceResult {
    let api_key = source.api_key.as_deref().unwrap_or("").trim();
    if api_key.is_empty() {
        return Ok(None);
    }
    let Some(group_id) = musicbrainz_id(Some(&identity.musicbrainz_release_group_id)) else {
        return Ok(None);
    };
    let url = format!("https://www.theaudiodb.com/api/v1/json/{api_key}/album-mb.php?i={group_id}");
    let response = client
        .get(&url)
        .timeout(source.timeout)
        .send()
        .await
        .map_err(|err| request_error("album_mbid_send", err))?;
    if !response.status().is_success() {
        return Err(format!("album_mbid_http: http {}", response.status()));
    }
    let payload = response
        .json::<TheAudioDbAlbumResponse>()
        .await
        .map_err(|err| request_error("album_mbid_decode", err))?;
    Ok(theaudiodb_album_identity_metadata(payload, &group_id))
}

fn theaudiodb_album_identity_metadata(
    payload: TheAudioDbAlbumResponse,
    group_id: &str,
) -> Option<ExternalMetadata> {
    let candidates = payload.album.unwrap_or_default();
    let count = candidates.len();
    let mut matching = candidates.into_iter().filter(|album| {
        // The release programme was verified before this cross-provider lookup.
        // Billing and titles may differ (performer/composer, aliases, reissues),
        // but the provider must explicitly return that same release-group ID.
        let returned_id = album.fields.get("strMusicBrainzID").and_then(Value::as_str);
        musicbrainz_id(returned_id).as_deref() == Some(group_id)
            && album.title.as_deref().is_some_and(|v| !v.trim().is_empty())
            && album
                .artist_name
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
    });
    let first = matching.next();
    let ambiguous = matching.next().is_some();
    let reason = if ambiguous {
        "ambiguous_provider_group_records"
    } else if first.is_none() && count == 0 {
        "empty_provider_group_lookup"
    } else if first.is_none() {
        "provider_group_identity_mismatch_or_missing"
    } else {
        "verified_provider_group"
    };
    info!(
        event = "metadata_provider_group_result",
        provider = "theaudiodb",
        release_group_id = group_id,
        candidates = count,
        reason
    );
    if ambiguous {
        return None;
    }
    let album = first?;
    let metadata = theaudiodb_album_metadata(album);
    info!(
        event = "metadata_verified_record",
        provider = "theaudiodb",
        kind = "album",
        release_group_id = group_id,
        description_present = metadata.summary.is_some(),
        genre_count = metadata.genres.len(),
        reason = if metadata.summary.is_some() {
            "description_found"
        } else {
            "verified_record_missing_description"
        }
    );
    Some(metadata)
}

async fn fetch_musicbrainz_album_by_identity(
    client: &Client,
    source: &ExternalSource,
    identity: &AlbumSourceIdentity,
) -> SourceResult {
    let user_agent = source.user_agent.as_deref().unwrap_or("").trim();
    if user_agent.is_empty() {
        return Ok(None);
    }
    let Some(group_id) = musicbrainz_id(Some(&identity.musicbrainz_release_group_id)) else {
        return Ok(None);
    };
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/{group_id}?fmt=json&inc=genres+artist-credits"
    );
    let detail: MusicBrainzReleaseGroup =
        musicbrainz_http::get_json(client, &url, user_agent, source.timeout)
            .await
            .map_err(|error| format!("album_verified_group_detail: {error}"))?;
    if let Some(reason) = cached_musicbrainz_group_rejection(identity, &detail) {
        info!(
            event = "metadata_detail_rejected",
            provider = "musicbrainz",
            kind = "album",
            release_group_id = group_id,
            reason,
            returned_mbid = diagnostic_text(detail.id.as_deref().unwrap_or_default())
        );
        return Ok(None);
    }
    let metadata = musicbrainz_genre_metadata(detail.genres);
    log_musicbrainz_verified_fields(
        "album",
        &identity.artist_names.join(" / "),
        Some(&identity.canonical_title),
        &group_id,
        metadata.as_ref(),
    );
    Ok(metadata)
}

fn cached_musicbrainz_group_rejection(
    identity: &AlbumSourceIdentity,
    detail: &MusicBrainzReleaseGroup,
) -> Option<&'static str> {
    if musicbrainz_id(detail.id.as_deref()).as_deref()
        != Some(identity.musicbrainz_release_group_id.as_str())
    {
        return Some("detail_mbid_mismatch_or_missing");
    }
    if !crate::album_lookup::titles_equivalent(
        &identity.canonical_title,
        detail.title.as_deref().unwrap_or_default(),
    ) {
        return Some("verified_group_title_changed_or_missing");
    }
    let returned: Vec<_> = detail
        .artist_credit
        .iter()
        .filter_map(|credit| musicbrainz_id(credit.artist.as_ref()?.id.as_deref()))
        .collect();
    if returned.is_empty()
        || returned.len() != detail.artist_credit.len()
        || returned.len() != identity.artist_ids.len()
        || !returned
            .iter()
            .zip(&identity.artist_ids)
            .all(|(a, b)| a == b)
    {
        return Some("verified_group_artist_ids_changed_or_missing");
    }
    None
}

#[derive(Deserialize)]
struct MusicBrainzArtistResponse {
    artists: Option<Vec<MusicBrainzArtist>>,
}

impl MusicBrainzArtistResponse {
    fn into_match(self, artist_name: &str) -> Option<MusicBrainzArtist> {
        diagnosed_unique_match(
            self.artists.unwrap_or_default(),
            Provider::MusicBrainz,
            "artist",
            artist_name,
            None,
            |artist| CandidateCheck {
                artist: identity_check(artist_name, artist.name.as_deref().unwrap_or_default()),
                title: None,
            },
            |artist| {
                format!(
                    "artist={:?}",
                    diagnostic_text(artist.name.as_deref().unwrap_or_default())
                )
            },
        )
    }
}

#[derive(Deserialize)]
struct MusicBrainzArtist {
    id: Option<String>,
    name: Option<String>,
    genres: Option<Vec<MusicBrainzGenre>>,
}

#[derive(Deserialize)]
struct MusicBrainzReleaseGroupResponse {
    #[serde(rename = "release-groups")]
    release_groups: Option<Vec<MusicBrainzReleaseGroup>>,
}

impl MusicBrainzReleaseGroupResponse {
    fn into_match(self, artist_name: &str, album_title: &str) -> Option<MusicBrainzReleaseGroup> {
        diagnosed_unique_match(
            self.release_groups.unwrap_or_default(),
            Provider::MusicBrainz,
            "album",
            artist_name,
            Some(album_title),
            |album| CandidateCheck {
                artist: musicbrainz_credit_check(artist_name, &album.artist_credit),
                title: Some(identity_check(
                    album_title,
                    album.title.as_deref().unwrap_or_default(),
                )),
            },
            |album| {
                format!(
                    "artist_credit={:?}, title={:?}",
                    diagnostic_text(&musicbrainz_credit_display(&album.artist_credit)),
                    diagnostic_text(album.title.as_deref().unwrap_or_default())
                )
            },
        )
    }
}

#[derive(Deserialize)]
struct MusicBrainzReleaseGroup {
    id: Option<String>,
    title: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<MusicBrainzArtistCredit>,
    genres: Option<Vec<MusicBrainzGenre>>,
}

#[derive(Deserialize)]
struct MusicBrainzArtistCredit {
    name: Option<String>,
    #[serde(default)]
    joinphrase: String,
    artist: Option<MusicBrainzCreditArtist>,
}

#[derive(Deserialize)]
struct MusicBrainzCreditArtist {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct MusicBrainzGenre {
    name: String,
}

async fn fetch_musicbrainz_artist(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let user_agent = source.user_agent.as_deref().unwrap_or("").trim();
    if user_agent.is_empty() {
        info!(
            event = "metadata_source_skipped",
            provider = "musicbrainz",
            kind = "artist",
            artist = artist_name,
            reason = "missing_user_agent"
        );
        return Ok(None);
    }
    let query = format!("artist:{}", lucene_phrase(artist_name.trim()));
    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query={}&fmt=json&limit=25",
        url_escape(&query)
    );
    let payload: MusicBrainzArtistResponse =
        musicbrainz_http::get_json(client, &url, user_agent, source.timeout)
            .await
            .map_err(|error| format!("artist_search: {error}"))?;
    verified_musicbrainz_artist_metadata(payload, artist_name, |id| async move {
        let url = format!("https://musicbrainz.org/ws/2/artist/{id}?fmt=json&inc=genres");
        musicbrainz_http::get_json(client, &url, user_agent, source.timeout)
            .await
            .map_err(|error| format!("artist_detail: {error}"))
    })
    .await
}

async fn fetch_musicbrainz_album(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let user_agent = source.user_agent.as_deref().unwrap_or("").trim();
    if user_agent.is_empty() {
        info!(
            event = "metadata_source_skipped",
            provider = "musicbrainz",
            kind = "album",
            artist = artist_name,
            album = album_title,
            reason = "missing_user_agent"
        );
        return Ok(None);
    }
    let query = format!(
        "artist:{} AND releasegroup:{}",
        lucene_phrase(artist_name.trim()),
        lucene_phrase(album_title.trim())
    );
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/?query={}&fmt=json&limit=25",
        url_escape(&query)
    );
    let payload: MusicBrainzReleaseGroupResponse =
        musicbrainz_http::get_json(client, &url, user_agent, source.timeout)
            .await
            .map_err(|error| format!("album_search: {error}"))?;
    verified_musicbrainz_album_metadata(payload, artist_name, album_title, |id| async move {
        let url = format!(
            "https://musicbrainz.org/ws/2/release-group/{id}?fmt=json&inc=genres+artist-credits"
        );
        musicbrainz_http::get_json(client, &url, user_agent, source.timeout)
            .await
            .map_err(|error| format!("album_detail: {error}"))
    })
    .await
}

fn musicbrainz_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.len() != 36 {
        return None;
    }
    let id = uuid::Uuid::parse_str(value).ok()?;
    (!id.is_nil()).then(|| id.hyphenated().to_string())
}

// Search results expose general folksonomy tags. Genres require a separate
// MBID lookup with inc=genres, followed by identity validation of that detail.
// A disambiguation comment distinguishes homonyms; it is not an artist bio.
async fn verified_musicbrainz_artist_metadata<F, Fut>(
    search: MusicBrainzArtistResponse,
    artist_name: &str,
    mut lookup: F,
) -> SourceResult
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<MusicBrainzArtist, String>>,
{
    let Some(artist) = search.into_match(artist_name) else {
        return Ok(None);
    };
    let Some(id) = musicbrainz_id(artist.id.as_deref()) else {
        info!(
            event = "metadata_detail_rejected",
            provider = "musicbrainz",
            kind = "artist",
            artist = artist_name,
            reason = "missing_or_invalid_mbid"
        );
        return Ok(None);
    };
    let detail = lookup(id.clone()).await?;
    if let Some(reason) = musicbrainz_artist_detail_rejection(&id, artist_name, &detail) {
        info!(
            event = "metadata_detail_rejected",
            provider = "musicbrainz",
            kind = "artist",
            artist = artist_name,
            mbid = id,
            reason,
            returned_artist = diagnostic_text(detail.name.as_deref().unwrap_or_default()),
            returned_mbid = diagnostic_text(detail.id.as_deref().unwrap_or_default())
        );
        return Ok(None);
    }
    let metadata = musicbrainz_genre_metadata(detail.genres);
    log_musicbrainz_verified_fields("artist", artist_name, None, &id, metadata.as_ref());
    Ok(metadata)
}

async fn verified_musicbrainz_album_metadata<F, Fut>(
    search: MusicBrainzReleaseGroupResponse,
    artist_name: &str,
    album_title: &str,
    mut lookup: F,
) -> SourceResult
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<MusicBrainzReleaseGroup, String>>,
{
    let Some(album) = search.into_match(artist_name, album_title) else {
        return Ok(None);
    };
    let Some(id) = musicbrainz_id(album.id.as_deref()) else {
        info!(
            event = "metadata_detail_rejected",
            provider = "musicbrainz",
            kind = "album",
            artist = artist_name,
            album = album_title,
            reason = "missing_or_invalid_mbid"
        );
        return Ok(None);
    };
    let detail = lookup(id.clone()).await?;
    if let Some(reason) = musicbrainz_album_detail_rejection(&id, artist_name, album_title, &detail)
    {
        info!(
            event = "metadata_detail_rejected",
            provider = "musicbrainz",
            kind = "album",
            artist = artist_name,
            album = album_title,
            mbid = id,
            reason,
            returned_title = diagnostic_text(detail.title.as_deref().unwrap_or_default()),
            returned_artist_credit =
                diagnostic_text(&musicbrainz_credit_display(&detail.artist_credit)),
            returned_mbid = diagnostic_text(detail.id.as_deref().unwrap_or_default())
        );
        return Ok(None);
    }
    let metadata = musicbrainz_genre_metadata(detail.genres);
    log_musicbrainz_verified_fields(
        "album",
        artist_name,
        Some(album_title),
        &id,
        metadata.as_ref(),
    );
    Ok(metadata)
}

fn musicbrainz_artist_detail_rejection(
    id: &str,
    artist: &str,
    detail: &MusicBrainzArtist,
) -> Option<&'static str> {
    if musicbrainz_id(detail.id.as_deref()).as_deref() != Some(id) {
        Some("detail_mbid_mismatch_or_missing")
    } else if !identity_matches(artist, detail.name.as_deref().unwrap_or_default()) {
        Some("detail_artist_mismatch_or_missing")
    } else {
        None
    }
}

fn musicbrainz_album_detail_rejection(
    id: &str,
    artist: &str,
    album: &str,
    detail: &MusicBrainzReleaseGroup,
) -> Option<&'static str> {
    if musicbrainz_id(detail.id.as_deref()).as_deref() != Some(id) {
        Some("detail_mbid_mismatch_or_missing")
    } else if !identity_matches(album, detail.title.as_deref().unwrap_or_default()) {
        Some("detail_title_mismatch_or_missing")
    } else if !musicbrainz_credit_matches(artist, &detail.artist_credit) {
        Some("detail_artist_mismatch_or_missing")
    } else {
        None
    }
}

fn log_musicbrainz_verified_fields(
    kind: &'static str,
    artist: &str,
    album: Option<&str>,
    id: &str,
    metadata: Option<&ExternalMetadata>,
) {
    info!(
        event = "metadata_verified_record", provider = "musicbrainz", kind, artist,
        album = album.unwrap_or_default(), mbid = id,
        reason = if metadata.is_some() { "genres_found" } else { "verified_record_missing_genres" },
        genre_count = metadata.map_or(0, |metadata| metadata.genres.len()), description_supported = false,
        "Verified MusicBrainz record; this integration supplies genres and does not supply descriptions"
    );
}

fn musicbrainz_genre_metadata(genres: Option<Vec<MusicBrainzGenre>>) -> Option<ExternalMetadata> {
    let mut out = Vec::new();
    for genre in genres.unwrap_or_default() {
        let name = genre.name.trim();
        if !name.is_empty()
            && !out
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            // API genre names are individual labels. Do not split punctuation
            // within recognized names such as "funk / soul".
            out.push(name.to_owned());
        }
    }
    (!out.is_empty()).then(|| ExternalMetadata {
        genres: out,
        ..ExternalMetadata::default()
    })
}

fn collect_genres(values: &[Option<String>]) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if let Some(value) = clean_text(value.clone()) {
            add_genre_parts(&mut out, &value);
        }
    }
    out
}

fn add_genre_parts(out: &mut Vec<String>, value: &str) {
    let value = normalize_genre_separators(value);
    for part in value.split(&[';', ',', '/', '|', '\0'][..]) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(trimmed))
        {
            out.push(trimmed.to_string());
        }
    }
}

fn normalize_genre_separators(value: &str) -> String {
    value
        .replace("&bull;", "|")
        .replace("&#8226;", "|")
        .replace("&#x2022;", "|")
        .replace("&middot;", "|")
        .replace("\u{2022}", "|")
        .replace("\u{00B7}", "|")
        .replace("\u{00E2}\u{20AC}\u{00A2}", "|")
        .replace("\u{00E2}\u{0080}\u{00A2}", "|")
        .replace("\u{00C2}\u{20AC}\u{00A2}", "|")
}

fn merge_metadata(base: &mut ExternalMetadata, incoming: ExternalMetadata, provider: Provider) {
    let prefer_summary = matches!(provider, Provider::TheAudioDb);
    let prefer_genres = matches!(provider, Provider::MusicBrainz);

    if let Some(summary) = incoming.summary {
        if prefer_summary || base.summary.is_none() {
            base.summary = Some(summary);
        }
    }
    if !incoming.genres.is_empty() {
        if prefer_genres || base.genres.is_empty() {
            base.genres = incoming.genres;
        }
    }
    if base.logo_url.is_none() {
        base.logo_url = incoming.logo_url;
    }
    if base.banner_url.is_none() {
        base.banner_url = incoming.banner_url;
    }
}

fn album_title_candidates(title: &str) -> Vec<String> {
    crate::album_lookup::title_candidates(title)
}

fn normalized_identity(value: &str) -> String {
    let mut normalized = String::new();
    let mut separated = false;
    for ch in value.nfkc().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if separated && !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push(ch);
            separated = false;
        } else {
            separated = true;
        }
    }
    normalized
}

fn known_artist_name(value: &str) -> bool {
    !matches!(
        normalized_identity(value).as_str(),
        "" | "unknown" | "unknown artist"
    )
}

fn identity_matches(expected: &str, actual: &str) -> bool {
    let expected = normalized_identity(expected);
    !expected.is_empty() && expected == normalized_identity(actual)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdentityCheck {
    Matched,
    Missing,
    Mismatch,
}

fn identity_check(expected: &str, actual: &str) -> IdentityCheck {
    if identity_matches(expected, actual) {
        IdentityCheck::Matched
    } else if normalized_identity(actual).is_empty() {
        IdentityCheck::Missing
    } else {
        IdentityCheck::Mismatch
    }
}

#[derive(Clone, Copy, Debug)]
struct CandidateCheck {
    artist: IdentityCheck,
    title: Option<IdentityCheck>,
}

impl CandidateCheck {
    fn matched(self) -> bool {
        self.artist == IdentityCheck::Matched
            && self
                .title
                .is_none_or(|title| title == IdentityCheck::Matched)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SearchDiagnostics {
    candidates: usize,
    exact_matches: usize,
    missing_artist: usize,
    artist_mismatch: usize,
    missing_title: usize,
    title_mismatch: usize,
    rejected_samples: Vec<String>,
}

impl SearchDiagnostics {
    fn outcome(&self) -> &'static str {
        match (self.candidates, self.exact_matches) {
            (0, _) => "search_empty",
            (_, 0) => "no_verified_match",
            (_, 1) => "matched",
            _ => "ambiguous_match",
        }
    }
}

// Search ranking is not evidence of identity. Preserve the unique-match policy
// while examining all candidates so rejection counts explain the result.
fn select_unique_match<T>(
    items: Vec<T>,
    check: impl Fn(&T) -> CandidateCheck,
    describe: impl Fn(&T) -> String,
) -> (Option<T>, SearchDiagnostics) {
    let mut diagnostics = SearchDiagnostics::default();
    let mut selected = None;
    for item in items {
        diagnostics.candidates += 1;
        let identity = check(&item);
        match identity.artist {
            IdentityCheck::Missing => diagnostics.missing_artist += 1,
            IdentityCheck::Mismatch => diagnostics.artist_mismatch += 1,
            IdentityCheck::Matched => {}
        }
        match identity.title {
            Some(IdentityCheck::Missing) => diagnostics.missing_title += 1,
            Some(IdentityCheck::Mismatch) => diagnostics.title_mismatch += 1,
            _ => {}
        }
        if identity.matched() {
            diagnostics.exact_matches += 1;
            if diagnostics.exact_matches == 1 {
                selected = Some(item);
            } else {
                selected = None;
            }
        } else if diagnostics.rejected_samples.len() < 3 {
            diagnostics.rejected_samples.push(format!(
                "{}; artist={:?}, title={:?}",
                describe(&item),
                identity.artist,
                identity.title
            ));
        }
    }
    (selected, diagnostics)
}

fn diagnosed_unique_match<T>(
    items: Vec<T>,
    provider: Provider,
    kind: &'static str,
    artist: &str,
    album: Option<&str>,
    check: impl Fn(&T) -> CandidateCheck,
    describe: impl Fn(&T) -> String,
) -> Option<T> {
    let (selected, diagnostics) = select_unique_match(items, check, describe);
    info!(
        event = "metadata_search_result", provider = provider_name(provider), kind, artist,
        attempted_title = album.unwrap_or_default(), outcome = diagnostics.outcome(),
        candidates = diagnostics.candidates, exact_matches = diagnostics.exact_matches,
        missing_artist = diagnostics.missing_artist, artist_mismatch = diagnostics.artist_mismatch,
        missing_title = diagnostics.missing_title, title_mismatch = diagnostics.title_mismatch,
        rejected_samples = ?diagnostics.rejected_samples,
        "Metadata identity search result; a candidate can have multiple rejection reasons"
    );
    selected
}

fn diagnostic_text(value: &str) -> String {
    let mut chars = value.chars();
    let mut bounded: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn musicbrainz_credit_check(expected: &str, credits: &[MusicBrainzArtistCredit]) -> IdentityCheck {
    if musicbrainz_credit_matches(expected, credits) {
        IdentityCheck::Matched
    } else if credits.iter().all(|credit| {
        normalized_identity(credit.name.as_deref().unwrap_or_default()).is_empty()
            && normalized_identity(
                credit
                    .artist
                    .as_ref()
                    .and_then(|artist| artist.name.as_deref())
                    .unwrap_or_default(),
            )
            .is_empty()
    }) {
        IdentityCheck::Missing
    } else {
        IdentityCheck::Mismatch
    }
}

fn musicbrainz_credit_display(credits: &[MusicBrainzArtistCredit]) -> String {
    // Bound work and output even if a malformed provider response has many credits.
    credits
        .iter()
        .take(8)
        .map(|credit| {
            format!(
                "credited={:?}, canonical={:?}, join={:?}",
                diagnostic_text(credit.name.as_deref().unwrap_or_default()),
                diagnostic_text(
                    credit
                        .artist
                        .as_ref()
                        .and_then(|artist| artist.name.as_deref())
                        .unwrap_or_default()
                ),
                diagnostic_text(&credit.joinphrase)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn musicbrainz_credit_matches(expected: &str, credits: &[MusicBrainzArtistCredit]) -> bool {
    let mut combined_credit = String::new();
    let mut combined_canonical = String::new();
    for credit in credits {
        let credited_name = credit.name.as_deref().unwrap_or_default();
        let canonical_name = credit
            .artist
            .as_ref()
            .and_then(|artist| artist.name.as_deref())
            .unwrap_or(credited_name);
        if identity_matches(expected, credited_name) || identity_matches(expected, canonical_name) {
            return true;
        }
        combined_credit.push_str(credited_name);
        combined_credit.push_str(&credit.joinphrase);
        combined_canonical.push_str(canonical_name);
        combined_canonical.push_str(&credit.joinphrase);
    }
    identity_matches(expected, &combined_credit) || identity_matches(expected, &combined_canonical)
}

fn lucene_phrase(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn clean_text_field(
    primary: Option<String>,
    fields: &HashMap<String, Value>,
    prefix: &str,
) -> Option<String> {
    if let Some(value) = clean_text(primary) {
        return Some(value);
    }

    let mut keys = fields
        .keys()
        .filter(|key| key.starts_with(prefix))
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        if let Some(value) = fields.get(&key).and_then(Value::as_str) {
            if let Some(value) = clean_text(Some(value.to_string())) {
                return Some(value);
            }
        }
    }
    None
}

fn clean_text(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn clean_url(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn url_escape(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::*;

    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(action: impl FnOnce()) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || LogBuffer(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        let bytes = output.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn search_diagnostics_distinguish_empty_rejected_and_ambiguous_matches() {
        let select = |items: Vec<(&'static str, &'static str)>| {
            select_unique_match(
                items,
                |item| CandidateCheck {
                    artist: identity_check("Artist", item.0),
                    title: Some(identity_check("Album", item.1)),
                },
                |item| format!("{:?}", item),
            )
        };
        let (matched, diagnostics) = select(vec![]);
        assert!(matched.is_none());
        assert_eq!(diagnostics.outcome(), "search_empty");
        let (matched, diagnostics) =
            select(vec![("Other", "Other"), ("", "Album"), ("Artist", "")]);
        assert!(matched.is_none());
        assert_eq!(diagnostics.outcome(), "no_verified_match");
        assert_eq!((diagnostics.candidates, diagnostics.exact_matches), (3, 0));
        assert_eq!(
            (diagnostics.artist_mismatch, diagnostics.title_mismatch),
            (1, 1)
        );
        assert_eq!(
            (diagnostics.missing_artist, diagnostics.missing_title),
            (1, 1)
        );
        let (matched, diagnostics) = select(vec![("Other", "Album"), ("Artist", "Album")]);
        assert_eq!(matched, Some(("Artist", "Album")));
        assert_eq!(diagnostics.outcome(), "matched");
        let (matched, diagnostics) = select(vec![
            ("Artist", "Album"),
            ("artist", "album"),
            ("ARTIST", "ALBUM"),
        ]);
        assert!(matched.is_none());
        assert_eq!(diagnostics.outcome(), "ambiguous_match");
        assert_eq!(diagnostics.exact_matches, 3);
    }

    #[test]
    fn provider_search_logs_explain_rejection_and_bound_candidate_samples() {
        let long_name = "東京".repeat(200);
        let payload = serde_json::json!({"album": [
            {"strArtist": "Other performer", "strAlbum": "Album"},
            {"strArtist": "Artist", "strAlbum": "Album (Live)"},
            {"strAlbum": "Album"},
            {"strArtist": long_name, "strAlbum": "Different"}
        ]});
        let log = capture_logs(|| {
            let response: TheAudioDbAlbumResponse = serde_json::from_value(payload).unwrap();
            assert!(response.into_match("Artist", "Album").is_none());
        });
        for expected in [
            "metadata_search_result",
            "theaudiodb",
            "no_verified_match",
            "candidates=4",
            "exact_matches=0",
            "missing_artist=1",
            "artist_mismatch=2",
            "title_mismatch=2",
            "Other performer",
            "Album (Live)",
        ] {
            assert!(log.contains(expected), "missing {expected}: {log}");
        }
        // Only three rejected identities are included; Unicode truncation is safe.
        assert!(!log.contains(&long_name));
        assert_eq!(diagnostic_text(&long_name).chars().count(), 161);
        assert!(diagnostic_text(&long_name).ends_with('…'));
    }

    #[test]
    fn album_lookup_logs_separate_no_match_from_verified_missing_description() {
        let titles = vec!["Album (Deluxe)".to_owned(), "Album".to_owned()];
        let empty_log =
            capture_logs(|| log_theaudiodb_album_incomplete("Artist", &titles[0], &titles, None));
        assert!(empty_log.contains("no_verified_album_match"));
        assert!(!empty_log.contains("matched_record_missing_description"));
        let partial = ExternalMetadata {
            genres: vec!["Rock".to_owned()],
            ..ExternalMetadata::default()
        };
        let partial_log = capture_logs(|| {
            log_theaudiodb_album_incomplete("Artist", &titles[0], &titles, Some(&partial))
        });
        assert!(partial_log.contains("matched_record_missing_description"));
        assert!(!partial_log.contains("no_verified_album_match"));
        assert!(partial_log.contains("Album (Deluxe)"));
    }

    #[test]
    fn musicbrainz_detail_diagnostics_identify_validation_failures_and_empty_genres() {
        for (field, value, reason) in [
            (
                "id",
                serde_json::json!(OTHER_MBID),
                "detail_mbid_mismatch_or_missing",
            ),
            (
                "title",
                serde_json::json!("The Four Seasons (Live)"),
                "detail_title_mismatch_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([]),
                "detail_artist_mismatch_or_missing",
            ),
        ] {
            let mut payload = album_detail();
            payload[field] = value;
            let detail = serde_json::from_value(payload).unwrap();
            assert_eq!(
                musicbrainz_album_detail_rejection(
                    MBID,
                    "Janine Jansen",
                    "The Four Seasons",
                    &detail
                ),
                Some(reason)
            );
        }
        let detail = serde_json::from_value(album_detail()).unwrap();
        assert_eq!(
            musicbrainz_album_detail_rejection(MBID, "Janine Jansen", "The Four Seasons", &detail),
            None
        );
        let log = capture_logs(|| {
            log_musicbrainz_verified_fields(
                "album",
                "Janine Jansen",
                Some("The Four Seasons"),
                MBID,
                None,
            )
        });
        assert!(log.contains("verified_record_missing_genres"));
        assert!(log.contains("description_supported=false"));
        assert!(log.contains("genre_count=0"));
    }

    #[test]
    fn request_error_diagnostics_strip_theaudiodb_credential_urls() {
        let url =
            "https://www.theaudiodb.com/api/v1/json/private-test-key/searchalbum.php?s=Artist";
        let error = Client::new()
            .get(url)
            .header("x-test", "\n")
            .build()
            .unwrap_err();
        let safe = request_error("album_search_send", error);
        assert!(safe.starts_with("album_search_send: request:"));
        assert!(!safe.contains("private-test-key"));
        assert!(!safe.contains("https://"));
    }

    #[tokio::test]
    async fn independent_providers_overlap_while_each_provider_and_results_keep_order() {
        let sources: Vec<_> = [
            Provider::TheAudioDb,
            Provider::MusicBrainz,
            Provider::TheAudioDb,
            Provider::MusicBrainz,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, provider)| ExternalSource {
            provider,
            api_key: Some(index.to_string()),
            user_agent: None,
            timeout: Duration::from_secs(1),
        })
        .collect();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let completed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let results = tokio::time::timeout(
            Duration::from_secs(1),
            fetch_sources(&sources, |source| {
                let barrier = barrier.clone();
                let completed = completed.clone();
                Box::pin(async move {
                    let marker = source.api_key.as_ref().unwrap().clone();
                    // Both providers must have begun before either can finish.
                    // The second entries cannot begin before their first entry.
                    if marker == "0" || marker == "1" {
                        barrier.wait().await;
                    }
                    completed.lock().push(marker.clone());
                    Ok(Some(ExternalMetadata {
                        summary: Some(marker),
                        ..ExternalMetadata::default()
                    }))
                })
            }),
        )
        .await
        .expect("different providers should overlap");
        let summaries: Vec<_> = results
            .into_iter()
            .map(|result| result.unwrap().unwrap().summary.unwrap())
            .collect();
        assert_eq!(summaries, ["0", "1", "2", "3"]);
        let completed = completed.lock();
        for (first, second) in [("0", "2"), ("1", "3")] {
            assert!(
                completed.iter().position(|value| value == first)
                    < completed.iter().position(|value| value == second)
            );
        }
    }

    #[tokio::test]
    async fn provider_overlap_preserves_errors_and_empty_sources() {
        let results = fetch_sources(&[], |_| Box::pin(async { unreachable!() })).await;
        assert!(results.is_empty());
        let sources = [ExternalSource {
            provider: Provider::MusicBrainz,
            api_key: None,
            user_agent: None,
            timeout: Duration::from_secs(1),
        }];
        let results = fetch_sources(&sources, |_| {
            Box::pin(async { Err("provider unavailable".to_owned()) })
        })
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap_err(), "provider unavailable");
    }

    #[test]
    fn collect_genres_splits_bullets_and_mojibake() {
        let genres = collect_genres(&[Some(
            "Rock \u{2022} Alternative Rock \u{00C2}\u{20AC}\u{00A2} Nu Metal".to_string(),
        )]);

        assert_eq!(genres, vec!["Rock", "Alternative Rock", "Nu Metal"]);
    }

    #[test]
    fn clean_text_field_uses_localized_fallback() {
        let mut fields = HashMap::new();
        fields.insert(
            "strBiographyDE".to_string(),
            Value::String(" Deutsche Bio ".to_string()),
        );

        assert_eq!(
            clean_text_field(None, &fields, "strBiography").as_deref(),
            Some("Deutsche Bio")
        );
        assert_eq!(
            clean_text_field(Some(" English Bio ".to_string()), &fields, "strBiography").as_deref(),
            Some("English Bio")
        );
    }

    #[test]
    fn album_title_candidates_trim_and_remove_release_qualifiers() {
        for (title, stripped) in [
            ("Californication ", "Californication"),
            ("Chopin : Nocturnes (1999 remastered)", "Chopin : Nocturnes"),
            ("System Of A Down (Deluxe)", "System Of A Down"),
            ("Automaton (Hi-Res Version)", "Automaton"),
            ("Unseen LP7 (Deluxe Edition) (Explicit)", "Unseen LP7"),
        ] {
            let candidates = album_title_candidates(title);
            assert_eq!(candidates[0], title.trim());
            assert!(candidates.iter().any(|value| value == stripped));
            assert!(candidates.len() <= 8);
            assert!(crate::album_lookup::titles_equivalent(title, stripped));
        }
    }

    #[test]
    fn album_title_candidates_preserve_distinct_performances() {
        for title in [
            "The Four Seasons (Live 2005)",
            "The Four Seasons (Acoustic Version)",
            "The Four Seasons (2005)",
            "The Four Seasons (Live Deluxe Edition)",
            "The Four Seasons (Remix Edition)",
            "The Four Seasons (New Recording Edition)",
        ] {
            let candidates = album_title_candidates(title);
            assert_eq!(candidates[0], title);
            assert!(candidates.iter().all(|candidate| {
                crate::album_lookup::titles_equivalent(title, candidate)
                    && !crate::album_lookup::titles_equivalent(candidate, "The Four Seasons")
            }));
        }
    }

    #[test]
    fn theaudiodb_identity_bridge_accepts_alias_only_for_returned_verified_group() {
        let group_id = "e248e931-c26c-4c94-aba2-2f89ce583901";
        let payload = serde_json::json!({"album": [{
            "strMusicBrainzID": group_id,
            "strArtist": "Tyler, The Creator",
            "strAlbum": "Scum Fuck Flower Boy",
            "strDescriptionEN": "A sanitized fixture description.",
            "strGenre": "Rap"
        }]});
        let search: TheAudioDbAlbumResponse = serde_json::from_value(payload.clone()).unwrap();
        assert!(search
            .into_match("Tyler, The Creator", "Flower Boy")
            .is_none());
        let response = serde_json::from_value(payload).unwrap();
        let metadata = theaudiodb_album_identity_metadata(response, group_id).unwrap();
        assert_eq!(
            metadata.summary.as_deref(),
            Some("A sanitized fixture description.")
        );
        assert_eq!(metadata.genres, ["Rap"]);
    }

    #[test]
    fn theaudiodb_identity_bridge_rejects_wrong_missing_and_ambiguous_group_records() {
        let group_id = "e248e931-c26c-4c94-aba2-2f89ce583901";
        let valid = serde_json::json!({
            "strMusicBrainzID": group_id, "strArtist": "Source Artist", "strAlbum": "Source Alias",
            "strDescriptionEN": "A sanitized fixture description."
        });
        for (field, value) in [
            ("strMusicBrainzID", serde_json::Value::Null),
            ("strMusicBrainzID", serde_json::json!("bad-id")),
            (
                "strMusicBrainzID",
                serde_json::json!("00000000-0000-0000-0000-000000000000"),
            ),
            (
                "strMusicBrainzID",
                serde_json::json!("11111111-1111-1111-1111-111111111111"),
            ),
            ("strMusicBrainzID", serde_json::json!(42)),
            ("strArtist", serde_json::Value::Null),
            ("strArtist", serde_json::json!("  ")),
            ("strAlbum", serde_json::Value::Null),
            ("strAlbum", serde_json::json!("  ")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let response = serde_json::from_value(serde_json::json!({"album": [invalid]})).unwrap();
            assert!(
                theaudiodb_album_identity_metadata(response, group_id).is_none(),
                "{field}"
            );
        }
        let mut other_alias = valid.clone();
        other_alias["strArtist"] = serde_json::json!("Different Source Billing");
        other_alias["strAlbum"] = serde_json::json!("Different Source Alias");
        let log = capture_logs(|| {
            let response =
                serde_json::from_value(serde_json::json!({"album": [valid, other_alias]})).unwrap();
            assert!(theaudiodb_album_identity_metadata(response, group_id).is_none());
        });
        assert!(log.contains("ambiguous_provider_group_records"));
        let empty = serde_json::from_value(serde_json::json!({"album": null})).unwrap();
        assert!(theaudiodb_album_identity_metadata(empty, group_id).is_none());
    }

    #[test]
    fn theaudiodb_identity_bridge_keeps_missing_description_partial() {
        let group_id = "8d31a2ad-7fd3-3490-814a-d489dd46feee";
        let log = capture_logs(|| {
            let response = serde_json::from_value(serde_json::json!({"album": [{
                "strMusicBrainzID": group_id, "strArtist": "Antonio Vivaldi",
                "strAlbum": "The Four Seasons", "strDescriptionEN": "  ", "strGenre": "Composer"
            }]}))
            .unwrap();
            let metadata = theaudiodb_album_identity_metadata(response, group_id).unwrap();
            assert!(metadata.summary.is_none());
            assert_eq!(metadata.genres, ["Composer"]);
        });
        assert!(log.contains("verified_record_missing_description"));
        assert!(log.contains("description_present=false"));
    }

    #[test]
    fn captured_seventeen_album_group_bridges_replay_availability_and_reject_changed_ids() {
        // This exercises the provider route after separate local programme proof.
        // It does not claim that title discovery or runtime resolution proves all 17 albums.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/metadata_lookup/theaudiodb_group_bridge.json"
        ))
        .unwrap();
        let placeholder = fixture["description_placeholder"].as_str().unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 17);
        let mut records_present = 0;
        let mut descriptions_present = 0;
        for row in cases {
            let case = row["case"].as_str().unwrap();
            let group_id = row["requested_release_group_id"].as_str().unwrap();
            assert_eq!(
                musicbrainz_id(Some(group_id)).as_deref(),
                Some(group_id),
                "{case}"
            );
            assert_ne!(
                group_id, "61d95895-b529-3b3c-bebf-741854c086e8",
                "wrong Bernstein anthology"
            );
            assert_ne!(
                group_id, "921f93e3-301d-4e61-84a2-29b27460b510",
                "wrong 14-track Rubinstein programme"
            );
            assert_eq!(row["captured_http_status"].as_u64(), Some(200), "{case}");

            let payload = row["payload"].clone();
            for album in payload["album"].as_array().unwrap() {
                let fields = album.as_object().unwrap();
                assert!(
                    fields.keys().all(|key| matches!(
                        key.as_str(),
                        "strArtist"
                            | "strAlbum"
                            | "strMusicBrainzID"
                            | "strGenre"
                            | "strStyle"
                            | "strDescription"
                    )),
                    "unexpected captured field in {case}"
                );
                if let Some(description) = fields.get("strDescription") {
                    assert_eq!(description.as_str(), Some(placeholder), "{case}");
                    assert_eq!(
                        row["expected_description_present"].as_bool(),
                        Some(true),
                        "{case}"
                    );
                }
            }
            let response = serde_json::from_value(payload.clone()).unwrap();
            let metadata = theaudiodb_album_identity_metadata(response, group_id);
            let expected_present = row["expected_metadata_present"].as_bool().unwrap();
            let expected_description = row["expected_description_present"].as_bool().unwrap();
            assert_eq!(metadata.is_some(), expected_present, "{case}");
            let Some(metadata) = metadata else {
                assert!(!expected_description, "{case}");
                continue;
            };
            records_present += 1;
            descriptions_present += usize::from(metadata.summary.is_some());
            assert_eq!(metadata.summary.is_some(), expected_description, "{case}");
            assert_eq!(
                metadata.summary.as_deref(),
                expected_description.then_some(placeholder),
                "{case}"
            );
            let expected_genres: Vec<String> =
                serde_json::from_value(row["expected_genres"].clone()).unwrap();
            assert_eq!(metadata.genres, expected_genres, "{case}");

            // A provider title/billing alias is allowed only while its explicit
            // release-group ID remains the already verified ID.
            for replacement in [
                None,
                Some("not-a-provider-id"),
                Some("11111111-1111-1111-1111-111111111111"),
            ] {
                let mut changed = payload.clone();
                for album in changed["album"].as_array_mut().unwrap() {
                    let fields = album.as_object_mut().unwrap();
                    if let Some(id) = replacement {
                        fields.insert("strMusicBrainzID".to_string(), serde_json::json!(id));
                    } else {
                        fields.remove("strMusicBrainzID");
                    }
                }
                let response = serde_json::from_value(changed).unwrap();
                assert!(
                    theaudiodb_album_identity_metadata(response, group_id).is_none(),
                    "{case}: {replacement:?}"
                );
            }
            let mut duplicate = payload;
            let albums = duplicate["album"].as_array_mut().unwrap();
            albums.push(albums[0].clone());
            let response = serde_json::from_value(duplicate).unwrap();
            assert!(
                theaudiodb_album_identity_metadata(response, group_id).is_none(),
                "ambiguous {case}"
            );
        }
        assert_eq!(records_present, 14);
        assert_eq!(descriptions_present, 12);
        let original = cases
            .iter()
            .find(|row| row["case"] == "eminem-murdered")
            .unwrap();
        let side_b = cases
            .iter()
            .find(|row| row["case"] == "eminem-side-b")
            .unwrap();
        assert_eq!(
            side_b["requested_release_group_id"],
            original["requested_release_group_id"]
        );
        assert_eq!(side_b["payload"], original["payload"]);
        assert_eq!(side_b["captured_request_case"], "eminem-murdered");
    }

    fn cached_group_identity_fixture() -> AlbumSourceIdentity {
        AlbumSourceIdentity {
            schema_version: library::ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION,
            context_fingerprint: "sanitized-local-program-fingerprint".to_string(),
            musicbrainz_release_id: "33333333-3333-3333-3333-333333333333".to_string(),
            musicbrainz_release_group_id: "44444444-4444-4444-4444-444444444444".to_string(),
            canonical_title: "Unseen LP2".to_string(),
            artist_names: vec!["First Artist".to_string(), "Second Artist".to_string()],
            artist_ids: vec![
                "55555555-5555-5555-5555-555555555555".to_string(),
                "66666666-6666-6666-6666-666666666666".to_string(),
            ],
            verified_at_ms: 1,
            evidence: "sanitized complete-program fixture".to_string(),
        }
    }

    fn cached_group_detail_fixture(identity: &AlbumSourceIdentity) -> serde_json::Value {
        serde_json::json!({
            "id": identity.musicbrainz_release_group_id,
            "title": "Unseen LP 2 (Explicit)",
            "artist-credit": [
                {"name": "First Artist", "artist": {"id": identity.artist_ids[0], "name": "First Artist"}},
                {"name": "Second Artist", "artist": {"id": identity.artist_ids[1], "name": "Second Artist"}}
            ],
            "genres": [{"name": "Rock"}]
        })
    }

    #[test]
    fn cached_musicbrainz_identity_revalidates_group_title_and_ordered_artist_ids() {
        let identity = cached_group_identity_fixture();
        let valid = cached_group_detail_fixture(&identity);
        let detail = serde_json::from_value(valid.clone()).unwrap();
        assert_eq!(cached_musicbrainz_group_rejection(&identity, &detail), None);
        for (field, value, reason) in [
            (
                "id",
                serde_json::Value::Null,
                "detail_mbid_mismatch_or_missing",
            ),
            (
                "id",
                serde_json::json!("77777777-7777-7777-7777-777777777777"),
                "detail_mbid_mismatch_or_missing",
            ),
            (
                "title",
                serde_json::Value::Null,
                "verified_group_title_changed_or_missing",
            ),
            (
                "title",
                serde_json::json!("Unseen LP3"),
                "verified_group_title_changed_or_missing",
            ),
            (
                "title",
                serde_json::json!("Unseen LP2 (Live)"),
                "verified_group_title_changed_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([]),
                "verified_group_artist_ids_changed_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([
                    {"artist": {"id": identity.artist_ids[0]}}, {"artist": {"name": "Second Artist"}}
                ]),
                "verified_group_artist_ids_changed_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([
                    {"artist": {"id": identity.artist_ids[1]}}, {"artist": {"id": identity.artist_ids[0]}}
                ]),
                "verified_group_artist_ids_changed_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([
                    {"artist": {"id": identity.artist_ids[0]}}
                ]),
                "verified_group_artist_ids_changed_or_missing",
            ),
            (
                "artist-credit",
                serde_json::json!([
                    {"artist": {"id": identity.artist_ids[0]}},
                    {"artist": {"id": "77777777-7777-7777-7777-777777777777"}}
                ]),
                "verified_group_artist_ids_changed_or_missing",
            ),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let detail = serde_json::from_value(invalid).unwrap();
            assert_eq!(
                cached_musicbrainz_group_rejection(&identity, &detail),
                Some(reason),
                "{field}"
            );
        }
    }

    #[test]
    fn identity_matching_accepts_formatting_but_not_different_names() {
        assert!(identity_matches("  JANINE JANSEN ", "Janine Jansen"));
        assert!(identity_matches("Sigur Rós", "Sigur Ro\u{301}s"));
        assert!(identity_matches("Chopin : Nocturnes", "Chopin: Nocturnes"));
        assert!(!identity_matches("Janine Jansen", "Jacques Loussier Trio"));
        assert!(!identity_matches("Janine Jansen", "Jamie Jansen"));
        assert!(!identity_matches("The Four Seasons", "Four Seasons"));
        assert!(!identity_matches("", ""));
        assert!(!identity_matches("...", "???"));
    }

    #[test]
    fn theaudiodb_album_search_requires_artist_and_title_identity() {
        let payload = serde_json::json!({"album": [
            {"strArtist": "Jacques Loussier Trio", "strAlbum": "The Four Seasons", "strGenre": "Jazz"},
            {"strArtist": "Janine Jansen", "strAlbum": "The Four Seasons", "strGenre": "Classical"}
        ]});
        let response: TheAudioDbAlbumResponse = serde_json::from_value(payload.clone()).unwrap();
        let album = response
            .into_match("Janine Jansen", "The Four Seasons")
            .unwrap();
        assert_eq!(album.genre.as_deref(), Some("Classical"));
        let response: TheAudioDbAlbumResponse = serde_json::from_value(payload).unwrap();
        assert!(response
            .into_match("Janine Jansen", "The Four Seasons (Live)")
            .is_none());
        for album in [
            serde_json::json!({"strArtist": "Jacques Loussier Trio", "strAlbum": "The Four Seasons"}),
            serde_json::json!({"strAlbum": "The Four Seasons"}),
            serde_json::json!({"strArtist": "Janine Jansen"}),
        ] {
            let response: TheAudioDbAlbumResponse =
                serde_json::from_value(serde_json::json!({"album": [album]})).unwrap();
            assert!(response
                .into_match("Janine Jansen", "The Four Seasons")
                .is_none());
        }
    }

    #[test]
    fn theaudiodb_artist_search_rejects_wrong_missing_and_ambiguous_names() {
        let payload = serde_json::json!({"artists": [
            {"strArtist": "Jacques Loussier Trio", "strBiographyEN": "Wrong artist"},
            {"strArtist": "Janine Jansen", "strBiographyEN": "Violinist"}
        ]});
        let response: TheAudioDbArtistResponse = serde_json::from_value(payload).unwrap();
        assert_eq!(
            response.into_match("Janine Jansen").unwrap().bio.as_deref(),
            Some("Violinist")
        );
        for artists in [
            serde_json::json!([{"strArtist": "Jacques Loussier Trio"}]),
            serde_json::json!([{"strBiographyEN": "Unidentified artist"}]),
            serde_json::json!([{"strArtist": "Janine Jansen"}, {"strArtist": "Janine Jansen"}]),
        ] {
            let response: TheAudioDbArtistResponse =
                serde_json::from_value(serde_json::json!({"artists": artists})).unwrap();
            assert!(response.into_match("Janine Jansen").is_none());
        }
    }

    #[test]
    fn musicbrainz_album_search_rejects_another_performance_of_the_same_work() {
        let wrong = serde_json::json!({
            "title": "The Four Seasons", "score": 100,
            "artist-credit": [{"name": "Jacques Loussier Trio", "artist": {"name": "Jacques Loussier Trio"}}],
            "tags": [{"name": "Jazz"}]
        });
        let correct = serde_json::json!({
            "title": "The Four Seasons", "score": 90,
            "artist-credit": [
                {"name": "Antonio Vivaldi", "joinphrase": "; ", "artist": {"name": "Antonio Vivaldi"}},
                {"name": "Janine Jansen", "artist": {"name": "Janine Jansen"}}
            ],
            "tags": [{"name": "Classical"}]
        });
        let response: MusicBrainzReleaseGroupResponse = serde_json::from_value(
            serde_json::json!({"release-groups": [wrong.clone(), correct.clone()]}),
        )
        .unwrap();
        assert_eq!(
            response
                .into_match("Janine Jansen", "The Four Seasons")
                .unwrap()
                .artist_credit[1]
                .name
                .as_deref(),
            Some("Janine Jansen")
        );
        for groups in [
            serde_json::json!([wrong]),
            serde_json::json!([correct.clone(), correct]),
            serde_json::json!([{"title": "The Four Seasons"}]),
            serde_json::json!([{"artist-credit": [{"name": "Janine Jansen"}]}]),
        ] {
            let response: MusicBrainzReleaseGroupResponse =
                serde_json::from_value(serde_json::json!({"release-groups": groups})).unwrap();
            assert!(response
                .into_match("Janine Jansen", "The Four Seasons")
                .is_none());
        }
    }

    #[test]
    fn musicbrainz_credit_supports_canonical_and_combined_artist_names() {
        let credits: Vec<MusicBrainzArtistCredit> = serde_json::from_value(serde_json::json!([
            {"name": "First Artist", "joinphrase": " & ", "artist": {"name": "Canonical First Artist"}},
            {"name": "Second Artist", "artist": {"name": "Second Artist"}}
        ])).unwrap();
        assert!(musicbrainz_credit_matches(
            "Canonical First Artist",
            &credits
        ));
        assert!(musicbrainz_credit_matches(
            "First Artist & Second Artist",
            &credits
        ));
        assert!(musicbrainz_credit_matches(
            "Canonical First Artist & Second Artist",
            &credits
        ));
        assert!(!musicbrainz_credit_matches("Unrelated Artist", &credits));
    }

    #[test]
    fn musicbrainz_artist_search_requires_a_unique_exact_name() {
        let payload = serde_json::json!({"artists": [
            {"name": "Jamie Jansen", "score": 100, "disambiguation": "Different artist"},
            {"name": "Janine Jansen", "score": 90, "disambiguation": "Violinist"}
        ]});
        let response: MusicBrainzArtistResponse = serde_json::from_value(payload).unwrap();
        assert_eq!(
            response
                .into_match("Janine Jansen")
                .unwrap()
                .name
                .as_deref(),
            Some("Janine Jansen")
        );
        for artists in [
            serde_json::json!([{"name": "Jamie Jansen"}]),
            serde_json::json!([{"disambiguation": "Unidentified artist"}]),
            serde_json::json!([{"name": "Janine Jansen"}, {"name": "JANINE JANSEN"}]),
        ] {
            let response: MusicBrainzArtistResponse =
                serde_json::from_value(serde_json::json!({"artists": artists})).unwrap();
            assert!(response.into_match("Janine Jansen").is_none());
        }
    }

    #[test]
    fn musicbrainz_query_phrases_escape_quotes_and_backslashes() {
        assert_eq!(lucene_phrase("AC/DC"), "\"AC/DC\"");
        assert_eq!(
            lucene_phrase("A \"quoted\" \\ name"),
            "\"A \\\"quoted\\\" \\\\ name\""
        );
    }

    const MBID: &str = "11111111-1111-1111-1111-111111111111";
    const OTHER_MBID: &str = "22222222-2222-2222-2222-222222222222";

    fn artist_search() -> MusicBrainzArtistResponse {
        serde_json::from_value(serde_json::json!({"artists": [{
            "id": MBID, "name": "Janine Jansen",
            "tags": [{"name": "seen live"}], "disambiguation": "Dutch violinist"
        }]}))
        .unwrap()
    }

    fn album_detail() -> serde_json::Value {
        serde_json::json!({
            "id": MBID, "title": "The Four Seasons",
            "artist-credit": [{"name": "Janine Jansen", "artist": {"name": "Janine Jansen"}}],
            "genres": [{"name": "classical"}], "tags": [{"name": "favorite albums"}]
        })
    }

    fn album_search() -> MusicBrainzReleaseGroupResponse {
        serde_json::from_value(serde_json::json!({"release-groups": [album_detail()]})).unwrap()
    }

    #[tokio::test]
    async fn musicbrainz_artist_genres_come_from_verified_detail_and_never_from_tags_or_comments() {
        let result = verified_musicbrainz_artist_metadata(artist_search(), "Janine Jansen", |id| {
            assert_eq!(id, MBID);
            std::future::ready(Ok(serde_json::from_value(serde_json::json!({
                "id": MBID, "name": "Janine Jansen", "disambiguation": "Dutch violinist",
                "genres": [{"name": "classical"}, {"name": "Classical"}, {"name": "funk / soul"}],
                "tags": [{"name": "seen live"}, {"name": "female vocalist"}]
            }))
            .unwrap()))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.genres, ["classical", "funk / soul"]);
        assert!(result.summary.is_none());

        let empty = verified_musicbrainz_artist_metadata(artist_search(), "Janine Jansen", |_| {
            std::future::ready(Ok(serde_json::from_value(serde_json::json!({
                "id": MBID, "name": "Janine Jansen", "disambiguation": "Dutch violinist",
                "tags": [{"name": "seen live"}]
            }))
            .unwrap()))
        })
        .await
        .unwrap();
        assert!(empty.is_none());
    }

    #[tokio::test]
    async fn musicbrainz_missing_or_invalid_search_ids_never_request_detail() {
        for id in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("not-an-mbid"),
            serde_json::json!("../artist"),
            serde_json::json!(uuid::Uuid::nil().to_string()),
        ] {
            let search = serde_json::from_value(serde_json::json!({"artists": [{
                "id": id, "name": "Janine Jansen", "tags": [{"name": "classical"}]
            }]}))
            .unwrap();
            assert!(
                verified_musicbrainz_artist_metadata(search, "Janine Jansen", |_| {
                    std::future::ready(Err("detail should not be requested".to_owned()))
                })
                .await
                .unwrap()
                .is_none()
            );

            let mut detail = album_detail();
            detail["id"] = id;
            let search =
                serde_json::from_value(serde_json::json!({"release-groups": [detail]})).unwrap();
            assert!(verified_musicbrainz_album_metadata(
                search,
                "Janine Jansen",
                "The Four Seasons",
                |_| { std::future::ready(Err("detail should not be requested".to_owned())) }
            )
            .await
            .unwrap()
            .is_none());
        }
    }

    #[tokio::test]
    async fn musicbrainz_artist_lookup_rejects_changed_id_or_name_and_propagates_failure() {
        for detail in [
            serde_json::json!({"id": OTHER_MBID, "name": "Janine Jansen", "genres": [{"name": "jazz"}]}),
            serde_json::json!({"id": MBID, "name": "Jamie Jansen", "genres": [{"name": "jazz"}]}),
            serde_json::json!({"name": "Janine Jansen", "genres": [{"name": "jazz"}]}),
        ] {
            assert!(
                verified_musicbrainz_artist_metadata(artist_search(), "Janine Jansen", |_| {
                    std::future::ready(Ok(serde_json::from_value(detail.clone()).unwrap()))
                })
                .await
                .unwrap()
                .is_none()
            );
        }
        assert_eq!(
            verified_musicbrainz_artist_metadata(artist_search(), "Janine Jansen", |_| {
                std::future::ready(Err("provider unavailable".to_owned()))
            })
            .await
            .unwrap_err(),
            "provider unavailable"
        );
    }

    #[tokio::test]
    async fn musicbrainz_album_lookup_revalidates_id_title_and_credits_before_using_genres() {
        let result = verified_musicbrainz_album_metadata(
            album_search(),
            "Janine Jansen",
            "The Four Seasons",
            |id| {
                assert_eq!(id, MBID);
                std::future::ready(Ok(serde_json::from_value(album_detail()).unwrap()))
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.genres, ["classical"]);
        assert!(result.summary.is_none());

        for (field, value) in [
            ("id", serde_json::json!(OTHER_MBID)),
            ("title", serde_json::json!("The Four Seasons (Live)")),
            (
                "artist-credit",
                serde_json::json!([{"name": "Jacques Loussier Trio"}]),
            ),
            ("artist-credit", serde_json::json!([])),
        ] {
            let mut detail = album_detail();
            detail[field] = value;
            assert!(verified_musicbrainz_album_metadata(
                album_search(),
                "Janine Jansen",
                "The Four Seasons",
                |_| { std::future::ready(Ok(serde_json::from_value(detail.clone()).unwrap())) }
            )
            .await
            .unwrap()
            .is_none());
        }

        let mut detail = album_detail();
        detail.as_object_mut().unwrap().remove("genres");
        assert!(verified_musicbrainz_album_metadata(
            album_search(),
            "Janine Jansen",
            "The Four Seasons",
            |_| { std::future::ready(Ok(serde_json::from_value(detail.clone()).unwrap())) }
        )
        .await
        .unwrap()
        .is_none());
        assert_eq!(
            verified_musicbrainz_album_metadata(
                album_search(),
                "Janine Jansen",
                "The Four Seasons",
                |_| { std::future::ready(Err("provider unavailable".to_owned())) }
            )
            .await
            .unwrap_err(),
            "provider unavailable"
        );
    }

    #[tokio::test]
    async fn unknown_identity_does_not_request_external_metadata() {
        let client = Client::new();
        let config = ExternalConfig {
            sources: vec![ExternalSource {
                provider: Provider::MusicBrainz,
                api_key: None,
                user_agent: Some("test-agent".to_string()),
                timeout: Duration::ZERO,
            }],
        };
        for artist in ["", "Unknown", " UNKNOWN ARTIST "] {
            assert!(fetch_artist(&client, &config, artist)
                .await
                .unwrap()
                .is_none());
            assert!(fetch_album(&client, &config, artist, "The Four Seasons")
                .await
                .unwrap()
                .is_none());
        }
        assert!(fetch_album(&client, &config, "Janine Jansen", "...")
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn musicbrainz_genres_do_not_replace_existing_summary() {
        let mut base = ExternalMetadata {
            summary: Some("Full TheAudioDB biography".to_string()),
            genres: vec!["Rock".to_string()],
            ..ExternalMetadata::default()
        };
        let incoming = ExternalMetadata {
            summary: Some("MusicBrainz disambiguation".to_string()),
            genres: vec!["Alternative".to_string()],
            ..ExternalMetadata::default()
        };

        merge_metadata(&mut base, incoming, Provider::MusicBrainz);

        assert_eq!(base.summary.as_deref(), Some("Full TheAudioDB biography"));
        assert_eq!(base.genres, vec!["Alternative"]);
    }

    #[test]
    fn summaries_fill_empty_values() {
        let mut base = ExternalMetadata::default();
        let incoming = ExternalMetadata {
            summary: Some("Fallback summary".to_string()),
            ..ExternalMetadata::default()
        };

        merge_metadata(&mut base, incoming, Provider::MusicBrainz);

        assert_eq!(base.summary.as_deref(), Some("Fallback summary"));
    }

    #[test]
    fn theaudiodb_summary_replaces_musicbrainz_fallback() {
        let mut base = ExternalMetadata {
            summary: Some("MusicBrainz disambiguation".to_string()),
            ..ExternalMetadata::default()
        };
        let incoming = ExternalMetadata {
            summary: Some("Full TheAudioDB biography".to_string()),
            ..ExternalMetadata::default()
        };

        merge_metadata(&mut base, incoming, Provider::TheAudioDb);

        assert_eq!(base.summary.as_deref(), Some("Full TheAudioDB biography"));
    }
}
