use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use common::{artist_identity_id, Album, Artist, Track};
use library::Library;
use metadata::read_tags;
use regex::Regex;
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use strsim::normalized_levenshtein;
use unicode_normalization::UnicodeNormalization;

use crate::musicbrainz_rate_limit;

const DEFAULT_MB_LIMIT: usize = 25;
const DEFAULT_MIN_TRACK_COUNT: usize = 2;
const DEFAULT_MIN_TRACK_RATIO: f32 = 0.25;

const CONNECTOR_ARTISTS: &[&str] = &[
    "&",
    "and",
    "feat",
    "feat.",
    "ft",
    "ft.",
    "featuring",
    "with",
];

const DEFAULT_ARTIST_NAME_MAP: &[(&str, &str)] = &[("ye", "Kanye West")];

#[derive(Clone, Copy, Debug)]
pub struct ResolveOptions {
    pub mb_limit: usize,
    pub min_track_count: usize,
    pub min_track_ratio: f32,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            mb_limit: DEFAULT_MB_LIMIT,
            min_track_count: DEFAULT_MIN_TRACK_COUNT,
            min_track_ratio: DEFAULT_MIN_TRACK_RATIO,
        }
    }
}

pub struct MusicBrainzAlbumArtistResolver<'a> {
    client: &'a Client,
    user_agent: &'a str,
    timeout: Duration,
    options: ResolveOptions,
}

impl<'a> MusicBrainzAlbumArtistResolver<'a> {
    pub fn new(
        client: &'a Client,
        user_agent: &'a str,
        timeout: Duration,
        options: ResolveOptions,
    ) -> Self {
        Self {
            client,
            user_agent,
            timeout,
            options,
        }
    }

    pub async fn resolve_album_artists(
        &self,
        library: &Library,
        album: &Album,
        tracks: &[Track],
        existing_artists: &[Artist],
    ) -> Result<Option<Vec<Artist>>, String> {
        if self.user_agent.trim().is_empty() {
            return Ok(None);
        }

        let local = build_local_album_info(library, album, tracks).await?;
        let base_name_map = default_artist_name_map();
        let resolved = match self.resolve_album_with_musicbrainz(&local).await? {
            Some(resolved) => resolved,
            None => return Ok(None),
        };

        let effective_name_map =
            build_effective_name_map(&base_name_map, Some(&resolved.auto_name_map));
        let album_artists = canonicalize_artist_list(&resolved.album_artists, &effective_name_map);
        let mut album_track_collaborators = Vec::with_capacity(local.track_artists.len());

        for (index, local_artist) in local.track_artists.iter().enumerate() {
            let release_artists = resolved
                .track_artists
                .get(index)
                .cloned()
                .unwrap_or_default();
            let collaborators = build_track_collaborators(
                local_artist,
                &album_artists,
                &release_artists,
                &effective_name_map,
            );
            album_track_collaborators.push(collaborators);
        }

        let selected = select_significant_album_artists(
            &album_artists,
            &album_track_collaborators,
            self.options.min_track_count,
            self.options.min_track_ratio,
        );
        let selected = rank_selected_album_artists(
            &selected,
            &local.track_artists,
            &album_track_collaborators,
            &effective_name_map,
        );
        if selected.is_empty() {
            return Ok(None);
        }

        let artists = materialize_artists(&selected, existing_artists, &effective_name_map);
        if artists.is_empty() {
            Ok(None)
        } else {
            Ok(Some(artists))
        }
    }

    async fn resolve_album_with_musicbrainz(
        &self,
        local: &LocalAlbumInfo,
    ) -> Result<Option<ResolvedRelease>, String> {
        let mut candidates = Vec::new();
        for query in
            build_release_queries(Some(local.artist_hint.as_str()), local.album_hint.as_str())
        {
            let results = self
                .search_releases(query.as_str(), self.options.mb_limit)
                .await?;
            if !results.is_empty() {
                candidates = results;
                break;
            }
        }

        if candidates.is_empty() {
            return Ok(None);
        }

        let mut best_score = -1.0f64;
        let mut best = None;
        for candidate in candidates {
            let Some(release_id) = candidate.id.as_deref() else {
                continue;
            };
            let release = match self.fetch_release(release_id).await {
                Ok(release) => release,
                Err(_) => continue,
            };

            let score = candidate_score(local, &candidate, &release);
            if score <= best_score {
                continue;
            }

            best_score = score;
            best = Some(ResolvedRelease {
                album_artists: collect_release_artists(&release),
                track_artists: collect_release_track_artists(&release),
                auto_name_map: infer_name_map_from_release(&release),
            });
        }

        Ok(best)
    }

    async fn search_releases(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ReleaseSearchCandidate>, String> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let url = Url::parse_with_params(
            "https://musicbrainz.org/ws/2/release",
            &[
                ("query", query),
                ("fmt", "json"),
                ("limit", limit.to_string().as_str()),
            ],
        )
        .map_err(|err| err.to_string())?;
        let payload: ReleaseSearchResponse = self.get_json(url).await?;
        Ok(payload.releases)
    }

    async fn fetch_release(&self, release_id: &str) -> Result<ReleaseDetail, String> {
        let url = Url::parse_with_params(
            &format!("https://musicbrainz.org/ws/2/release/{release_id}"),
            &[("fmt", "json"), ("inc", "recordings+artist-credits")],
        )
        .map_err(|err| err.to_string())?;
        self.get_json(url).await
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T, String> {
        let tries = 6usize;
        let mut last_error = String::from("request failed");
        for attempt in 0..tries {
            musicbrainz_rate_limit::wait_for_slot().await;
            let response = self
                .client
                .get(url.clone())
                .timeout(self.timeout)
                .header("User-Agent", self.user_agent)
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
                        if !(status.as_u16() == 429 || status.is_server_error())
                            || attempt + 1 == tries
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
}

#[derive(Clone, Debug)]
struct LocalAlbumInfo {
    album_hint: String,
    artist_hint: String,
    year_hint: Option<i32>,
    track_artists: Vec<String>,
    track_count: usize,
}

#[derive(Clone, Debug)]
struct ResolvedRelease {
    album_artists: Vec<String>,
    track_artists: Vec<Vec<String>>,
    auto_name_map: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseSearchResponse {
    #[serde(default)]
    releases: Vec<ReleaseSearchCandidate>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseSearchCandidate {
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_score")]
    score: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseDetail {
    title: Option<String>,
    date: Option<String>,
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<Value>>,
    #[serde(default)]
    media: Vec<ReleaseMedium>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseMedium {
    #[serde(rename = "track-count")]
    track_count: Option<usize>,
    #[serde(default)]
    tracks: Vec<ReleaseTrack>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseTrack {
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<Value>>,
    recording: Option<ReleaseRecording>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseRecording {
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<Value>>,
}

async fn build_local_album_info(
    library: &Library,
    album: &Album,
    tracks: &[Track],
) -> Result<LocalAlbumInfo, String> {
    let paths: Vec<Option<PathBuf>> = tracks
        .iter()
        .map(|track| library.resolve_relpath(&track.file_relpath))
        .collect();
    let track_count = tracks.len();
    let track_artists = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                path.and_then(|path| read_tags(&path).ok())
                    .and_then(|tag| tag.artist.or(tag.album_artist))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|err| err.to_string())?;

    let artist_hint = album
        .artist_names
        .first()
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let display = album.artist_display_name();
            if display.trim().is_empty() {
                None
            } else {
                Some(display)
            }
        })
        .unwrap_or_else(|| "Unknown Artist".to_string());

    Ok(LocalAlbumInfo {
        album_hint: album.title.clone(),
        artist_hint,
        year_hint: album.year,
        track_artists,
        track_count,
    })
}

fn default_artist_name_map() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (alias, canonical) in DEFAULT_ARTIST_NAME_MAP {
        out.insert(dedupe_key(alias), normalize_artist_display(canonical));
    }
    out
}

fn build_effective_name_map(
    base_map: &HashMap<String, String>,
    auto_map: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut effective = base_map.clone();
    let Some(auto_map) = auto_map else {
        return effective;
    };

    for (alias_raw, mapped_name) in auto_map {
        let alias_key = dedupe_key(alias_raw);
        if alias_key.is_empty() || effective.contains_key(&alias_key) {
            continue;
        }

        let resolved = canonicalize_artist_name(mapped_name, base_map);
        let resolved_key = dedupe_key(&resolved);
        if resolved.is_empty() || resolved_key.is_empty() || resolved_key == alias_key {
            continue;
        }

        effective.insert(alias_key, resolved);
    }

    effective
}

fn materialize_artists(
    selected_names: &[String],
    existing_artists: &[Artist],
    name_map: &HashMap<String, String>,
) -> Vec<Artist> {
    let mut existing_by_id = HashMap::new();
    let mut existing_lookup = HashMap::new();
    for artist in existing_artists {
        existing_by_id.insert(artist.id.clone(), artist.clone());
        let canonical = canonicalize_artist_name(&artist.name, name_map);
        let key = dedupe_key(&canonical);
        if key.is_empty() {
            continue;
        }
        existing_lookup.entry(key).or_insert_with(|| artist.clone());
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in selected_names {
        let canonical = canonicalize_artist_name(name, name_map);
        let key = dedupe_key(&canonical);
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }

        let canonical_id = artist_identity_id(canonical.as_str());
        if let Some(existing) = existing_by_id.get(&canonical_id) {
            let mut artist = existing.clone();
            artist.name = canonical;
            out.push(artist);
            continue;
        }

        let mut artist = existing_lookup.get(&key).cloned().unwrap_or(Artist {
            id: canonical_id.clone(),
            name: canonical.clone(),
            genres: Vec::new(),
            summary: None,
            logo_ref: None,
            banner_ref: None,
        });
        artist.id = canonical_id.clone();
        artist.name = canonical;

        existing_by_id.insert(canonical_id, artist.clone());
        existing_lookup.insert(key, artist.clone());
        out.push(artist);
    }

    out
}

fn build_release_query(artist: Option<&str>, release: &str) -> String {
    let mut parts = Vec::new();
    if let Some(release) = query_term("release", release) {
        parts.push(release);
    }
    if let Some(artist) = artist.and_then(|value| query_term("artist", value)) {
        parts.push(artist);
    }
    parts.join(" AND ")
}

fn build_release_queries(artist: Option<&str>, release: &str) -> Vec<String> {
    let mut queries = Vec::new();
    if let Some(query) = build_exact_release_query(artist, release) {
        queries.push(query);
    }
    if let Some(query) = artist
        .and_then(|artist| build_loose_artist_release_query(artist, release))
        .filter(|query| !queries.iter().any(|existing| existing == query))
    {
        queries.push(query);
    }
    if let Some(query) = build_exact_release_query(None, release)
        .filter(|query| !queries.iter().any(|existing| existing == query))
    {
        queries.push(query);
    }
    queries
}

fn build_exact_release_query(artist: Option<&str>, release: &str) -> Option<String> {
    let query = build_release_query(artist, release);
    if query.is_empty() {
        None
    } else {
        Some(query)
    }
}

fn build_loose_artist_release_query(artist: &str, release: &str) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(release) = query_term("release", release) {
        parts.push(release);
    }
    let artist_tokens = artist_search_tokens(artist);
    if !artist_tokens.is_empty() {
        let clause = artist_tokens
            .into_iter()
            .map(|token| format!("\"{}\"", token.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        parts.push(format!("artist:({clause})"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

fn artist_search_tokens(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for token in normalize_search_text(value).split_whitespace() {
        if token.is_empty() {
            continue;
        }
        let owned = token.to_string();
        if seen.insert(owned.clone()) {
            out.push(owned);
        }
    }
    out
}

fn query_term(field: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let escaped = trimmed.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!(r#"{field}:"{escaped}""#))
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

fn normalize_text(value: &str) -> String {
    let normalized = value
        .nfkc()
        .map(normalize_dash_char)
        .collect::<String>()
        .to_lowercase();
    collapse_whitespace(&normalized)
}

fn normalize_search_text(value: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in normalize_text(value).chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn normalize_artist_display(value: &str) -> String {
    let normalized = value.nfkc().map(normalize_dash_char).collect::<String>();
    normalized.trim().to_string()
}

fn normalize_dash_char(ch: char) -> char {
    match ch {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
        _ => ch,
    }
}

fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn dedupe_key(value: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in normalize_text(value).chars() {
        let mapped = match ch {
            '.' | '-' | '_' | '\'' | '`' | '\u{2019}' => ' ',
            _ => ch,
        };
        if mapped.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(mapped);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn has_alnum(value: &str) -> bool {
    value.chars().any(|ch| ch.is_alphanumeric())
}

fn canonicalize_artist_name(name: &str, name_map: &HashMap<String, String>) -> String {
    let cleaned = normalize_artist_display(name);
    if cleaned.is_empty() {
        return String::new();
    }
    name_map
        .get(&dedupe_key(&cleaned))
        .cloned()
        .unwrap_or(cleaned)
}

fn canonicalize_artist_list(names: &[String], name_map: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let canonical = canonicalize_artist_name(name, name_map);
        let key = dedupe_key(&canonical);
        if canonical.is_empty()
            || key.is_empty()
            || !has_alnum(&canonical)
            || is_connector(&canonical)
        {
            continue;
        }
        if seen.insert(key) {
            out.push(canonical);
        }
    }
    out
}

fn split_artist_regex() -> &'static Regex {
    static SPLIT_RE: OnceLock<Regex> = OnceLock::new();
    SPLIT_RE.get_or_init(|| {
        Regex::new(r"(?i)\s*(?:&|\band\b|feat\.?|ft\.?|featuring|with)\s+|\s*/\s*")
            .expect("valid split regex")
    })
}

fn split_artist_string(value: &str) -> Vec<String> {
    split_artist_regex()
        .split(value)
        .filter_map(|part| {
            let cleaned = normalize_artist_display(part);
            if cleaned.is_empty() || !has_alnum(&cleaned) {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

fn is_connector(name: &str) -> bool {
    CONNECTOR_ARTISTS
        .iter()
        .any(|connector| normalize_text(name) == normalize_text(connector))
}

fn extract_credit_names(credit: Option<&[Value]>) -> Vec<String> {
    let mut names = Vec::new();
    let Some(credit) = credit else {
        return names;
    };

    for item in credit {
        if let Some(raw) = item.as_str() {
            names.extend(split_artist_string(raw));
            continue;
        }

        let Some(object) = item.as_object() else {
            continue;
        };

        let name = object.get("name").and_then(Value::as_str).or_else(|| {
            object
                .get("artist")
                .and_then(Value::as_object)
                .and_then(|artist| artist.get("name"))
                .and_then(Value::as_str)
        });

        if let Some(name) = name {
            let cleaned = normalize_artist_display(name);
            if !cleaned.is_empty() && has_alnum(&cleaned) && !is_connector(&cleaned) {
                names.push(cleaned);
            }
        }
    }

    unique_preserve_order(names)
}

fn unique_preserve_order(names: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let key = dedupe_key(&name);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(name);
    }
    out
}

fn collect_release_artists(release: &ReleaseDetail) -> Vec<String> {
    extract_credit_names(release.artist_credit.as_deref())
}

fn release_year_from_release(release: &ReleaseDetail) -> Option<i32> {
    release.date.as_deref().and_then(parse_year)
}

fn release_track_count(release: &ReleaseDetail) -> usize {
    release
        .media
        .iter()
        .map(|medium| {
            if !medium.tracks.is_empty() {
                medium.tracks.len()
            } else {
                medium.track_count.unwrap_or(0)
            }
        })
        .sum()
}

fn collect_release_track_artists(release: &ReleaseDetail) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for medium in &release.media {
        for track in &medium.tracks {
            let mut names = Vec::new();
            names.extend(extract_credit_names(track.artist_credit.as_deref()));
            if let Some(recording) = &track.recording {
                names.extend(extract_credit_names(recording.artist_credit.as_deref()));
            }
            out.push(unique_preserve_order(names));
        }
    }
    out
}

fn infer_name_map_from_release(release: &ReleaseDetail) -> HashMap<String, String> {
    let mut alias_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut canonical_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut display_names: HashMap<(String, String), String> = HashMap::new();

    let mut harvest_credit = |credit: Option<&[Value]>| {
        let Some(credit) = credit else {
            return;
        };
        for item in credit {
            let Some(object) = item.as_object() else {
                continue;
            };
            let Some(artist) = object.get("artist").and_then(Value::as_object) else {
                continue;
            };
            let artist_id = artist
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if artist_id.is_empty() {
                continue;
            }

            if let Some(credited_name) = object.get("name").and_then(Value::as_str) {
                add_alias_name(
                    &mut alias_counts,
                    &mut canonical_counts,
                    &mut display_names,
                    &artist_id,
                    credited_name,
                    false,
                );
            }
            if let Some(artist_name) = artist.get("name").and_then(Value::as_str) {
                add_alias_name(
                    &mut alias_counts,
                    &mut canonical_counts,
                    &mut display_names,
                    &artist_id,
                    artist_name,
                    true,
                );
            }
        }
    };

    harvest_credit(release.artist_credit.as_deref());
    for medium in &release.media {
        for track in &medium.tracks {
            harvest_credit(track.artist_credit.as_deref());
            if let Some(recording) = &track.recording {
                harvest_credit(recording.artist_credit.as_deref());
            }
        }
    }

    let mut inferred = HashMap::new();
    for (artist_id, aliases) in alias_counts {
        let pool = canonical_counts.get(&artist_id).unwrap_or(&aliases);
        let Some((canonical_key, _)) = pool
            .iter()
            .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        else {
            continue;
        };
        let Some(canonical_name) = display_names
            .get(&(artist_id.clone(), canonical_key.clone()))
            .cloned()
        else {
            continue;
        };
        for alias_key in aliases.keys() {
            inferred.insert(alias_key.clone(), canonical_name.clone());
        }
    }

    inferred
}

fn add_alias_name(
    alias_counts: &mut HashMap<String, HashMap<String, usize>>,
    canonical_counts: &mut HashMap<String, HashMap<String, usize>>,
    display_names: &mut HashMap<(String, String), String>,
    artist_id: &str,
    raw_name: &str,
    canonical: bool,
) {
    let cleaned = normalize_artist_display(raw_name);
    if cleaned.is_empty() || !has_alnum(&cleaned) || is_connector(&cleaned) {
        return;
    }

    let key = dedupe_key(&cleaned);
    if key.is_empty() {
        return;
    }

    alias_counts
        .entry(artist_id.to_string())
        .or_default()
        .entry(key.clone())
        .and_modify(|count| *count += 1)
        .or_insert(1);
    display_names.insert((artist_id.to_string(), key.clone()), cleaned);

    if canonical {
        canonical_counts
            .entry(artist_id.to_string())
            .or_default()
            .entry(key)
            .and_modify(|count| *count += 1)
            .or_insert(1);
    }
}

fn build_track_collaborators(
    local_artist: &str,
    album_artists: &[String],
    release_artists: &[String],
    name_map: &HashMap<String, String>,
) -> Vec<String> {
    let album_canonical = canonicalize_artist_list(album_artists, name_map);
    let release_canonical = canonicalize_artist_list(release_artists, name_map);

    if !release_canonical.is_empty() {
        return unique_preserve_order(
            album_canonical
                .into_iter()
                .chain(release_canonical)
                .collect::<Vec<_>>(),
        );
    }
    if !album_canonical.is_empty() {
        return album_canonical;
    }
    if local_artist.trim().is_empty() {
        return Vec::new();
    }
    canonicalize_artist_list(&split_artist_string(local_artist), name_map)
}

fn select_significant_album_artists(
    album_artists: &[String],
    track_collaborators: &[Vec<String>],
    min_track_count: usize,
    min_track_ratio: f32,
) -> Vec<String> {
    let mut selected = unique_preserve_order(album_artists.to_vec());
    let mut selected_keys = selected
        .iter()
        .map(|name| dedupe_key(name))
        .collect::<HashSet<_>>();

    let mut counts = HashMap::<String, usize>::new();
    let mut display_names = HashMap::<String, String>::new();
    for names in track_collaborators {
        for name in unique_preserve_order(names.clone()) {
            let key = dedupe_key(&name);
            if key.is_empty() {
                continue;
            }
            counts
                .entry(key.clone())
                .and_modify(|count| *count += 1)
                .or_insert(1);
            display_names.entry(key).or_insert(name);
        }
    }

    let track_total = track_collaborators.len().max(1);
    let ratio_threshold = ((track_total as f32) * min_track_ratio.max(0.0))
        .ceil()
        .max(1.0) as usize;
    let threshold = min_track_count.max(ratio_threshold).max(1);

    let mut extras = counts
        .into_iter()
        .filter(|(key, count)| !selected_keys.contains(key) && *count >= threshold)
        .collect::<Vec<_>>();
    extras.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| display_names[&left.0].cmp(&display_names[&right.0]))
    });

    for (key, _) in extras {
        if let Some(name) = display_names.get(&key) {
            selected_keys.insert(key);
            selected.push(name.clone());
        }
    }

    if !selected.is_empty() {
        return unique_preserve_order(selected);
    }

    let Some((best_key, _)) = display_names
        .keys()
        .filter_map(|key| counts_for_key(track_collaborators, key).map(|count| (key, count)))
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(left.0)))
    else {
        return Vec::new();
    };

    display_names.get(best_key).cloned().into_iter().collect()
}

fn rank_selected_album_artists(
    selected: &[String],
    local_track_artists: &[String],
    track_collaborators: &[Vec<String>],
    name_map: &HashMap<String, String>,
) -> Vec<String> {
    if selected.len() <= 2 {
        return unique_preserve_order(selected.to_vec());
    }

    let mut local_counts = HashMap::<String, usize>::new();
    for local_artist in local_track_artists {
        for name in local_track_artist_candidates(local_artist, name_map) {
            let key = dedupe_key(&name);
            if key.is_empty() {
                continue;
            }
            local_counts
                .entry(key)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        }
    }

    let mut collaborator_counts = HashMap::<String, usize>::new();
    for names in track_collaborators {
        for name in unique_preserve_order(names.clone()) {
            let key = dedupe_key(&name);
            if key.is_empty() {
                continue;
            }
            collaborator_counts
                .entry(key)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        }
    }

    let mut ranked = selected
        .iter()
        .enumerate()
        .map(|(index, name)| (index, name.clone()))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        let left_key = dedupe_key(&left.1);
        let right_key = dedupe_key(&right.1);
        local_counts
            .get(&right_key)
            .copied()
            .unwrap_or(0)
            .cmp(&local_counts.get(&left_key).copied().unwrap_or(0))
            .then_with(|| {
                collaborator_counts
                    .get(&right_key)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&collaborator_counts.get(&left_key).copied().unwrap_or(0))
            })
            .then_with(|| left.0.cmp(&right.0))
    });

    unique_preserve_order(ranked.into_iter().map(|(_, name)| name).collect())
}

fn local_track_artist_candidates(
    local_artist: &str,
    name_map: &HashMap<String, String>,
) -> Vec<String> {
    let split = split_artist_string(local_artist);
    let canonical = canonicalize_artist_list(&split, name_map);
    if !canonical.is_empty() {
        return canonical;
    }

    let single = canonicalize_artist_name(local_artist, name_map);
    let key = dedupe_key(&single);
    if single.is_empty() || key.is_empty() || !has_alnum(&single) || is_connector(&single) {
        Vec::new()
    } else {
        vec![single]
    }
}

fn counts_for_key(track_collaborators: &[Vec<String>], target_key: &str) -> Option<usize> {
    let mut count = 0usize;
    for names in track_collaborators {
        let mut seen = false;
        for name in names {
            if dedupe_key(name) == target_key {
                seen = true;
                break;
            }
        }
        if seen {
            count += 1;
        }
    }
    if count == 0 {
        None
    } else {
        Some(count)
    }
}

fn parse_year(value: &str) -> Option<i32> {
    let mut digits = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() == 4 {
                break;
            }
        } else if !digits.is_empty() {
            break;
        }
    }
    if digits.len() == 4 {
        digits.parse().ok()
    } else {
        None
    }
}

fn deserialize_optional_score<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    let score = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.parse::<f64>().ok(),
        _ => None,
    };
    Ok(score)
}

fn ratio(left: &str, right: &str) -> f64 {
    let left = normalize_text(left);
    let right = normalize_text(right);
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    normalized_levenshtein(&left, &right)
}

fn candidate_score(
    local: &LocalAlbumInfo,
    candidate: &ReleaseSearchCandidate,
    release: &ReleaseDetail,
) -> f64 {
    let mb_score_raw = candidate.score.unwrap_or(0.0);
    let mb_score = mb_score_raw.min(100.0) / 10.0;
    let title_score = 40.0
        * ratio(
            local.album_hint.as_str(),
            release.title.as_deref().unwrap_or(""),
        );

    let mut year_score = 0.0;
    if let (Some(local_year), Some(release_year)) =
        (local.year_hint, release_year_from_release(release))
    {
        if local_year == release_year {
            year_score = 20.0;
        } else if (local_year - release_year).abs() <= 2 {
            year_score = 10.0;
        }
    }

    let mut count_score = 0.0;
    let remote_tracks = release_track_count(release);
    if local.track_count > 0 && remote_tracks > 0 {
        let diff = (local.track_count as f64 - remote_tracks as f64).abs();
        let max_count = local.track_count.max(remote_tracks) as f64;
        count_score = 30.0 * (1.0 - (diff / max_count));
    }

    mb_score + title_score + year_score + count_score.clamp(0.0, 30.0)
}

#[cfg(test)]
mod tests {
    use common::{artist_identity_id, stable_id, Artist};

    use super::{
        artist_search_tokens, build_release_queries, dedupe_key, default_artist_name_map,
        materialize_artists, rank_selected_album_artists, select_significant_album_artists,
    };

    #[test]
    fn dedupe_key_normalizes_dash_variants() {
        assert_eq!(dedupe_key("Jay-Z"), dedupe_key("Jay‐Z"));
    }

    #[test]
    fn select_significant_album_artists_keeps_repeat_collaborators() {
        let selected = select_significant_album_artists(
            &[String::from("Jay-Z")],
            &[
                vec![String::from("Jay-Z"), String::from("Kanye West")],
                vec![String::from("Jay-Z"), String::from("Kanye West")],
                vec![String::from("Jay-Z")],
            ],
            2,
            0.25,
        );
        assert_eq!(
            selected,
            vec![String::from("Jay-Z"), String::from("Kanye West")]
        );
    }

    #[test]
    fn materialize_artists_uses_canonical_artist_id() {
        let artists = materialize_artists(
            &[String::from("Ye")],
            &[Artist {
                id: stable_id("Ye"),
                name: String::from("Ye"),
                genres: vec![String::from("hip hop")],
                summary: Some(String::from("alias metadata")),
                logo_ref: None,
                banner_ref: None,
            }],
            &default_artist_name_map(),
        );

        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].id, artist_identity_id("Kanye West"));
        assert_eq!(artists[0].name, "Kanye West");
        assert_eq!(artists[0].summary.as_deref(), Some("alias metadata"));
    }

    #[test]
    fn materialize_artists_reuses_case_variant_identity() {
        let artists = materialize_artists(
            &[String::from("System of a Down")],
            &[Artist {
                id: artist_identity_id("System Of A Down"),
                name: String::from("System Of A Down"),
                genres: vec![String::from("Metal")],
                summary: None,
                logo_ref: None,
                banner_ref: None,
            }],
            &default_artist_name_map(),
        );

        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].id, artist_identity_id("System of a Down"));
        assert_eq!(artists[0].name, "System of a Down");
        assert_eq!(artists[0].genres, vec![String::from("Metal")]);
    }

    #[test]
    fn build_release_queries_adds_loose_artist_fallback() {
        let queries = build_release_queries(Some("Prince, 3RDEYEGIRL"), "PlectrumElectrum");

        assert_eq!(queries.len(), 3);
        assert!(queries[0].contains(r#"artist:"Prince, 3RDEYEGIRL""#));
        assert!(queries[1].contains(r#"artist:("prince" OR "3rdeyegirl")"#));
        assert!(queries[2].contains(r#"release:"PlectrumElectrum""#));
    }

    #[test]
    fn artist_search_tokens_normalizes_punctuation() {
        assert_eq!(
            artist_search_tokens("Tyler, The Creator"),
            vec![
                String::from("tyler"),
                String::from("the"),
                String::from("creator"),
            ]
        );
    }

    #[test]
    fn rank_selected_album_artists_prioritizes_main_local_track_artist() {
        let ranked = rank_selected_album_artists(
            &[
                String::from("Guest Artist"),
                String::from("Another Guest"),
                String::from("Kanye West"),
            ],
            &[String::from("Ye"), String::from("Ye"), String::from("Ye")],
            &[
                vec![String::from("Kanye West"), String::from("Guest Artist")],
                vec![String::from("Kanye West"), String::from("Another Guest")],
                vec![String::from("Kanye West")],
            ],
            &default_artist_name_map(),
        );

        assert_eq!(
            ranked,
            vec![
                String::from("Kanye West"),
                String::from("Guest Artist"),
                String::from("Another Guest"),
            ]
        );
    }
}
