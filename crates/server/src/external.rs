use std::{collections::HashMap, time::Duration};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::musicbrainz_rate_limit;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
    let mut combined = ExternalMetadata::default();
    let mut found = false;
    let mut last_error: Option<String> = None;
    for source in &config.sources {
        let result = match source.provider {
            Provider::TheAudioDb => fetch_theaudiodb_artist(client, source, artist_name).await,
            Provider::MusicBrainz => fetch_musicbrainz_artist(client, source, artist_name).await,
        };
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    "External artist metadata source {:?} failed for '{}': {}",
                    source.provider, artist_name, err
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

pub async fn fetch_album(
    client: &Client,
    config: &ExternalConfig,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let mut combined = ExternalMetadata::default();
    let mut found = false;
    let mut last_error: Option<String> = None;
    for source in &config.sources {
        let result = match source.provider {
            Provider::TheAudioDb => {
                fetch_theaudiodb_album(client, source, artist_name, album_title).await
            }
            Provider::MusicBrainz => {
                fetch_musicbrainz_album(client, source, artist_name, album_title).await
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    "External album metadata source {:?} failed for '{} - {}': {}",
                    source.provider, artist_name, album_title, err
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
                .map_err(|err| err.to_string())?;
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
                .map_err(|err| err.to_string())?;
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

#[derive(Deserialize)]
struct TheAudioDbArtist {
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

#[derive(Deserialize)]
struct TheAudioDbAlbum {
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
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("http {}", response.status()));
    }
    let payload = response
        .json::<TheAudioDbArtistResponse>()
        .await
        .map_err(|err| err.to_string())?;
    let artist = match payload.artists.and_then(|items| items.into_iter().next()) {
        Some(artist) => artist,
        None => return Ok(None),
    };

    let summary = clean_text_field(artist.bio, &artist.fields, "strBiography");
    if summary.is_none() {
        warn!(
            "TheAudioDB returned no artist description for '{}'",
            artist_name
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
        return Ok(None);
    }
    let mut best = None;
    for candidate in album_title_candidates(album_title) {
        let metadata =
            fetch_theaudiodb_album_candidate(client, source, api_key, artist_name, &candidate)
                .await?;
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
    if best
        .as_ref()
        .and_then(|metadata| metadata.summary.as_ref())
        .is_none()
    {
        warn!(
            "TheAudioDB returned no album description for '{} - {}'",
            artist_name,
            album_title.trim()
        );
    }
    Ok(best)
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
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("http {}", response.status()));
    }
    let payload = response
        .json::<TheAudioDbAlbumResponse>()
        .await
        .map_err(|err| err.to_string())?;
    let album = match payload.album.and_then(|items| items.into_iter().next()) {
        Some(album) => album,
        None => return Ok(None),
    };

    let summary = clean_text_field(album.description, &album.fields, "strDescription");
    let genres = collect_genres(&[album.genre, album.style]);
    Ok(Some(ExternalMetadata {
        summary,
        genres,
        logo_url: None,
        banner_url: None,
    }))
}

#[derive(Deserialize)]
struct MusicBrainzArtistResponse {
    artists: Option<Vec<MusicBrainzArtist>>,
}

#[derive(Deserialize)]
struct MusicBrainzArtist {
    disambiguation: Option<String>,
    tags: Option<Vec<MusicBrainzTag>>,
}

#[derive(Deserialize)]
struct MusicBrainzReleaseGroupResponse {
    #[serde(rename = "release-groups")]
    release_groups: Option<Vec<MusicBrainzReleaseGroup>>,
}

#[derive(Deserialize)]
struct MusicBrainzReleaseGroup {
    tags: Option<Vec<MusicBrainzTag>>,
}

#[derive(Deserialize)]
struct MusicBrainzTag {
    name: String,
}

async fn fetch_musicbrainz_artist(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let user_agent = source.user_agent.as_deref().unwrap_or("").trim();
    if user_agent.is_empty() {
        return Ok(None);
    }
    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query=artist:{}&fmt=json&limit=1&inc=tags",
        url_escape(artist_name)
    );
    let payload: MusicBrainzArtistResponse =
        fetch_musicbrainz_json(client, source, &url, user_agent).await?;
    let artist = match payload.artists.and_then(|items| items.into_iter().next()) {
        Some(artist) => artist,
        None => return Ok(None),
    };
    let summary = clean_text(artist.disambiguation);
    let genres = collect_tag_genres(artist.tags);
    Ok(Some(ExternalMetadata {
        summary,
        genres,
        logo_url: None,
        banner_url: None,
    }))
}

async fn fetch_musicbrainz_album(
    client: &Client,
    source: &ExternalSource,
    artist_name: &str,
    album_title: &str,
) -> Result<Option<ExternalMetadata>, String> {
    let user_agent = source.user_agent.as_deref().unwrap_or("").trim();
    if user_agent.is_empty() {
        return Ok(None);
    }
    let query = format!("artist:{} releasegroup:{}", artist_name, album_title);
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/?query={}&fmt=json&limit=1&inc=tags",
        url_escape(&query)
    );
    let payload: MusicBrainzReleaseGroupResponse =
        fetch_musicbrainz_json(client, source, &url, user_agent).await?;
    let album = match payload
        .release_groups
        .and_then(|items| items.into_iter().next())
    {
        Some(album) => album,
        None => return Ok(None),
    };
    let genres = collect_tag_genres(album.tags);
    Ok(Some(ExternalMetadata {
        summary: None,
        genres,
        logo_url: None,
        banner_url: None,
    }))
}

async fn fetch_musicbrainz_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    source: &ExternalSource,
    url: &str,
    user_agent: &str,
) -> Result<T, String> {
    let tries = 6usize;
    let mut last_error = String::from("request failed");

    for attempt in 0..tries {
        musicbrainz_rate_limit::wait_for_slot().await;
        let response = client
            .get(url)
            .timeout(source.timeout)
            .header("User-Agent", user_agent)
            .send()
            .await;

        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    match response.bytes().await {
                        Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                            Ok(payload) => return Ok(payload),
                            Err(err) => {
                                last_error = format_musicbrainz_decode_error(err, &bytes);
                            }
                        },
                        Err(err) => {
                            last_error = err.to_string();
                        }
                    }
                    if attempt + 1 == tries {
                        return Err(last_error);
                    }
                } else {
                    last_error = format!("http {status}");
                    if !(status.as_u16() == 429 || status.is_server_error()) || attempt + 1 == tries
                    {
                        return Err(last_error);
                    }
                }
            }
            Err(err) => {
                last_error = err.to_string();
                if attempt + 1 == tries {
                    return Err(last_error);
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(1000 + (attempt as u64 * 500))).await;
    }

    Err(last_error)
}

fn format_musicbrainz_decode_error(err: serde_json::Error, body: &[u8]) -> String {
    let snippet = String::from_utf8_lossy(body);
    let snippet = snippet.trim();
    if snippet.is_empty() {
        return format!("error decoding response body: {}", err);
    }
    let snippet = if snippet.len() > 240 {
        format!("{}...", &snippet[..240])
    } else {
        snippet.to_string()
    };
    format!("error decoding response body: {} | body={}", err, snippet)
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

fn collect_tag_genres(tags: Option<Vec<MusicBrainzTag>>) -> Vec<String> {
    let mut out = Vec::new();
    let tags = match tags {
        Some(tags) => tags,
        None => return out,
    };
    for tag in tags {
        add_genre_parts(&mut out, &tag.name);
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
    let mut out = Vec::new();
    let mut current = title.trim().to_string();
    add_unique_candidate(&mut out, &current);

    loop {
        let Some(stripped) = strip_trailing_release_qualifier(&current) else {
            break;
        };
        current = stripped;
        add_unique_candidate(&mut out, &current);
    }

    out
}

fn add_unique_candidate(out: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if !out
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(trimmed))
    {
        out.push(trimmed.to_string());
    }
}

fn strip_trailing_release_qualifier(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let (open, close) = match trimmed.chars().last()? {
        ')' => ('(', ')'),
        ']' => ('[', ']'),
        _ => return None,
    };
    let start = trimmed.rfind(open)?;
    let qualifier = trimmed[start + open.len_utf8()..trimmed.len() - close.len_utf8()].trim();
    if !is_release_qualifier(qualifier) {
        return None;
    }
    let stripped = trimmed[..start].trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

fn is_release_qualifier(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let digits = lower.chars().filter(|ch| ch.is_ascii_digit()).count();
    digits >= 4
        || lower.contains("remaster")
        || lower.contains("deluxe")
        || lower.contains("hi-res")
        || lower.contains("hi res")
        || lower.contains("version")
        || lower.contains("edition")
        || lower.contains("anniversary")
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

    use serde_json::Value;

    use super::*;

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
        assert_eq!(
            album_title_candidates("Californication "),
            vec!["Californication"]
        );
        assert_eq!(
            album_title_candidates("Chopin : Nocturnes (1999 remastered)"),
            vec!["Chopin : Nocturnes (1999 remastered)", "Chopin : Nocturnes"]
        );
        assert_eq!(
            album_title_candidates("System Of A Down (Deluxe)"),
            vec!["System Of A Down (Deluxe)", "System Of A Down"]
        );
        assert_eq!(
            album_title_candidates("Automaton (Hi-Res Version)"),
            vec!["Automaton (Hi-Res Version)", "Automaton"]
        );
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
