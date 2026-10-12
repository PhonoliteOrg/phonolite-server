use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{artist_identity_id, Album, Artist, ArtistCredit, Track};
use library::Library;
use metadata::read_tags;
use regex::Regex;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use strsim::normalized_levenshtein;
use unicode_normalization::UnicodeNormalization;

use crate::{musicbrainz_http, performance};

#[path = "musicbrainz_album_identity.rs"]
mod album_identity;
pub use album_identity::{identity_source_fingerprint, AlbumIdentityOutcome};

const DEFAULT_MB_LIMIT: usize = 25;

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
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            mb_limit: DEFAULT_MB_LIMIT,
        }
    }
}

pub struct MusicBrainzAlbumArtistResolver<'a> {
    client: &'a Client,
    user_agent: &'a str,
    timeout: Duration,
    options: ResolveOptions,
}

pub struct AlbumArtistResolution {
    pub artists: Vec<Artist>,
    pub source_credit_repaired: bool,
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
    ) -> Result<Option<AlbumArtistResolution>, String> {
        if self.user_agent.trim().is_empty() {
            return Ok(None);
        }

        // Enriched credits can already contain a wrong primary. Always search
        // using the locally indexed identity rather than feeding it back into
        // the provider on later sweeps.
        let mut source_album = library
            .get_local_album(&album.id)
            .map_err(|err| err.to_string())?
            .unwrap_or_else(|| album.clone());
        let mut local = build_local_album_info(&source_album, tracks.len());
        let source_credit = library
            .album_credits(&album.id)
            .map_err(|err| err.to_string())?;
        local.allow_display_resolution = source_credit
            .credits
            .iter()
            .filter(|credit| credit.is_album_artist())
            .all(|credit| matches!(credit.source.as_str(), "local_tags" | "folder"));
        if local.allow_display_resolution && !source_credit.display_name.trim().is_empty() {
            local.artist_hint = source_credit.display_name;
        }
        local.track_titles = tracks.iter().map(|track| track.title.clone()).collect();
        let base_name_map = default_artist_name_map();
        let started = performance::enabled().then(Instant::now);
        let result = self.resolve_album_with_musicbrainz(&local).await;
        if let Some(started) = started {
            tracing::info!(
                target: "performance",
                event = "metadata_source_fetch",
                provider = "musicbrainz",
                kind = "album_artists",
                outcome = match &result {
                    Ok(Some(_)) => "found",
                    Ok(None) => "not_found",
                    Err(_) => "error",
                },
                elapsed_ms = performance::milliseconds(started.elapsed()),
            );
        }
        let resolved = match result? {
            Some(resolved) => resolved,
            None => return Ok(None),
        };
        let mut source_credit_repaired = false;
        if local.allow_display_resolution && !resolved.artist_credits.is_empty() {
            let repair_library = library.clone();
            let album_id = album.id.clone();
            let provider_credits = resolved.artist_credits.clone();
            let repaired = tokio::task::spawn_blocking(move || {
                repair_library.resolve_album_display_credit(&album_id, &provider_credits)
            })
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())?;
            if repaired {
                source_credit_repaired = true;
                source_album = library
                    .get_local_album(&album.id)
                    .map_err(|err| err.to_string())?
                    .unwrap_or(source_album);
            }
        }
        // Local collaborator tags are only needed after a remote release match.
        // Unmatched albums no longer reread every audio file during this sweep.
        local.track_artists = read_local_track_artists(library, tracks).await?;

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

        // Only release-level billing can establish album ownership. Repeated
        // song features remain song credits, regardless of their frequency.
        let selected = unique_preserve_order(album_artists.clone());
        // A shared composer, band name, or work title cannot identify a
        // performer. A release can authenticate the existing local primary
        // without proving that its additional credits belong to these files.
        // Only add artists independently supported by the local tags, and do
        // not let remote alias maps manufacture that supporting evidence.
        let selected = locally_supported_artists(
            &selected,
            &source_album,
            &local.track_artists,
            &base_name_map,
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
        let artists = preserve_local_primary(
            artists,
            &source_album,
            existing_artists,
            &effective_name_map,
        );
        if artists.is_empty() {
            Ok(None)
        } else {
            Ok(Some(AlbumArtistResolution {
                artists,
                source_credit_repaired,
            }))
        }
    }

    async fn resolve_album_with_musicbrainz(
        &self,
        local: &LocalAlbumInfo,
    ) -> Result<Option<ResolvedRelease>, String> {
        if !usable_artist_hint(&local.artist_hint) {
            return Ok(None);
        }
        let mut diagnostics = ReleaseMatchDiagnostics::default();
        for query in
            build_release_queries(Some(local.artist_hint.as_str()), local.album_hint.as_str())
        {
            let candidates = self
                .search_releases(query.as_str(), self.options.mb_limit)
                .await?;
            let best = best_matching_release_with_diagnostics(
                local,
                &candidates,
                |release_id| async move { self.fetch_release(&release_id).await },
                &mut diagnostics,
            )
            .await;
            if let Some(release) = best {
                return Ok(Some(ResolvedRelease {
                    album_artists: collect_release_artists(&release),
                    artist_credits: if release_tracks_align(local, &release) {
                        provider_artist_credits(&release)
                    } else {
                        Vec::new()
                    },
                    // Equal counts alone do not identify corresponding tracks.
                    track_artists: if release_tracks_align(local, &release) {
                        collect_release_track_artists(&release)
                    } else {
                        Vec::new()
                    },
                    auto_name_map: infer_name_map_from_release(&release),
                }));
            }
        }
        tracing::info!(
            provider = "musicbrainz",
            kind = "album_artists",
            album = %local.album_hint,
            artist = %local.artist_hint,
            candidates = diagnostics.candidates,
            reasons = %diagnostics.summary(),
            "MusicBrainz album-artist candidates did not establish the local release identity"
        );
        Ok(None)
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
        let payload: ReleaseSearchResponse =
            musicbrainz_http::get_json(self.client, url.as_str(), self.user_agent, self.timeout)
                .await?;
        Ok(payload.releases)
    }

    async fn fetch_release(&self, release_id: &str) -> Result<ReleaseDetail, String> {
        let url = Url::parse_with_params(
            &format!("https://musicbrainz.org/ws/2/release/{release_id}"),
            &[("fmt", "json"), ("inc", "recordings+artist-credits")],
        )
        .map_err(|err| err.to_string())?;
        musicbrainz_http::get_json(self.client, url.as_str(), self.user_agent, self.timeout).await
    }
}

#[derive(Clone, Debug)]
struct LocalAlbumInfo {
    allow_display_resolution: bool,
    album_hint: String,
    artist_hint: String,
    local_artist_names: Vec<String>,
    year_hint: Option<i32>,
    track_artists: Vec<String>,
    track_titles: Vec<String>,
    track_count: usize,
}

#[derive(Clone, Debug)]
struct ResolvedRelease {
    artist_credits: Vec<ArtistCredit>,
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
    title: Option<String>,
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<Value>>,
    recording: Option<ReleaseRecording>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseRecording {
    title: Option<String>,
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<Value>>,
}

async fn read_local_track_artists(
    library: &Library,
    tracks: &[Track],
) -> Result<Vec<String>, String> {
    let paths: Vec<Option<PathBuf>> = tracks
        .iter()
        .map(|track| library.resolve_relpath(&track.file_relpath))
        .collect();
    tokio::task::spawn_blocking(move || {
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
    .map_err(|err| err.to_string())
}

fn build_local_album_info(album: &Album, track_count: usize) -> LocalAlbumInfo {
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

    LocalAlbumInfo {
        allow_display_resolution: false,
        album_hint: album.title.clone(),
        artist_hint,
        local_artist_names: album.artist_names.clone(),
        year_hint: album.year,
        track_artists: Vec::new(),
        track_titles: Vec::new(),
        track_count,
    }
}

fn default_artist_name_map() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (alias, canonical) in DEFAULT_ARTIST_NAME_MAP {
        out.insert(dedupe_key(alias), normalize_artist_display(canonical));
    }
    out
}

fn locally_supported_artists(
    selected: &[String],
    source_album: &Album,
    local_track_artists: &[String],
    local_name_map: &HashMap<String, String>,
) -> Vec<String> {
    let supported = source_album
        .artist_names
        .iter()
        .chain(local_track_artists)
        // Repeated artist tag fields are joined with semicolons by the tag
        // reader. Slashes, commas, ampersands and "and" can be part of one
        // artist's name and must not authenticate separate remote artists.
        .flat_map(|value| value.split([';', '\0']))
        .map(|name| dedupe_key(&canonicalize_artist_name(name, local_name_map)))
        .filter(|key| !key.is_empty())
        .collect::<HashSet<_>>();
    selected
        .iter()
        .filter(|name| {
            supported.contains(&dedupe_key(&canonicalize_artist_name(name, local_name_map)))
        })
        .cloned()
        .collect()
}

fn preserve_local_primary(
    mut artists: Vec<Artist>,
    source_album: &Album,
    existing_artists: &[Artist],
    name_map: &HashMap<String, String>,
) -> Vec<Artist> {
    let Some(name) = source_album.artist_names.first() else {
        return artists;
    };
    let local_key = dedupe_key(&canonicalize_artist_name(name, name_map));
    artists.retain(|artist| {
        artist.id != source_album.artist_id
            && dedupe_key(&canonicalize_artist_name(&artist.name, name_map)) != local_key
    });
    let primary = existing_artists
        .iter()
        .find(|artist| artist.id == source_album.artist_id)
        .cloned()
        .map(|artist| Artist {
            name: name.clone(),
            ..artist
        })
        .unwrap_or_else(|| Artist {
            id: source_album.artist_id.clone(),
            name: name.clone(),
            genres: Vec::new(),
            summary: None,
            logo_ref: None,
            banner_ref: None,
        });
    artists.insert(0, primary);
    artists
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
    // Prefer the complete local title. Only recognized edition qualifiers
    // provide a second search title; arbitrary subtitles remain significant.
    let mut titles = vec![release];
    if let Some(base) = edition_base_title(release) {
        titles.push(base);
    }
    for title in titles {
        let variants = [
            build_exact_release_query(artist, title),
            artist.and_then(|artist| build_loose_artist_release_query(artist, title)),
            build_exact_release_query(None, title),
        ];
        for query in variants.into_iter().flatten() {
            if !queries.iter().any(|existing| existing == &query) {
                queries.push(query);
            }
        }
    }
    queries
}

fn edition_base_title(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    let open = match trimmed.chars().last()? {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    let start = trimmed.rfind(open)?;
    let base = trimmed[..start].trim_end();
    if base.is_empty()
        || !trimmed[..start]
            .chars()
            .last()
            .is_some_and(char::is_whitespace)
    {
        return None;
    }
    let qualifier = &trimmed[start + 1..trimmed.len() - 1];
    if qualifier.contains(['(', ')', '[', ']']) {
        return None;
    }
    // An edition word in a meaningful subtitle, such as "Deluxe Live at
    // Wembley", is insufficient. Every word must be edition vocabulary.
    let words = normalize_search_text(qualifier);
    let mut has_edition = false;
    for word in words.split_whitespace() {
        match word {
            "deluxe" | "expanded" | "anniversary" | "remaster" | "remastered" | "remastering" => {
                has_edition = true
            }
            "edition" | "version" | "super" | "special" | "limited" | "collectors"
            | "collector" | "s" | "digital" | "bonus" | "track" | "tracks" | "with" | "and"
            | "the" => {}
            _ if word.chars().all(|character| character.is_ascii_digit()) => {}
            _ if ["st", "nd", "rd", "th"].iter().any(|suffix| {
                word.strip_suffix(*suffix).is_some_and(|number| {
                    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
                })
            }) => {}
            _ => return None,
        }
    }
    has_edition.then_some(base)
}

fn edition_titles_match(local: &str, remote: &str) -> bool {
    let local_base = edition_base_title(local);
    let remote_base = edition_base_title(remote);
    (local_base.is_some() || remote_base.is_some())
        && normalize_search_text(local_base.unwrap_or(local))
            == normalize_search_text(remote_base.unwrap_or(remote))
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

fn provider_artist_credits(release: &ReleaseDetail) -> Vec<ArtistCredit> {
    let mut result = Vec::new();
    for value in release.artist_credit.as_deref().unwrap_or_default() {
        let Some(artist) = value.get("artist") else {
            return Vec::new();
        };
        let Some(id) = artist
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Vec::new();
        };
        let Some(name) = artist
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            return Vec::new();
        };
        result.push(ArtistCredit {
            artist_id: id.to_owned(),
            name: name.to_owned(),
            credited_name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_owned(),
            role: "album_artist".into(),
            join_phrase: value
                .get("joinphrase")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            source: "musicbrainz".into(),
        });
    }
    result
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

fn release_tracks_align(local: &LocalAlbumInfo, release: &ReleaseDetail) -> bool {
    let remote: Vec<_> = release
        .media
        .iter()
        .flat_map(|medium| &medium.tracks)
        .collect();
    !local.track_titles.is_empty()
        && local.track_titles.len() == remote.len()
        && local.track_titles.iter().zip(remote).all(|(title, track)| {
            let key = normalize_search_text(title);
            let remote_title = track.title.as_deref().or_else(|| {
                track
                    .recording
                    .as_ref()
                    .and_then(|recording| recording.title.as_deref())
            });
            !key.is_empty() && key == normalize_search_text(remote_title.unwrap_or_default())
        })
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
    let mut ambiguous = HashSet::new();
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
            if inferred
                .get(alias_key)
                .is_some_and(|name: &String| dedupe_key(name) != dedupe_key(&canonical_name))
            {
                ambiguous.insert(alias_key.clone());
            }
            inferred.insert(alias_key.clone(), canonical_name.clone());
        }
    }
    inferred.retain(|alias, _| !ambiguous.contains(alias));
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

fn rank_selected_album_artists(
    selected: &[String],
    local_track_artists: &[String],
    track_collaborators: &[Vec<String>],
    name_map: &HashMap<String, String>,
) -> Vec<String> {
    if selected.len() <= 1 {
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

fn usable_artist_hint(name: &str) -> bool {
    let key = normalize_search_text(name);
    !key.is_empty()
        && !matches!(
            key.as_str(),
            "unknown" | "unknown artist" | "various" | "various artists"
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseIdentityRejection {
    TitleMismatch,
    UnusableLocalArtist,
    MissingArtistCredits,
    MissingArtistIds,
    TracklistMismatch,
    ArtistMismatch,
}

impl ReleaseIdentityRejection {
    fn description(self) -> &'static str {
        match self {
            Self::TitleMismatch => "title mismatch",
            Self::UnusableLocalArtist => "unusable local artist",
            Self::MissingArtistCredits => "missing artist credits",
            Self::MissingArtistIds => "missing structured artist IDs",
            Self::TracklistMismatch => "track count, titles or order mismatch",
            Self::ArtistMismatch => "artist identities not supported locally",
        }
    }
}

#[derive(Default)]
struct ReleaseMatchDiagnostics {
    candidates: usize,
    detail_failures: usize,
    missing_release_ids: usize,
    rejections: HashMap<&'static str, usize>,
}

impl ReleaseMatchDiagnostics {
    fn reject(&mut self, rejection: ReleaseIdentityRejection) {
        *self.rejections.entry(rejection.description()).or_default() += 1;
    }

    fn summary(&self) -> String {
        if self.candidates == 0 {
            return "no search candidates".to_owned();
        }
        let mut counts = self.rejections.clone();
        if self.detail_failures > 0 {
            counts.insert("release detail fetch failed", self.detail_failures);
        }
        if self.missing_release_ids > 0 {
            counts.insert("missing release IDs", self.missing_release_ids);
        }
        let mut entries = counts.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(reason, _)| *reason);
        entries
            .into_iter()
            .map(|(reason, count)| format!("{reason}: {count}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn edition_artist_credits_supported(
    local: &LocalAlbumInfo,
    provider_credits: &[ArtistCredit],
) -> bool {
    if provider_credits.is_empty() {
        return false;
    }
    if local.allow_display_resolution
        && library::resolve_display_credit(&local.artist_hint, provider_credits).is_some()
    {
        return true;
    }

    // Names already established by explicit local credits are independent
    // evidence. Do not split punctuation or introduce remote-inferred aliases
    // here: either can manufacture support for an unmentioned collaborator.
    let name_map = default_artist_name_map();
    let local_names = local
        .local_artist_names
        .iter()
        .map(|name| dedupe_key(&canonicalize_artist_name(name, &name_map)))
        .filter(|key| !key.is_empty())
        .collect::<HashSet<_>>();
    if local_names.is_empty() {
        return false;
    }
    let mut covered = HashMap::new();
    let mut identified = HashMap::new();
    for credit in provider_credits {
        let matches = [&credit.name, &credit.credited_name]
            .into_iter()
            .map(|name| dedupe_key(&canonicalize_artist_name(name, &name_map)))
            .filter(|key| local_names.contains(key))
            .collect::<HashSet<_>>();
        if matches.len() != 1 {
            return false;
        }
        let local_key = matches.into_iter().next().unwrap();
        if covered
            .insert(local_key.clone(), credit.artist_id.clone())
            .is_some_and(|previous_id| previous_id != credit.artist_id)
            || identified
                .insert(credit.artist_id.clone(), local_key.clone())
                .is_some_and(|previous_key| previous_key != local_key)
        {
            return false;
        }
    }
    covered.len() == local_names.len()
}

fn release_identity_rejection(
    local: &LocalAlbumInfo,
    release: &ReleaseDetail,
) -> Option<ReleaseIdentityRejection> {
    let title_key = normalize_search_text(&local.album_hint);
    let remote_title = release.title.as_deref().unwrap_or_default();
    let exact_title = title_key == normalize_search_text(remote_title);
    if title_key.is_empty()
        || (!exact_title && !edition_titles_match(&local.album_hint, remote_title))
    {
        return Some(ReleaseIdentityRejection::TitleMismatch);
    }
    if !usable_artist_hint(&local.artist_hint) {
        return Some(ReleaseIdentityRejection::UnusableLocalArtist);
    }
    let Some(credits) = release
        .artist_credit
        .as_deref()
        .filter(|credits| !credits.is_empty())
    else {
        return Some(ReleaseIdentityRejection::MissingArtistCredits);
    };

    if !exact_title {
        let provider_credits = provider_artist_credits(release);
        if provider_credits.is_empty() {
            return Some(ReleaseIdentityRejection::MissingArtistIds);
        }
        if !release_tracks_align(local, release) {
            return Some(ReleaseIdentityRejection::TracklistMismatch);
        }
        return (!edition_artist_credits_supported(local, &provider_credits))
            .then_some(ReleaseIdentityRejection::ArtistMismatch);
    }

    // Search relevance is not identity evidence. In particular, classical
    // releases often share a work title, year and track count across performers.
    // Accept only the credited name, canonical name, or full joint credit;
    // never use fuzzy artist similarity to change an album's attribution.
    // Track aliases cannot authenticate a release artist. A shared or
    // conflicting track alias must never remap the trusted local identity.
    let release_credit_only = ReleaseDetail {
        media: Vec::new(),
        ..release.clone()
    };
    let name_map = build_effective_name_map(
        &default_artist_name_map(),
        Some(&infer_name_map_from_release(&release_credit_only)),
    );
    let local_key = normalize_search_text(&canonicalize_artist_name(&local.artist_hint, &name_map));
    let mut full_credit = String::new();
    let mut matched = false;
    for credit in credits {
        let credited = credit
            .as_str()
            .or_else(|| credit.get("name").and_then(Value::as_str));
        let canonical = credit
            .get("artist")
            .and_then(|artist| artist.get("name"))
            .and_then(Value::as_str);
        for name in credited.into_iter().chain(canonical) {
            if normalize_search_text(&canonicalize_artist_name(name, &name_map)) == local_key {
                matched = true;
            }
        }
        if let Some(name) = credited.or(canonical) {
            full_credit.push_str(name);
        }
        if let Some(joinphrase) = credit.get("joinphrase").and_then(Value::as_str) {
            full_credit.push_str(joinphrase);
        }
    }
    if matched || normalize_search_text(&full_credit) == local_key {
        return None;
    }
    if local.allow_display_resolution {
        let provider_credits = provider_artist_credits(release);
        if provider_credits.is_empty() {
            return Some(ReleaseIdentityRejection::MissingArtistIds);
        }
        if library::resolve_display_credit(&local.artist_hint, &provider_credits).is_some() {
            return (!release_tracks_align(local, release))
                .then_some(ReleaseIdentityRejection::TracklistMismatch);
        }
    }
    Some(ReleaseIdentityRejection::ArtistMismatch)
}

#[cfg(test)]
fn release_matches_local_identity(local: &LocalAlbumInfo, release: &ReleaseDetail) -> bool {
    release_identity_rejection(local, release).is_none()
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

fn candidate_score_upper_bound(local: &LocalAlbumInfo, candidate: &ReleaseSearchCandidate) -> f64 {
    candidate.score.unwrap_or(0.0).min(100.0) / 10.0
        + 40.0 // A perfect title match.
        + if local.year_hint.is_some() { 20.0 } else { 0.0 }
        + if local.track_count > 0 { 30.0 } else { 0.0 }
}

#[cfg(test)]
async fn best_matching_release<F, Fut>(
    local: &LocalAlbumInfo,
    candidates: &[ReleaseSearchCandidate],
    fetch: F,
) -> Option<ReleaseDetail>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<ReleaseDetail, String>>,
{
    best_matching_release_with_diagnostics(
        local,
        candidates,
        fetch,
        &mut ReleaseMatchDiagnostics::default(),
    )
    .await
}

async fn best_matching_release_with_diagnostics<F, Fut>(
    local: &LocalAlbumInfo,
    candidates: &[ReleaseSearchCandidate],
    mut fetch: F,
    diagnostics: &mut ReleaseMatchDiagnostics,
) -> Option<ReleaseDetail>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<ReleaseDetail, String>>,
{
    let mut best_score = -1.0f64;
    let mut best = None;
    let mut fetched = HashMap::new();
    for candidate in candidates {
        diagnostics.candidates += 1;
        let Some(release_id) = candidate.id.as_deref() else {
            diagnostics.missing_release_ids += 1;
            continue;
        };
        // Equal scores retain the first candidate, matching the original search
        // order. A detail request cannot change that when its best possible
        // score is no better. A perfect first result skips the remaining 24
        // requests at the default search limit.
        if candidate_score_upper_bound(local, candidate) <= best_score {
            continue;
        }
        let release = match fetched.get(release_id) {
            Some(release) => release,
            None => {
                let Ok(release) = fetch(release_id.to_owned()).await else {
                    diagnostics.detail_failures += 1;
                    // Failed requests are not cached: a duplicate candidate may
                    // still succeed if the provider recovers later in this pass.
                    continue;
                };
                fetched.insert(release_id.to_owned(), release);
                &fetched[release_id]
            }
        };
        if let Some(rejection) = release_identity_rejection(local, release) {
            diagnostics.reject(rejection);
            continue;
        }
        let score = candidate_score(local, candidate, release);
        if score > best_score {
            best_score = score;
            best = Some(release.clone());
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use common::{artist_identity_id, stable_id, Artist};

    use super::{
        artist_search_tokens, build_release_queries, dedupe_key, default_artist_name_map,
        materialize_artists, rank_selected_album_artists,
    };

    fn local_fixture(year: Option<i32>, tracks: usize) -> super::LocalAlbumInfo {
        super::LocalAlbumInfo {
            allow_display_resolution: false,
            album_hint: "Album".to_owned(),
            artist_hint: "Artist".to_owned(),
            local_artist_names: vec!["Artist".to_owned()],
            year_hint: year,
            track_artists: Vec::new(),
            track_titles: Vec::new(),
            track_count: tracks,
        }
    }

    fn candidate_fixture(id: Option<&str>, score: Option<f64>) -> super::ReleaseSearchCandidate {
        super::ReleaseSearchCandidate {
            id: id.map(str::to_owned),
            score,
        }
    }

    fn release_fixture(
        title: &str,
        year: Option<i32>,
        tracks: usize,
        artist: &str,
    ) -> super::ReleaseDetail {
        super::ReleaseDetail {
            title: Some(title.to_owned()),
            date: year.map(|year| year.to_string()),
            artist_credit: Some(vec![serde_json::json!({
                "name": artist, "artist": {"id": "fixture-artist", "name": "Artist"}
            })]),
            media: vec![super::ReleaseMedium {
                track_count: Some(tracks),
                tracks: Vec::new(),
            }],
        }
    }

    fn credited_release(title: &str, artist: &str) -> super::ReleaseDetail {
        let mut release = release_fixture(title, Some(2005), 12, artist);
        release.artist_credit = Some(vec![serde_json::json!({
            "name": artist, "artist": {"id": artist, "name": artist}
        })]);
        release
    }

    fn source_album(names: &[&str]) -> common::Album {
        serde_json::from_value(serde_json::json!({
            "id": "album", "artist_id": artist_identity_id(names[0]),
            "artist_names": names, "title": "The Four Seasons", "year": 2005,
            "folder_relpath": "Vivaldi/The Four Seasons", "cover_ref": null
        }))
        .unwrap()
    }

    #[test]
    fn repeated_song_guests_require_release_billing_to_qualify_as_album_artists() {
        let mut release = release_fixture("Album", Some(2026), 4, "Main");
        let source = source_album(&["Main"]);
        let track_artists = vec!["Main; Guest".to_owned(); 4];
        let release_artists = super::collect_release_artists(&release);
        assert_eq!(
            super::locally_supported_artists(
                &release_artists,
                &source,
                &track_artists,
                &default_artist_name_map()
            ),
            ["Main"]
        );
        release
            .artist_credit
            .as_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "Guest", "artist": {"id": "guest", "name": "Guest"}
            }));
        let joint_billing = super::collect_release_artists(&release);
        assert_eq!(
            super::locally_supported_artists(
                &joint_billing,
                &source,
                &track_artists,
                &default_artist_name_map()
            ),
            ["Main", "Guest"]
        );
    }

    #[tokio::test]
    async fn a_composer_match_cannot_import_an_unconfirmed_performer() {
        let source = source_album(&["Antonio Vivaldi"]);
        let mut local = super::build_local_album_info(&source, 12);
        local.track_artists = vec!["Antonio Vivaldi".to_owned(); 12];
        let remote: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "title": "The Four Seasons", "date": "2005",
            "artist-credit": [
                {"name": "Antonio Vivaldi", "artist": {"id": "composer", "name": "Antonio Vivaldi"}},
                {"name": "Unconfirmed Performer", "artist": {"id": "performer", "name": "Unconfirmed Performer"}}
            ],
            "media": [{"track-count": 12}]
        }))
        .unwrap();
        let matched = super::best_matching_release(
            &local,
            &[candidate_fixture(
                Some("perfect-but-ambiguous"),
                Some(100.0),
            )],
            |_| std::future::ready(Ok(remote.clone())),
        )
        .await
        .unwrap();
        let supported = super::locally_supported_artists(
            &super::collect_release_artists(&matched),
            &source,
            &local.track_artists,
            &default_artist_name_map(),
        );
        assert_eq!(supported, ["Antonio Vivaldi"]);
    }

    #[test]
    fn named_local_performers_and_explicit_tag_lists_support_additional_credits() {
        let source = source_album(&["Antonio Vivaldi"]);
        let selected = [
            "Antonio Vivaldi".to_owned(),
            "Janine Jansen".to_owned(),
            "Another Performer".to_owned(),
            "Unknown Guest".to_owned(),
        ];
        let supported = super::locally_supported_artists(
            &selected,
            &source,
            &["Janine Jansen; Another Performer".to_owned()],
            &default_artist_name_map(),
        );
        assert_eq!(
            supported,
            ["Antonio Vivaldi", "Janine Jansen", "Another Performer"]
        );
    }

    #[test]
    fn remote_aliases_and_band_punctuation_do_not_manufacture_local_artist_evidence() {
        let source = source_album(&["AC/DC"]);
        let selected = [
            "AC/DC".to_owned(),
            "AC".to_owned(),
            "DC".to_owned(),
            "Earth".to_owned(),
            "Wind".to_owned(),
            "Fire".to_owned(),
            "Remote Canonical Artist".to_owned(),
            "Kanye West".to_owned(),
        ];
        let supported = super::locally_supported_artists(
            &selected,
            &source,
            &[
                "Earth, Wind & Fire".to_owned(),
                "Local Artist Alias".to_owned(),
                "Ye".to_owned(),
            ],
            &default_artist_name_map(),
        );
        assert_eq!(supported, ["AC/DC", "Kanye West"]);
    }

    #[tokio::test]
    async fn four_seasons_requires_the_local_performer_even_for_a_perfect_search_hit() {
        let mut local = local_fixture(Some(2005), 12);
        local.album_hint = "The Four Seasons".to_owned();
        local.artist_hint = "Janine Jansen".to_owned();
        let wrong = credited_release("The Four Seasons", "Jacques Loussier Trio");
        let right = credited_release("The Four Seasons", "Janine Jansen");
        let candidates = [
            candidate_fixture(Some("wrong"), Some(100.0)),
            candidate_fixture(Some("right"), Some(80.0)),
        ];
        let release = super::best_matching_release(&local, &candidates, |id| {
            std::future::ready(Ok(if id == "wrong" {
                wrong.clone()
            } else {
                right.clone()
            }))
        })
        .await
        .unwrap();
        assert_eq!(super::collect_release_artists(&release), ["Janine Jansen"]);
        assert!(super::best_matching_release(&local, &candidates[..1], |_| {
            std::future::ready(Ok(wrong.clone()))
        })
        .await
        .is_none());
    }

    #[test]
    fn identity_validation_rejects_missing_credits_unknown_artists_and_other_versions() {
        let mut local = local_fixture(Some(2020), 10);
        let mut release = credited_release("Album", "Artist");
        assert!(super::release_matches_local_identity(&local, &release));
        release.artist_credit = None;
        assert!(!super::release_matches_local_identity(&local, &release));
        release = credited_release("Album (Live)", "Artist");
        assert!(!super::release_matches_local_identity(&local, &release));
        for name in ["", "Unknown Artist", "Various Artists"] {
            local.artist_hint = name.to_owned();
            release = credited_release("Album", name);
            assert!(!super::release_matches_local_identity(&local, &release));
        }
    }

    #[test]
    fn artist_identity_accepts_release_credit_aliases_but_never_track_aliases() {
        let local = local_fixture(Some(2020), 10);
        let release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "title": "Album",
            "artist-credit": [{"name": "Artist", "artist": {"id": "a", "name": "Canonical"}}]
        }))
        .unwrap();
        assert!(super::release_matches_local_identity(&local, &release));

        let release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "title": "Album",
            "artist-credit": [{"artist": {"id": "other", "name": "Other Artist"}}],
            "media": [{"tracks": [{
                "artist-credit": [{"name": "Artist", "artist": {"id": "other", "name": "Other Artist"}}]
            }]}]
        }))
        .unwrap();
        assert!(!super::release_matches_local_identity(&local, &release));
    }

    #[test]
    fn conflicting_artist_aliases_are_not_canonicalized() {
        let release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "artist-credit": [{"name": "Shared Name", "artist": {"id": "a", "name": "First Artist"}}],
            "media": [{"tracks": [{
                "artist-credit": [{"name": "Shared Name", "artist": {"id": "b", "name": "Second Artist"}}]
            }]}]
        }))
        .unwrap();
        let map = super::infer_name_map_from_release(&release);
        assert!(!map.contains_key(&dedupe_key("Shared Name")));
    }

    #[test]
    fn verified_alias_keeps_the_original_local_primary_identity() {
        let source: common::Album = serde_json::from_value(serde_json::json!({
            "id": "album", "artist_id": artist_identity_id("Ye"),
            "artist_names": ["Ye"], "title": "Album", "year": null,
            "folder_relpath": "Ye/Album", "cover_ref": null
        }))
        .unwrap();
        let map = default_artist_name_map();
        let remote = materialize_artists(
            &["Guest Artist".to_owned(), "Kanye West".to_owned()],
            &[],
            &map,
        );
        let artists = super::preserve_local_primary(remote, &source, &[], &map);
        assert_eq!(artists[0].id, source.artist_id);
        assert_eq!(artists[0].name, "Ye");
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[1].name, "Guest Artist");
    }

    #[test]
    fn track_credits_require_matching_titles_and_order_not_just_count() {
        let mut local = local_fixture(Some(2020), 2);
        local.track_titles = vec!["First".to_owned(), "Second".to_owned()];
        let mut release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "media": [{"tracks": [
                {"title": "First"}, {"recording": {"title": "Second"}}
            ]}]
        }))
        .unwrap();
        assert!(super::release_tracks_align(&local, &release));
        release.media[0].tracks.reverse();
        assert!(!super::release_tracks_align(&local, &release));
        release.media[0].tracks.pop();
        assert!(!super::release_tracks_align(&local, &release));
        local.track_titles.clear();
        assert!(!super::release_tracks_align(&local, &release));
    }

    #[test]
    fn duplicated_prince_display_requires_complete_provider_ids_and_an_aligned_tracklist() {
        let mut local = local_fixture(Some(1984), 2);
        local.allow_display_resolution = true;
        local.album_hint = "Purple Rain".into();
        local.artist_hint = "Prince, Prince and the Revolution".into();
        local.track_titles = vec!["Let's Go Crazy".into(), "Take Me with U".into()];
        let mut release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "title": "Purple Rain", "date": "1984",
            "artist-credit": [
                {"name": "Prince", "joinphrase": " and ", "artist": {
                    "id": "070d193a-845c-479f-980e-bef15710653e", "name": "Prince"}},
                {"name": "the Revolution", "joinphrase": "", "artist": {
                    "id": "4c8ead39-b9df-4c56-a27c-51bc049cfd48", "name": "The Revolution"}}
            ],
            "media": [{"tracks": [{"title": "Let's Go Crazy"}, {"title": "Take Me with U"}]}]
        }))
        .unwrap();
        assert!(super::release_matches_local_identity(&local, &release));
        let credits = super::provider_artist_credits(&release);
        assert_eq!(credits[0].join_phrase, " and ");
        let resolved = library::resolve_display_credit(&local.artist_hint, &credits).unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(|credit| credit.name.as_str())
                .collect::<Vec<_>>(),
            ["Prince", "The Revolution"]
        );

        local.allow_display_resolution = false;
        assert!(!super::release_matches_local_identity(&local, &release));
        local.allow_display_resolution = true;
        release.media[0].tracks.reverse();
        assert!(!super::release_matches_local_identity(&local, &release));
        release.media[0].tracks.reverse();
        release.artist_credit.as_mut().unwrap()[1]["artist"]
            .as_object_mut()
            .unwrap()
            .remove("id");
        assert!(super::provider_artist_credits(&release).is_empty());
        assert!(!super::release_matches_local_identity(&local, &release));
    }

    fn prince_expanded_fixture() -> (super::LocalAlbumInfo, super::ReleaseDetail) {
        let mut local = local_fixture(Some(1985), 22);
        local.allow_display_resolution = true;
        local.album_hint = "Around The World In A Day (Deluxe Expanded Edition)".into();
        local.artist_hint = "Prince, Prince and the Revolution".into();
        local.local_artist_names = vec![local.artist_hint.clone()];
        // Synthetic track names test an expanded catalog without implying that
        // this user-specific edition exists in the live provider catalog.
        local.track_titles = (1..=22).map(|number| format!("Track {number}")).collect();
        let tracks = local
            .track_titles
            .iter()
            .map(|title| serde_json::json!({"title": title}))
            .collect::<Vec<_>>();
        let release = serde_json::from_value(serde_json::json!({
            "title": "Around The World In A Day", "date": "1985",
            "artist-credit": [
                {"name": "Prince", "joinphrase": " and ", "artist": {
                    "id": "070d193a-845c-479f-980e-bef15710653e", "name": "Prince"}},
                {"name": "the Revolution", "joinphrase": "", "artist": {
                    "id": "4c8ead39-b9df-4c56-a27c-51bc049cfd48", "name": "The Revolution"}}
            ],
            "media": [{"tracks": tracks}]
        }))
        .unwrap();
        (local, release)
    }

    #[tokio::test]
    async fn expanded_title_fallback_selects_only_the_fully_aligned_twenty_two_track_release() {
        let (local, right) = prince_expanded_fixture();
        let mut standard = right.clone();
        standard.media[0].tracks.truncate(9);
        let mut wrong_performer = right.clone();
        wrong_performer.artist_credit =
            credited_release("unused", "Another Performer").artist_credit;
        let candidates = [
            candidate_fixture(Some("standard"), Some(100.0)),
            candidate_fixture(Some("wrong-performer"), Some(100.0)),
            candidate_fixture(Some("expanded"), Some(80.0)),
        ];
        let selected = super::best_matching_release(&local, &candidates, |id| {
            std::future::ready(Ok(match id.as_str() {
                "standard" => standard.clone(),
                "wrong-performer" => wrong_performer.clone(),
                _ => right.clone(),
            }))
        })
        .await
        .unwrap();
        assert_eq!(
            super::collect_release_artists(&selected),
            ["Prince", "the Revolution"]
        );
        assert!(super::release_tracks_align(&local, &selected));
        assert!(
            super::best_matching_release(&local, &candidates[..2], |id| {
                std::future::ready(Ok(if id == "standard" {
                    standard.clone()
                } else {
                    wrong_performer.clone()
                }))
            })
            .await
            .is_none()
        );
    }

    #[test]
    fn edition_title_variants_require_every_track_and_complete_supported_artist_ids() {
        let (local, right) = prince_expanded_fixture();
        assert!(super::release_matches_local_identity(&local, &right));
        let mut release = right.clone();
        release.title =
            Some("Around The World In A Day [25th Anniversary Remastered Edition]".into());
        assert!(super::release_matches_local_identity(&local, &release));

        release = right.clone();
        release.media[0].tracks.swap(0, 1);
        assert!(!super::release_matches_local_identity(&local, &release));
        release = right.clone();
        release.media[0].tracks.pop();
        assert!(!super::release_matches_local_identity(&local, &release));
        release = right.clone();
        release.media[0].tracks[0].title = Some("Different recording".into());
        assert!(!super::release_matches_local_identity(&local, &release));
        release = right.clone();
        release.artist_credit.as_mut().unwrap()[1]["artist"]
            .as_object_mut()
            .unwrap()
            .remove("id");
        assert!(!super::release_matches_local_identity(&local, &release));
        release = right.clone();
        release
            .artist_credit
            .as_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "Unmentioned Guest", "artist": {"id": "guest", "name": "Unmentioned Guest"}
            }));
        assert!(!super::release_matches_local_identity(&local, &release));
        release = right.clone();
        release.artist_credit.as_mut().unwrap().remove(0);
        assert!(!super::release_matches_local_identity(&local, &release));
    }

    #[test]
    fn explicit_local_artist_names_support_edition_variants_without_parsing_display_punctuation() {
        let (template, right) = prince_expanded_fixture();
        let mut album = source_album(&["Prince", "The Revolution"]);
        album.title = template.album_hint;
        let mut local = super::build_local_album_info(&album, 22);
        local.track_titles = template.track_titles;
        assert!(!local.allow_display_resolution);
        assert_eq!(local.artist_hint, "Prince");
        assert_eq!(local.local_artist_names, ["Prince", "The Revolution"]);
        assert!(super::release_matches_local_identity(&local, &right));

        let mut release = right.clone();
        release.artist_credit.as_mut().unwrap().pop();
        assert!(!super::release_matches_local_identity(&local, &release));
        local.local_artist_names = vec!["Prince".into()];
        assert!(!super::release_matches_local_identity(&local, &right));
        local.local_artist_names = vec!["Prince, Prince and the Revolution".into()];
        local.artist_hint = local.local_artist_names[0].clone();
        assert!(!super::release_matches_local_identity(&local, &right));
    }

    #[test]
    fn edition_variants_reject_conflicting_aliases_and_identity_collapses() {
        let (mut local, right) = prince_expanded_fixture();
        local.allow_display_resolution = false;
        local.artist_hint = "Prince".into();
        local.local_artist_names = vec!["Prince".into(), "The Revolution".into()];
        let mut release = right.clone();
        // A canonical name and its alias cannot merge two established local
        // artists into a single provider identity.
        release.artist_credit.as_mut().unwrap().pop();
        release.artist_credit.as_mut().unwrap()[0]["name"] = "The Revolution".into();
        assert!(!super::release_matches_local_identity(&local, &release));

        // A shared alias cannot authenticate an additional, distinct artist ID.
        local.local_artist_names = vec!["Prince".into()];
        release = right.clone();
        release.artist_credit.as_mut().unwrap()[1]["name"] = "Prince".into();
        assert!(!super::release_matches_local_identity(&local, &release));
        release.artist_credit.as_mut().unwrap()[1]["artist"]["name"] = "Prince".into();
        assert!(!super::release_matches_local_identity(&local, &release));

        // Repeated references to the same authenticated identity are harmless.
        release.artist_credit.as_mut().unwrap()[1]["artist"]["id"] =
            "070d193a-845c-479f-980e-bef15710653e".into();
        assert!(super::release_matches_local_identity(&local, &release));
    }

    #[test]
    fn release_queries_prefer_full_title_and_only_strip_known_trailing_edition_qualifiers() {
        let full = "Around The World In A Day (Deluxe Expanded Edition)";
        let queries = build_release_queries(Some("Prince, Prince and the Revolution"), full);
        assert_eq!(queries.len(), 6);
        assert!(queries[..3]
            .iter()
            .all(|query| query.contains(&format!(r#"release:"{full}""#))));
        assert!(queries[3..]
            .iter()
            .all(|query| query.contains(r#"release:"Around The World In A Day""#)));

        for suffix in [
            "(Deluxe Edition)",
            "[Super Deluxe Expanded Edition]",
            "(2011 Remaster)",
            "(Remastered)",
            "(25th Anniversary Edition)",
        ] {
            let title = format!("Album {suffix}");
            assert_eq!(super::edition_base_title(&title), Some("Album"));
        }
        for title in [
            "Album (Live)",
            "Album (The Motion Picture)",
            "Album (Deluxe Live at Wembley)",
            "Album (Special Edition)",
            "Album (Live (Deluxe))",
            "Album(Deluxe)",
            "(Deluxe Edition)",
            "Album (Expanded Experience)",
        ] {
            assert_eq!(super::edition_base_title(title), None, "{title}");
            assert_eq!(
                build_release_queries(Some("Artist"), title).len(),
                3,
                "{title}"
            );
        }
        let (mut local, right) = prince_expanded_fixture();
        local.album_hint = "Around The World In A Day (Live)".into();
        assert!(!super::release_matches_local_identity(&local, &right));
    }

    #[tokio::test]
    async fn unmatched_release_diagnostics_distinguish_fetch_title_tracks_ids_and_artists() {
        let (local, right) = prince_expanded_fixture();
        let candidates = [
            candidate_fixture(None, Some(100.0)),
            candidate_fixture(Some("fetch-failed"), Some(100.0)),
            candidate_fixture(Some("wrong-title"), Some(100.0)),
            candidate_fixture(Some("wrong-tracks"), Some(100.0)),
            candidate_fixture(Some("missing-ids"), Some(100.0)),
            candidate_fixture(Some("wrong-artist"), Some(100.0)),
        ];
        let mut diagnostics = super::ReleaseMatchDiagnostics::default();
        assert!(super::best_matching_release_with_diagnostics(
            &local,
            &candidates,
            |id| {
                let mut release = right.clone();
                match id.as_str() {
                    "fetch-failed" => {
                        return std::future::ready(Err("provider unavailable".into()))
                    }
                    "wrong-title" => release.title = Some("Unrelated Album".into()),
                    "wrong-tracks" => {
                        release.media[0].tracks.pop();
                    }
                    "missing-ids" => {
                        release.artist_credit.as_mut().unwrap()[0]["artist"]
                            .as_object_mut()
                            .unwrap()
                            .remove("id");
                    }
                    _ => {
                        release.artist_credit =
                            credited_release("unused", "Another Performer").artist_credit
                    }
                }
                std::future::ready(Ok(release))
            },
            &mut diagnostics
        )
        .await
        .is_none());
        assert_eq!(diagnostics.candidates, 6);
        let summary = diagnostics.summary();
        for reason in [
            "release detail fetch failed: 1",
            "missing release IDs: 1",
            "title mismatch: 1",
            "track count, titles or order mismatch: 1",
            "missing structured artist IDs: 1",
            "artist identities not supported locally: 1",
        ] {
            assert!(summary.contains(reason), "{summary}");
        }
        assert_eq!(
            super::ReleaseMatchDiagnostics::default().summary(),
            "no search candidates"
        );
    }

    #[test]
    fn structured_provider_group_credit_is_one_identity_even_with_a_joint_display() {
        let release: super::ReleaseDetail = serde_json::from_value(serde_json::json!({
            "title": "Raising Sand",
            "artist-credit": [{"name": "Robert Plant | Alison Krauss", "artist": {
                "id": "38eb4af8-2282-4b8f-8819-1c7f92640dde", "name": "Robert Plant & Alison Krauss"}}]
        })).unwrap();
        let credits = super::provider_artist_credits(&release);
        let resolved =
            library::resolve_display_credit("Robert Plant | Alison Krauss", &credits).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].artist_id,
            "38eb4af8-2282-4b8f-8819-1c7f92640dde"
        );
        assert_eq!(
            super::collect_release_artists(&release),
            ["Robert Plant | Alison Krauss"]
        );
    }

    #[test]
    fn identity_validation_keeps_non_latin_names_distinct() {
        let mut local = local_fixture(None, 0);
        local.artist_hint = "東京".to_owned();
        local.album_hint = "Album 東京".to_owned();
        assert!(super::release_matches_local_identity(
            &local,
            &credited_release("Album 東京", "東京")
        ));
        assert!(!super::release_matches_local_identity(
            &local,
            &credited_release("Album 大阪", "東京")
        ));
        assert!(!super::release_matches_local_identity(
            &local,
            &credited_release("Album 東京", "大阪")
        ));
    }

    #[tokio::test]
    async fn perfect_release_skips_remaining_24_requests_and_retains_first_tie() {
        let local = local_fixture(Some(2020), 10);
        let candidates: Vec<_> = (0..25)
            .map(|index| candidate_fixture(Some(&index.to_string()), Some(100.0)))
            .collect();
        let mut requests = 0;
        let release = super::best_matching_release(&local, &candidates, |id| {
            requests += 1;
            std::future::ready(Ok(release_fixture("Album", Some(2020), 10, &id)))
        })
        .await
        .unwrap();
        assert_eq!(requests, 1);
        assert_eq!(super::collect_release_artists(&release), ["0"]);
    }

    #[tokio::test]
    async fn missing_local_year_and_count_use_lower_bound_without_changing_ties() {
        let local = local_fixture(None, 0);
        let candidates = [
            candidate_fixture(Some("first"), None),
            candidate_fixture(Some("second"), Some(0.0)),
        ];
        let mut requests = 0;
        let release = super::best_matching_release(&local, &candidates, |id| {
            requests += 1;
            std::future::ready(Ok(release_fixture("Album", Some(2020), 10, &id)))
        })
        .await
        .unwrap();
        assert_eq!(requests, 1);
        assert_eq!(super::collect_release_artists(&release), ["first"]);
    }

    #[tokio::test]
    async fn successful_duplicate_release_ids_reuse_details_with_new_search_score() {
        let local = local_fixture(Some(2020), 10);
        let candidates = [
            candidate_fixture(Some("same"), Some(50.0)),
            candidate_fixture(Some("same"), Some(100.0)),
            candidate_fixture(Some("cannot_win"), Some(99.0)),
        ];
        let mut requests = 0;
        let release = super::best_matching_release(&local, &candidates, |id| {
            requests += 1;
            std::future::ready(Ok(release_fixture("Album", Some(2020), 10, &id)))
        })
        .await
        .unwrap();
        assert_eq!(requests, 1);
        assert_eq!(super::collect_release_artists(&release), ["same"]);
    }

    #[tokio::test]
    async fn failed_duplicate_release_can_recover_and_missing_ids_do_not_fetch() {
        let local = local_fixture(Some(2020), 10);
        let candidates = [
            candidate_fixture(None, Some(100.0)),
            candidate_fixture(Some("same"), Some(100.0)),
            candidate_fixture(Some("same"), Some(100.0)),
        ];
        let mut requests = 0;
        let release = super::best_matching_release(&local, &candidates, |id| {
            requests += 1;
            std::future::ready(if requests == 1 {
                Err("provider unavailable".to_owned())
            } else {
                Ok(release_fixture("Album", Some(2020), 10, &id))
            })
        })
        .await;
        assert!(release.is_some());
        assert_eq!(requests, 2);
    }

    #[tokio::test]
    async fn no_candidates_or_all_failed_details_return_no_match() {
        let local = local_fixture(Some(2020), 10);
        let mut requests = 0;
        let mut fetch = |_| {
            requests += 1;
            std::future::ready(Err("not available".to_owned()))
        };
        assert!(super::best_matching_release(&local, &[], &mut fetch)
            .await
            .is_none());
        let candidates = [candidate_fixture(Some("failed"), Some(100.0))];
        assert!(
            super::best_matching_release(&local, &candidates, &mut fetch)
                .await
                .is_none()
        );
        assert_eq!(requests, 1);
    }

    #[tokio::test]
    async fn pruning_matches_exhaustive_scoring_for_partial_and_different_releases() {
        let releases = [
            release_fixture("Other", None, 0, "other"),
            release_fixture("Albums", Some(2018), 8, "close"),
            release_fixture("Album", Some(2020), 10, "perfect"),
            release_fixture("Album", Some(2020), 10, "equal"),
        ];
        for year in [None, Some(2020), Some(2010)] {
            for tracks in [0, 10, 20] {
                for search_score in [None, Some(-10.0), Some(50.0), Some(100.0), Some(150.0)] {
                    let local = local_fixture(year, tracks);
                    let candidates: Vec<_> = (0..releases.len())
                        .map(|index| candidate_fixture(Some(&index.to_string()), search_score))
                        .collect();
                    let mut expected = None;
                    let mut best_score = -1.0;
                    for (candidate, release) in candidates.iter().zip(&releases) {
                        let score = super::candidate_score(&local, candidate, release);
                        assert!(score <= super::candidate_score_upper_bound(&local, candidate));
                        if super::release_matches_local_identity(&local, release)
                            && score > best_score
                        {
                            best_score = score;
                            expected = Some(super::collect_release_artists(release));
                        }
                    }
                    let actual = super::best_matching_release(&local, &candidates, |id| {
                        std::future::ready(Ok(releases[id.parse::<usize>().unwrap()].clone()))
                    })
                    .await
                    .map(|release| super::collect_release_artists(&release));
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn dedupe_key_normalizes_dash_variants() {
        assert_eq!(dedupe_key("Jay-Z"), dedupe_key("Jay‐Z"));
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

    #[test]
    fn two_artist_releases_also_prioritize_the_local_track_artist() {
        let ranked = rank_selected_album_artists(
            &["Guest Artist".to_owned(), "Janine Jansen".to_owned()],
            &["Janine Jansen".to_owned(), "Janine Jansen".to_owned()],
            &[],
            &default_artist_name_map(),
        );
        assert_eq!(ranked, ["Janine Jansen", "Guest Artist"]);
    }
}
