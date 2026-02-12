use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

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
    for source in &config.sources {
        let result = match source.provider {
            Provider::TheAudioDb => fetch_theaudiodb_artist(client, source, artist_name).await,
            Provider::MusicBrainz => fetch_musicbrainz_artist(client, source, artist_name).await,
        }?;
        if let Some(metadata) = result {
            merge_metadata(&mut combined, metadata, source.provider);
            found = true;
        }
    }
    if found {
        Ok(Some(combined))
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
    for source in &config.sources {
        let result = match source.provider {
            Provider::TheAudioDb => {
                fetch_theaudiodb_album(client, source, artist_name, album_title).await
            }
            Provider::MusicBrainz => {
                fetch_musicbrainz_album(client, source, artist_name, album_title).await
            }
        }?;
        if let Some(metadata) = result {
            merge_metadata(&mut combined, metadata, source.provider);
            found = true;
        }
    }
    if found {
        Ok(Some(combined))
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
            let url = "https://musicbrainz.org/ws/2/artist/?query=artist:radiohead&fmt=json&limit=1";
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
        url_escape(artist_name)
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
    let artist = match payload.artists.and_then(|mut items| items.pop()) {
        Some(artist) => artist,
        None => return Ok(None),
    };

    let summary = clean_text(artist.bio);
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
    let url = format!(
        "https://www.theaudiodb.com/api/v1/json/{}/searchalbum.php?s={}&a={}",
        api_key,
        url_escape(artist_name),
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
    let album = match payload.album.and_then(|mut items| items.pop()) {
        Some(album) => album,
        None => return Ok(None),
    };

    let summary = clean_text(album.description);
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
    let response = client
        .get(&url)
        .timeout(source.timeout)
        .header("User-Agent", user_agent)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("http {}", response.status()));
    }
    let payload = response
        .json::<MusicBrainzArtistResponse>()
        .await
        .map_err(|err| err.to_string())?;
    let artist = match payload.artists.and_then(|mut items| items.pop()) {
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
    let query = format!(
        "artist:{} releasegroup:{}",
        artist_name,
        album_title
    );
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/?query={}&fmt=json&limit=1&inc=tags",
        url_escape(&query)
    );
    let response = client
        .get(&url)
        .timeout(source.timeout)
        .header("User-Agent", user_agent)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("http {}", response.status()));
    }
    let payload = response
        .json::<MusicBrainzReleaseGroupResponse>()
        .await
        .map_err(|err| err.to_string())?;
    let album = match payload.release_groups.and_then(|mut items| items.pop()) {
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

fn collect_genres(values: &[Option<String>]) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if let Some(value) = clean_text(value.clone()) {
            for part in value.split(&[';', ',', '/', '|'][..]) {
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
        let name = tag.name.trim();
        if name.is_empty() {
            continue;
        }
        if !out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            out.push(name.to_string());
        }
    }
    out
}

fn merge_metadata(base: &mut ExternalMetadata, incoming: ExternalMetadata, provider: Provider) {
    let mut prefer = false;
    if matches!(provider, Provider::MusicBrainz) {
        prefer = true;
    }

    if let Some(summary) = incoming.summary {
        if prefer || base.summary.is_none() {
            base.summary = Some(summary);
        }
    }
    if !incoming.genres.is_empty() {
        if prefer || base.genres.is_empty() {
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
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*byte as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}
