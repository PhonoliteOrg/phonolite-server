//! Release-program evidence for descriptive group metadata. This module never
//! changes source credits; the original album-artist matcher owns that decision.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{Album, ArtistCredit, Track};
use library::Library;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use super::MusicBrainzAlbumArtistResolver;
use crate::{album_lookup, musicbrainz_http};

const MAX_SEARCHES: usize = 12;
const MAX_RELEASE_DETAILS: usize = 12;
const MAX_GROUP_BROWSES: usize = 4;
const MAX_GROUP_DETAILS: usize = 8;
const CACHE_LIMIT: usize = 256;
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_CACHED_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlbumIdentityEvidence {
    pub match_kind: String,
    pub local_track_count: usize,
    pub remote_track_count: usize,
    pub matched_track_count: usize,
    pub strict_credit_eligible: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifiedAlbumIdentity {
    pub release_id: String,
    pub release_group_id: String,
    pub release_title: String,
    pub canonical_title: String,
    pub credited_artists: Vec<ArtistCredit>,
    pub release_artist_credits: Vec<ArtistCredit>,
    pub genres: Vec<String>,
    pub evidence: AlbumIdentityEvidence,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug)]
pub enum AlbumIdentityOutcome {
    Verified(VerifiedAlbumIdentity),
    NoMatch,
    Ambiguous,
}

impl VerifiedAlbumIdentity {
    pub fn to_source_identity(&self) -> library::AlbumSourceIdentity {
        library::AlbumSourceIdentity {
            schema_version: library::ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION,
            context_fingerprint: self.source_fingerprint.clone(),
            musicbrainz_release_id: self.release_id.clone(),
            musicbrainz_release_group_id: self.release_group_id.clone(),
            canonical_title: self.canonical_title.clone(),
            artist_names: self
                .credited_artists
                .iter()
                .map(|c| c.credited_name.clone())
                .collect(),
            artist_ids: self
                .credited_artists
                .iter()
                .map(|c| c.artist_id.clone())
                .collect(),
            verified_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
                .unwrap_or(1),
            evidence: serde_json::to_string(&self.evidence)
                .unwrap_or_else(|_| self.evidence.match_kind.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ProgrammeTrack {
    title: String,
    duration_ms: u32,
    disc: Option<u16>,
    position: Option<u16>,
}

#[derive(Clone, Debug)]
struct IdentityContext {
    title: String,
    names: Vec<String>,
    display: String,
    allow_display_resolution: bool,
    discovery_names: Vec<String>,
    tracks: Vec<ProgrammeTrack>,
    fingerprint: String,
}

fn source_context(
    library: &Library,
    album: &Album,
    tracks: &[Track],
) -> Result<IdentityContext, String> {
    let source = library
        .get_local_album(&album.id)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| album.clone());
    let credits = library
        .raw_album_credits(&album.id)
        .map_err(|e| e.to_string())?;
    // Only explicitly structured local identities are independent name lists.
    // An unstructured display is kept whole and must resolve completely against
    // provider IDs. Prior display repairs must not manufacture trusted names.
    let structured_names: Vec<_> = credits
        .credits
        .iter()
        .filter(|c| c.is_album_artist() && c.source == "local_tags_structured")
        .map(|c| c.name.clone())
        .collect();
    let display = if credits.display_name.trim().is_empty() {
        source.artist_display_name()
    } else {
        credits.display_name.clone()
    };
    let mut names = if structured_names.is_empty() {
        vec![display.clone()]
    } else {
        structured_names.clone()
    };
    let mut discovery_names = names.clone();
    let mut composer_names = Vec::new();
    let mut track_credit_context = Vec::new();
    for track in tracks {
        let mut credit = library
            .raw_track_credits(&track.id)
            .map_err(|e| e.to_string())?;
        credit.credits.retain(|c| {
            matches!(
                c.source.as_str(),
                "local_tags" | "local_tags_structured" | "folder"
            )
        });
        for item in &credit.credits {
            if item.role == "composer" {
                composer_names.push(item.name.clone());
            } else if matches!(item.role.as_str(), "track_artist" | "album_artist") {
                if item.source == "local_tags_structured" {
                    discovery_names.push(item.name.clone());
                }
            }
        }
        if !credit.display_name.trim().is_empty() {
            discovery_names.push(credit.display_name.clone());
        }
        track_credit_context.push(credit);
    }
    for credit in &credits.credits {
        if credit.role == "composer"
            && matches!(
                credit.source.as_str(),
                "local_tags" | "local_tags_structured" | "folder"
            )
        {
            composer_names.push(credit.name.clone());
        }
    }
    // A consistent original song performer is independent evidence for
    // classical folders billed primarily to a composer. Features are excluded.
    let mut independent_performer = None;
    if let Some(first) = track_credit_context
        .first()
        .filter(|c| !c.display_name.trim().is_empty())
    {
        if track_credit_context.iter().all(|c| {
            c.display_name == first.display_name
                && c.credits.iter().any(|v| v.role == "track_artist")
        }) {
            names.push(first.display_name.clone());
            independent_performer = Some(super::normalize_search_text(&first.display_name));
        }
    }
    retain_independent_performers(
        &mut names,
        &composer_names,
        independent_performer.as_deref(),
    );
    names = super::unique_preserve_order(names);
    discovery_names = super::unique_preserve_order(discovery_names);
    composer_names = super::unique_preserve_order(composer_names);
    let fingerprint_payload = json!({
        "policy": 1,
        "title": source.title,
        "year": source.year,
        "original_album_display": display,
        "structured_album_names": structured_names,
        "local_contributors": composer_names,
        "original_track_credits": track_credit_context.iter().map(|c| json!({
            "display": c.display_name,
            "structured_names": c.credits.iter().filter(|v| v.source == "local_tags_structured").map(|v| (&v.role, &v.name)).collect::<Vec<_>>(),
            "local_contributors": c.credits.iter().filter(|v| !v.is_billed_artist()).map(|v| (&v.role, &v.name)).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),
        "tracks": tracks.iter().map(|t| (&t.title, t.duration_ms, t.disc_no, t.track_no)).collect::<Vec<_>>()
    });
    let fingerprint = blake3::hash(fingerprint_payload.to_string().as_bytes())
        .to_hex()
        .to_string();
    Ok(IdentityContext {
        title: source.title,
        names,
        display,
        allow_display_resolution: credits
            .credits
            .iter()
            .filter(|c| c.is_album_artist())
            .all(|c| matches!(c.source.as_str(), "local_tags" | "folder")),
        discovery_names,
        tracks: tracks
            .iter()
            .map(|t| ProgrammeTrack {
                title: t.title.clone(),
                duration_ms: t.duration_ms,
                disc: t.disc_no,
                position: t.track_no,
            })
            .collect(),
        fingerprint,
    })
}

fn retain_independent_performers(
    names: &mut Vec<String>,
    composers: &[String],
    performer_key: Option<&str>,
) {
    names.retain(|name| {
        let name = super::normalize_search_text(name);
        performer_key == Some(name.as_str())
            || !composers.iter().any(|composer| {
                let composer = super::normalize_search_text(composer);
                name == composer || composer.ends_with(&format!(" {name}"))
            })
    });
}

/// Cache keys follow raw source identity and programme evidence, never metadata
/// previously returned by a provider. Library I/O errors are retained as errors.
pub fn identity_source_fingerprint(
    library: &Library,
    album: &Album,
    tracks: &[Track],
) -> Result<String, String> {
    Ok(source_context(library, album, tracks)?.fingerprint)
}

struct CachedPayload {
    at: Instant,
    value: Value,
    bytes: usize,
}

fn payload_cache() -> &'static Mutex<HashMap<String, CachedPayload>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedPayload>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) async fn cached_json(
    resolver: &MusicBrainzAlbumArtistResolver<'_>,
    url: &str,
) -> Result<Value, String> {
    if let Some(value) = payload_cache()
        .lock()
        .get(url)
        .filter(|v| v.at.elapsed() < CACHE_TTL)
        .map(|v| v.value.clone())
    {
        return Ok(value);
    }
    // Only successful public payloads are cached. Failures remain retryable and
    // all actual requests use the shared global pacing/retry policy.
    let value: Value =
        musicbrainz_http::get_json(resolver.client, url, resolver.user_agent, resolver.timeout)
            .await?;
    let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len();
    if bytes > MAX_CACHED_PAYLOAD_BYTES {
        return Ok(value);
    }
    let mut cache = payload_cache().lock();
    cache.retain(|_, v| v.at.elapsed() < CACHE_TTL);
    while cache.len() >= CACHE_LIMIT
        || cache.values().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_CACHE_BYTES
    {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, v)| v.at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        } else {
            break;
        }
    }
    cache.insert(
        url.to_owned(),
        CachedPayload {
            at: Instant::now(),
            value: value.clone(),
            bytes,
        },
    );
    Ok(value)
}

async fn get(
    resolver: &MusicBrainzAlbumArtistResolver<'_>,
    path: &str,
    params: &[(&str, String)],
) -> Result<Value, String> {
    let url = Url::parse_with_params(&format!("https://musicbrainz.org/ws/2/{path}"), params)
        .map_err(|e| e.to_string())?;
    cached_json(resolver, url.as_str()).await
}

fn public_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

fn provider_credits(payload: &Value) -> Option<Vec<ArtistCredit>> {
    let values = payload.get("artist-credit")?.as_array()?;
    if values.is_empty() {
        return None;
    }
    let mut credits = Vec::new();
    let mut aliases = HashMap::new();
    for value in values {
        let artist = value.get("artist")?;
        let id = artist.get("id")?.as_str()?;
        let name = artist.get("name")?.as_str()?.trim();
        let credited_name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .trim();
        if !public_id(id) || name.is_empty() || credited_name.is_empty() {
            return None;
        }
        for label in [name, credited_name] {
            let key = super::normalize_search_text(label);
            if aliases
                .insert(key, id)
                .is_some_and(|previous| previous != id)
            {
                return None;
            }
        }
        credits.push(ArtistCredit {
            artist_id: id.to_owned(),
            name: name.to_owned(),
            credited_name: credited_name.to_owned(),
            role: "album_artist".into(),
            join_phrase: value
                .get("joinphrase")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            source: "musicbrainz".into(),
        });
    }
    Some(credits)
}

fn artist_supported(local: &IdentityContext, credits: &[ArtistCredit]) -> bool {
    let local_names: HashSet<_> = local
        .names
        .iter()
        .map(|v| super::normalize_search_text(v))
        .filter(|v| !v.is_empty())
        .collect();
    let direct = credits.iter().any(|c| {
        [&c.name, &c.credited_name]
            .iter()
            .any(|n| local_names.contains(&super::normalize_search_text(n)))
    });
    direct
        || (!local.names.is_empty()
            && local.allow_display_resolution
            && library::resolve_display_credit(&local.display, credits).is_some())
}

fn fold(value: &str) -> String {
    let plain: String = value.nfkd().filter(|c| !is_combining_mark(*c)).collect();
    super::normalize_search_text(&plain)
}

fn strip_track_labels(value: &str) -> String {
    static LABEL: OnceLock<Regex> = OnceLock::new();
    let re = LABEL.get_or_init(|| Regex::new(r"(?i)\s*\((?:(?:\d{4}\s+)?(?:digital\s+)?remaster(?:ed)?|explicit|album(?:\s+version(?:\s+\(explicit\))?)?|digital\s+version)\)\s*$").unwrap());
    let mut title = value.trim().to_owned();
    for _ in 0..4 {
        let stripped = re.replace(&title, "").trim().to_owned();
        if stripped == title {
            break;
        }
        title = stripped;
    }
    title
}

fn track_key(value: &str) -> String {
    album_lookup::release_title_key(&strip_track_labels(value))
}

fn material_versions(value: &str) -> BTreeSet<String> {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let re = VERSION.get_or_init(|| Regex::new(r"(?i)\b(live|remix|mix|acoustic|instrumental|demo|rerecorded|re recorded|extended|radio edit|mono|stereo)\b").unwrap());
    re.find_iter(&fold(value))
        .map(|v| v.as_str().to_owned())
        .collect()
}

fn duration_matches(local: &ProgrammeTrack, remote: &ProgrammeTrack) -> bool {
    local.duration_ms > 0
        && remote.duration_ms > 0
        && local.duration_ms.abs_diff(remote.duration_ms) <= 3000
}

fn default_single_key(value: &str) -> String {
    // An omitted default single label is only compatible with a duration-backed
    // full programme. Never erase 12-inch, remix, live or extended distinctions.
    static SINGLE: OnceLock<Regex> = OnceLock::new();
    let re = SINGLE.get_or_init(|| {
        Regex::new(r#"(?i)\s*\(7\s*["″\u{2033}]?\s*(?:inch\s+)?version\)\s*$"#).unwrap()
    });
    track_key(&re.replace(&strip_track_labels(value), ""))
}

fn classical_parts(value: &str) -> Option<(BTreeSet<String>, Option<String>)> {
    static CATALOGUE: OnceLock<Regex> = OnceLock::new();
    static NUMBER: OnceLock<Regex> = OnceLock::new();
    static MOVEMENT: OnceLock<Regex> = OnceLock::new();
    let text = fold(value);
    let catalogue =
        CATALOGUE.get_or_init(|| Regex::new(r"\b(rv|op|bwv|k|hob)\s+(\d+[a-z]?)\b").unwrap());
    let number = NUMBER.get_or_init(|| Regex::new(r"\bno\s+(\d+)\b").unwrap());
    let movement = MOVEMENT.get_or_init(|| Regex::new(r"(?i):\s*([ivx]+)\s*[.:(]").unwrap());
    let mut parts = BTreeSet::new();
    for found in catalogue.captures_iter(&text) {
        parts.insert(format!("{}:{}", &found[1], &found[2]));
        if &found[1] == "op" {
            if let Some(no) = number.captures(&text[found.get(0)?.end()..]) {
                parts.insert(format!("op-number:{}", &no[1]));
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let movement = movement.captures(value).map(|m| m[1].to_ascii_lowercase());
    Some((parts, movement))
}

fn classical_track_matches(local: &str, remote: &str) -> bool {
    if !material_versions(local).is_empty() || !material_versions(remote).is_empty() {
        return false;
    }
    match (classical_parts(local), classical_parts(remote)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn programme_track_matches(local: &ProgrammeTrack, remote: &ProgrammeTrack) -> bool {
    if !duration_matches(local, remote)
        || material_versions(&local.title) != material_versions(&remote.title)
    {
        return false;
    }
    if super::normalize_search_text(&local.title) == super::normalize_search_text(&remote.title) {
        return true;
    }
    track_key(&local.title) == track_key(&remote.title)
        || default_single_key(&local.title) == default_single_key(&remote.title)
        || classical_track_matches(&local.title, &remote.title)
}

fn remote_tracks(release: &Value) -> Option<Vec<ProgrammeTrack>> {
    let media = release.get("media")?.as_array()?;
    let mut result = Vec::new();
    for (disc_index, medium) in media.iter().enumerate() {
        let tracks = medium.get("tracks")?.as_array()?;
        if medium
            .get("track-count")
            .and_then(Value::as_u64)
            .is_some_and(|n| n as usize != tracks.len())
        {
            return None;
        }
        for (index, track) in tracks.iter().enumerate() {
            let recording = track.get("recording").unwrap_or(&Value::Null);
            let title = track
                .get("title")
                .or_else(|| recording.get("title"))?
                .as_str()?
                .trim();
            if title.is_empty() {
                return None;
            }
            result.push(ProgrammeTrack {
                title: title.into(),
                duration_ms: track
                    .get("length")
                    .or_else(|| recording.get("length"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
                disc: Some(
                    medium
                        .get("position")
                        .and_then(Value::as_u64)
                        .and_then(|n| u16::try_from(n).ok())
                        .unwrap_or((disc_index + 1) as u16),
                ),
                position: Some(
                    track
                        .get("position")
                        .and_then(Value::as_u64)
                        .and_then(|n| u16::try_from(n).ok())
                        .unwrap_or((index + 1) as u16),
                ),
            });
        }
    }
    (!result.is_empty()).then_some(result)
}

fn has_classical_programme(local: &IdentityContext) -> bool {
    !local.tracks.is_empty()
        && local
            .tracks
            .iter()
            .filter(|t| classical_parts(&t.title).is_some())
            .count()
            * 5
            >= local.tracks.len() * 4
}

fn classical_title_supported(local: &IdentityContext, credits: &[ArtistCredit]) -> bool {
    if !has_classical_programme(local) {
        return false;
    }
    let Some((composer, _)) = local.title.split_once(':') else {
        return false;
    };
    let key = fold(composer);
    if key.is_empty() {
        return false;
    }
    credits.iter().any(|credit| {
        [&credit.name, &credit.credited_name].iter().any(|name| {
            let remote = fold(name);
            remote == key || remote.ends_with(&format!(" {key}"))
        })
    })
}

fn validate_programme(
    local: &IdentityContext,
    release: &Value,
) -> Result<AlbumIdentityEvidence, &'static str> {
    let credits =
        provider_credits(release).ok_or("missing or conflicting structured artist IDs")?;
    if !artist_supported(local, &credits) {
        return Err("local performer or complete display credit not supported");
    }
    let title = release
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title_equivalent = album_lookup::titles_equivalent(&local.title, title);
    if material_versions(&local.title) != material_versions(title) {
        return Err("material album version mismatch");
    }
    let classical = classical_title_supported(local, &credits);
    if !title_equivalent && !classical {
        return Err("album title mismatch");
    }
    let remote = remote_tracks(release).ok_or("missing or incomplete remote programme")?;
    if local.tracks.is_empty() {
        return Err("no local programme evidence");
    }
    let strict = local.tracks.len() == remote.len()
        && local.tracks.iter().zip(&remote).all(|(a, b)| {
            super::normalize_search_text(&a.title) == super::normalize_search_text(&b.title)
        });
    if local.tracks.len() == remote.len()
        && local
            .tracks
            .iter()
            .zip(&remote)
            .all(|(a, b)| programme_track_matches(a, b))
    {
        // A classical work-only discovery requires every duration, not merely
        // repeated opus titles shared by many performances.
        if !title_equivalent
            && !local
                .tracks
                .iter()
                .zip(&remote)
                .all(|(a, b)| duration_matches(a, b))
        {
            return Err("classical duration evidence missing or conflicting");
        }
        return Ok(AlbumIdentityEvidence {
            match_kind: if strict {
                "complete_ordered_titles"
            } else if classical {
                "complete_classical_programme"
            } else {
                "complete_duration_backed_programme"
            }
            .into(),
            local_track_count: local.tracks.len(),
            remote_track_count: remote.len(),
            matched_track_count: local.tracks.len(),
            strict_credit_eligible: strict,
        });
    }
    // A small explicitly numbered gap is different from an expanded local
    // edition with absent provider bonus tracks. All local files must match.
    if title_equivalent
        && remote.len() > local.tracks.len()
        && remote.len() - local.tracks.len() <= 3
        && local.tracks.len() * 5 >= remote.len() * 4
    {
        let mut used = HashSet::new();
        let mut previous = None;
        let all = local.tracks.iter().all(|track| {
            let Some(position) = track.position else {
                return false;
            };
            let disc = track.disc.unwrap_or(1);
            let matches: Vec<_> = remote
                .iter()
                .enumerate()
                .filter(|(_, t)| t.position == Some(position) && t.disc == Some(disc))
                .collect();
            if matches.len() != 1 {
                return false;
            }
            let (index, remote_track) = matches[0];
            if previous.is_some_and(|p| index <= p)
                || !used.insert(index)
                || !duration_matches(track, remote_track)
                || !programme_track_matches(track, remote_track)
            {
                return false;
            }
            previous = Some(index);
            true
        });
        if all {
            return Ok(AlbumIdentityEvidence {
                match_kind: "numbered_duration_backed_subset".into(),
                local_track_count: local.tracks.len(),
                remote_track_count: remote.len(),
                matched_track_count: local.tracks.len(),
                strict_credit_eligible: false,
            });
        }
    }
    Err("track programme, order, version or duration mismatch")
}

fn artist_clause(local: &IdentityContext) -> Option<String> {
    let names: Vec<_> = local
        .discovery_names
        .iter()
        .flat_map(|name| album_lookup::artist_name_variants(name))
        .filter(|name| super::usable_artist_hint(name))
        .take(8)
        .filter_map(|name| super::query_term("artist", &name))
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(format!("({})", names.join(" OR ")))
    }
}

fn discovery_titles(local: &IdentityContext) -> Vec<String> {
    let variants = album_lookup::title_candidates(&local.title);
    let mut titles = vec![local.title.clone()];
    let base = album_lookup::strip_release_qualifiers(&local.title);
    if !base.is_empty() && !titles.contains(&base) {
        titles.push(base.clone());
    }
    if has_classical_programme(local) {
        if let Some((_, work)) = base.split_once(':') {
            let work = work.trim().to_owned();
            if !work.is_empty() && !titles.contains(&work) {
                titles.push(work);
            }
        }
    }
    for variant in variants {
        if !titles.contains(&variant) && titles.len() < 4 {
            titles.push(variant);
        }
    }
    titles.truncate(4);
    titles
}

fn queries(local: &IdentityContext) -> Vec<(bool, String)> {
    let Some(artist) = artist_clause(local) else {
        return Vec::new();
    };
    let titles = discovery_titles(local);
    let mut result = Vec::new();
    // Reserve each title a count-constrained release, uncounted release and
    // group stage. A title variant cannot consume the entire fallback budget.
    for title in &titles {
        if let Some(title) = super::query_term("release", title) {
            result.push((
                false,
                format!("{title} AND {artist} AND tracks:{}", local.tracks.len()),
            ));
        }
        if let Some(title) = super::query_term("release", title) {
            result.push((false, format!("{title} AND {artist}")));
        }
        if let Some(title) = super::query_term("releasegroup", title) {
            result.push((true, format!("{title} AND {artist}")));
        }
    }
    result.truncate(MAX_SEARCHES);
    result
}

fn candidate_track_count(release: &Value) -> usize {
    release
        .get("track-count")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .or_else(|| {
            release.get("media").and_then(Value::as_array).map(|m| {
                m.iter()
                    .filter_map(|v| v.get("track-count").and_then(Value::as_u64))
                    .sum::<u64>() as usize
            })
        })
        .unwrap_or(0)
}

fn group_payload_agrees(left: &Value, right: &Value) -> bool {
    left.get("id") == right.get("id")
        && left
            .get("title")
            .and_then(Value::as_str)
            .zip(right.get("title").and_then(Value::as_str))
            .is_some_and(|(a, b)| {
                super::normalize_search_text(a) == super::normalize_search_text(b)
            })
        && provider_credits(left)
            .zip(provider_credits(right))
            .is_some_and(|(a, b)| {
                a.iter()
                    .map(|c| &c.artist_id)
                    .eq(b.iter().map(|c| &c.artist_id))
            })
}

fn group_is_bound(release: &Value, group: &Value, canonical_redirect_verified: bool) -> bool {
    let Some(embedded) = release.get("release-group") else {
        return false;
    };
    let same_id = embedded.get("id") == group.get("id");
    let titles_agree = embedded
        .get("title")
        .and_then(Value::as_str)
        .zip(group.get("title").and_then(Value::as_str))
        .is_some_and(|(a, b)| super::normalize_search_text(a) == super::normalize_search_text(b))
        || (canonical_redirect_verified
            && embedded
                .get("title")
                .and_then(Value::as_str)
                .zip(release.get("title").and_then(Value::as_str))
                .is_some_and(|(a, b)| album_lookup::titles_equivalent(a, b)));
    let credits_agree = if embedded.get("artist-credit").is_some() {
        provider_credits(embedded)
            .zip(provider_credits(group))
            .is_some_and(|(a, b)| {
                a.iter()
                    .map(|c| &c.artist_id)
                    .eq(b.iter().map(|c| &c.artist_id))
            })
    } else {
        true
    };
    (same_id || canonical_redirect_verified) && titles_agree && credits_agree
}

async fn identity_from_release(
    resolver: &MusicBrainzAlbumArtistResolver<'_>,
    local: &IdentityContext,
    release: &Value,
    group_lookups: &mut HashSet<String>,
    group_budget_exhausted: &mut bool,
) -> Result<Option<VerifiedAlbumIdentity>, String> {
    let mut evidence = match validate_programme(local, release) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(release_id) = release
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| public_id(id))
    else {
        return Ok(None);
    };
    let Some(group_id) = release
        .get("release-group")
        .and_then(|g| g.get("id"))
        .and_then(Value::as_str)
        .filter(|id| public_id(id))
    else {
        return Ok(None);
    };
    if !group_lookups.contains(group_id) {
        if group_lookups.len() >= MAX_GROUP_DETAILS {
            *group_budget_exhausted = true;
            return Ok(None);
        }
        group_lookups.insert(group_id.to_owned());
    }
    let group = get(
        resolver,
        &format!("release-group/{group_id}"),
        &[
            ("fmt", "json".into()),
            ("inc", "genres+artist-credits".into()),
        ],
    )
    .await?;
    let Some(canonical_group_id) = group
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| public_id(id))
    else {
        return Ok(None);
    };
    let redirect_verified = if canonical_group_id == group_id {
        false
    } else {
        if !group_lookups.contains(canonical_group_id) {
            if group_lookups.len() >= MAX_GROUP_DETAILS {
                *group_budget_exhausted = true;
                return Ok(None);
            }
            group_lookups.insert(canonical_group_id.to_owned());
        }
        let canonical = get(
            resolver,
            &format!("release-group/{canonical_group_id}"),
            &[
                ("fmt", "json".into()),
                ("inc", "genres+artist-credits".into()),
            ],
        )
        .await?;
        group_payload_agrees(&group, &canonical)
    };
    if !group_is_bound(release, &group, redirect_verified) {
        return Ok(None);
    }
    let Some(credited_artists) = provider_credits(&group) else {
        return Ok(None);
    };
    if !artist_supported(local, &credited_artists) {
        return Ok(None);
    }
    let Some(canonical_title) = group
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
    else {
        return Ok(None);
    };
    // Metadata programme evidence never loosens the original credit-repair
    // contract. Keep that eligibility distinct in the shared result.
    let legacy_local = super::LocalAlbumInfo {
        allow_display_resolution: local.allow_display_resolution,
        album_hint: local.title.clone(),
        artist_hint: local.display.clone(),
        local_artist_names: local.names.clone(),
        year_hint: None,
        track_artists: Vec::new(),
        track_titles: local.tracks.iter().map(|t| t.title.clone()).collect(),
        track_count: local.tracks.len(),
    };
    evidence.strict_credit_eligible = evidence.strict_credit_eligible
        && serde_json::from_value::<super::ReleaseDetail>(release.clone())
            .ok()
            .is_some_and(|remote| {
                super::release_identity_rejection(&legacy_local, &remote).is_none()
            });
    let genres = group
        .get("genres")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("name").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Some(VerifiedAlbumIdentity {
        release_id: release_id.into(),
        release_group_id: canonical_group_id.into(),
        release_title: release
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        canonical_title: canonical_title.into(),
        credited_artists,
        release_artist_credits: provider_credits(release).unwrap_or_default(),
        genres,
        evidence,
        source_fingerprint: local.fingerprint.clone(),
    }))
}

impl MusicBrainzAlbumArtistResolver<'_> {
    /// Keep known ambiguity distinct so callers can suppress less specific
    /// provider fallback instead of hiding conflicting verified programmes.
    pub async fn resolve_album_identity_outcome(
        &self,
        library: &Library,
        album: &Album,
        tracks: &[Track],
    ) -> Result<AlbumIdentityOutcome, String> {
        if self.user_agent.trim().is_empty() {
            return Ok(AlbumIdentityOutcome::NoMatch);
        }
        let local = source_context(library, album, tracks)?;
        if local.tracks.is_empty() || local.names.is_empty() {
            return Ok(AlbumIdentityOutcome::NoMatch);
        }
        let mut seen_releases = HashSet::new();
        let mut seen_groups = HashSet::new();
        let mut detail_count = 0;
        let mut verified = HashMap::<String, VerifiedAlbumIdentity>::new();
        let mut truncated = false;
        let mut unexamined_candidates = 0usize;
        let mut group_lookups = HashSet::new();
        let mut group_budget_exhausted = false;
        let mut rejections = HashMap::<&'static str, usize>::new();
        for (group_search, query) in queries(&local) {
            let path = if group_search {
                "release-group"
            } else {
                "release"
            };
            let payload = get(
                self,
                path,
                &[
                    ("query", query.clone()),
                    ("fmt", "json".into()),
                    ("limit", self.options.mb_limit.clamp(1, 100).to_string()),
                ],
            )
            .await?;
            let field = if group_search {
                "release-groups"
            } else {
                "releases"
            };
            let mut candidates = payload
                .get(field)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let returned_candidates = candidates.len();
            let provider_count = payload
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(returned_candidates as u64);
            if provider_count > returned_candidates as u64 {
                truncated = true;
            }
            candidates.sort_by_key(|c| {
                (
                    usize::from(candidate_track_count(c) != local.tracks.len()),
                    usize::from(!album_lookup::titles_equivalent(
                        &local.title,
                        c.get("title").and_then(Value::as_str).unwrap_or_default(),
                    )),
                )
            });
            let mut programmes = Vec::new();
            if group_search {
                for group in candidates {
                    let Some(id) = group
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| public_id(id))
                    else {
                        continue;
                    };
                    let Some(credits) = provider_credits(&group) else {
                        continue;
                    };
                    if !artist_supported(&local, &credits) {
                        continue;
                    }
                    if seen_groups.contains(id) {
                        continue;
                    }
                    if seen_groups.len() >= MAX_GROUP_BROWSES {
                        truncated = true;
                        unexamined_candidates += 1;
                        continue;
                    }
                    seen_groups.insert(id.to_owned());
                    let browse = get(
                        self,
                        "release",
                        &[
                            ("release-group", id.into()),
                            ("fmt", "json".into()),
                            ("inc", "recordings+artist-credits+release-groups".into()),
                            ("limit", "100".into()),
                        ],
                    )
                    .await?;
                    if browse
                        .get("release-count")
                        .and_then(Value::as_u64)
                        .is_some_and(|count| {
                            count
                                > browse
                                    .get("releases")
                                    .and_then(Value::as_array)
                                    .map(|r| r.len())
                                    .unwrap_or(0) as u64
                        })
                    {
                        truncated = true;
                    }
                    for programme in browse
                        .get("releases")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if programme
                            .get("release-group")
                            .and_then(|g| g.get("id"))
                            .and_then(Value::as_str)
                            != Some(id)
                        {
                            *rejections
                                .entry("browsed release did not belong to requested group")
                                .or_default() += 1;
                            continue;
                        }
                        programmes.push(programme.clone());
                    }
                }
            } else {
                let mut stage_details = 0;
                for candidate in candidates {
                    let Some(id) = candidate
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| public_id(id))
                    else {
                        continue;
                    };
                    if let Some(credits) = provider_credits(&candidate) {
                        if !artist_supported(&local, &credits) {
                            continue;
                        }
                    }
                    if seen_releases.contains(id) {
                        continue;
                    }
                    if detail_count >= MAX_RELEASE_DETAILS || stage_details >= 3 {
                        truncated = true;
                        unexamined_candidates += 1;
                        continue;
                    }
                    seen_releases.insert(id.to_owned());
                    // Search credits and programme counts are discovery hints;
                    // the lookup payload supplies identity evidence.
                    detail_count += 1;
                    stage_details += 1;
                    let detail = get(
                        self,
                        &format!("release/{id}"),
                        &[
                            ("fmt", "json".into()),
                            ("inc", "recordings+artist-credits+release-groups".into()),
                        ],
                    )
                    .await?;
                    if detail.get("id").and_then(Value::as_str) != Some(id) {
                        *rejections
                            .entry("release lookup ID did not match the discovery candidate")
                            .or_default() += 1;
                        continue;
                    }
                    programmes.push(detail);
                }
            }
            for programme in programmes {
                match validate_programme(&local, &programme) {
                    Ok(_) => {
                        if let Some(identity) = identity_from_release(
                            self,
                            &local,
                            &programme,
                            &mut group_lookups,
                            &mut group_budget_exhausted,
                        )
                        .await?
                        {
                            verified
                                .entry(identity.release_group_id.clone())
                                .and_modify(|previous| {
                                    if identity.evidence.strict_credit_eligible
                                        && !previous.evidence.strict_credit_eligible
                                    {
                                        *previous = identity.clone();
                                    }
                                })
                                .or_insert(identity);
                        }
                    }
                    Err(reason) => {
                        *rejections.entry(reason).or_default() += 1;
                    }
                }
            }
            tracing::info!(provider="musicbrainz", kind="album_identity", album_id=%album.id, album=%local.title, search_query=%query,
                returned_candidates, provider_count, truncated, unexamined_candidates, verified_groups=verified.len(), release_detail_requests=detail_count, group_browses=seen_groups.len(), "MusicBrainz album identity discovery stage completed");
            if verified.len() > 1 {
                tracing::info!(provider="musicbrainz", kind="album_identity", album_id=%album.id, reason="ambiguous_verified_programmes", group_count=verified.len(), "MusicBrainz album identity was not selected");
                return Ok(AlbumIdentityOutcome::Ambiguous);
            }
        }
        truncated |= group_budget_exhausted;
        if group_budget_exhausted {
            tracing::info!(provider="musicbrainz", kind="album_identity", album_id=%album.id, reason="group_detail_budget_exhausted_without_unique_identity", group_details=group_lookups.len(), "MusicBrainz album identity remains inconclusive");
            return Ok(AlbumIdentityOutcome::NoMatch);
        }
        if let Some(identity) = verified.into_values().next() {
            tracing::info!(provider="musicbrainz", kind="album_identity", album_id=%album.id, release_id=%identity.release_id,
                release_group_id=%identity.release_group_id, evidence=%identity.evidence.match_kind, truncated, unexamined_candidates,
                "MusicBrainz album group identity verified against local programme within the bounded search");
            return Ok(AlbumIdentityOutcome::Verified(identity));
        }
        let mut reasons: Vec<_> = rejections.into_iter().collect();
        reasons.sort_unstable_by_key(|(reason, _)| *reason);
        tracing::info!(provider="musicbrainz", kind="album_identity", album_id=%album.id, album=%local.title,
            reason=if truncated { "search_budget_exhausted_without_verified_identity" } else { "no_verified_programme" }, truncated, unexamined_candidates, rejection_counts=?reasons,
            release_detail_requests=detail_count, group_browses=seen_groups.len(), "MusicBrainz album identity was not selected");
        Ok(AlbumIdentityOutcome::NoMatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIST: &str = "070d193a-845c-479f-980e-bef15710653e";

    fn fixture(title: &str, track_titles: &[&str]) -> (IdentityContext, Value) {
        let local = IdentityContext {
            title: title.into(),
            names: vec!["Known Performer".into()],
            display: "Known Performer".into(),
            allow_display_resolution: false,
            discovery_names: vec!["Known Performer".into()],
            tracks: track_titles
                .iter()
                .enumerate()
                .map(|(i, t)| ProgrammeTrack {
                    title: (*t).into(),
                    duration_ms: 200_000 + i as u32 * 1000,
                    disc: Some(1),
                    position: Some(i as u16 + 1),
                })
                .collect(),
            fingerprint: "fixture".into(),
        };
        let remote = json!({"title":title,"artist-credit":[{"name":"Known Performer","artist":{"id":ARTIST,"name":"Known Performer"}}],
            "media":[{"position":1,"track-count":track_titles.len(),"tracks":track_titles.iter().enumerate().map(|(i,t)|json!({"position":i+1,"title":t,"length":200_000+i*1000})).collect::<Vec<_>>()}]});
        (local, remote)
    }

    #[test]
    fn duration_backed_labels_preserve_mix_and_performer_boundaries() {
        let (local, mut remote) = fixture("New Album (Deluxe)", &["Song (2025 Remaster)", "Other"]);
        remote["title"] = json!("New Album");
        remote["media"][0]["tracks"][0]["title"] = json!("Song");
        let evidence = validate_programme(&local, &remote).unwrap();
        assert!(!evidence.strict_credit_eligible);
        remote["media"][0]["tracks"][0]["title"] = json!("Song (Live)");
        assert!(validate_programme(&local, &remote).is_err());
        remote["media"][0]["tracks"][0]["title"] = json!("Song");
        remote["artist-credit"][0]["artist"]["name"] = json!("Another Performer");
        remote["artist-credit"][0]["name"] = json!("Another Performer");
        assert!(validate_programme(&local, &remote).is_err());
    }

    #[test]
    fn same_title_and_count_cannot_authenticate_reordered_or_expanded_files() {
        let (local, mut remote) = fixture("New Album", &["First", "Second", "Third"]);
        remote["media"][0]["tracks"][0]["title"] = json!("Second");
        assert!(validate_programme(&local, &remote).is_err());
        remote["media"][0]["tracks"] = json!([{"title":"First","length":200000}]);
        remote["media"][0]["track-count"] = json!(1);
        assert!(validate_programme(&local, &remote).is_err());
    }

    #[test]
    fn missing_numbered_track_can_only_establish_group_metadata() {
        let (mut local, remote) =
            fixture("New Album", &["One", "Two", "Three", "Four", "Five", "Six"]);
        local.tracks.remove(2);
        let evidence = validate_programme(&local, &remote).unwrap();
        assert_eq!(evidence.match_kind, "numbered_duration_backed_subset");
        assert!(!evidence.strict_credit_eligible);
        for track in &mut local.tracks {
            track.position = None;
        }
        assert!(validate_programme(&local, &remote).is_err());
    }

    #[test]
    fn classical_catalogue_and_movement_need_complete_duration_evidence() {
        let (local, mut remote) = fixture(
            "Vivaldi: Seasons",
            &[
                "Concerto RV 269, Op. 8 No. 1: I. Allegro",
                "Concerto RV 269, Op. 8 No. 1: II. Largo",
            ],
        );
        remote["artist-credit"].as_array_mut().unwrap().push(json!({"name":"Vivaldi","artist":{"id":"4c8ead39-b9df-4c56-a27c-51bc049cfd48","name":"Vivaldi"}}));
        remote["title"] = json!("Seasons");
        remote["media"][0]["tracks"][0]["title"] =
            json!("Le stagioni Op. 8 No. 1, RV 269: I. Allegro");
        remote["media"][0]["tracks"][1]["title"] =
            json!("Le stagioni Op. 8 No. 1, RV 269: II. Adagio");
        assert!(validate_programme(&local, &remote).is_ok());
        remote["media"][0]["tracks"][1]["length"] = json!(240000);
        assert!(validate_programme(&local, &remote).is_err());
        remote["media"][0]["tracks"][1]["length"] = json!(201000);
        remote["media"][0]["tracks"][1]["title"] =
            json!("Le stagioni Op. 8 No. 1, RV 269: III. Allegro");
        assert!(validate_programme(&local, &remote).is_err());
    }

    #[test]
    fn conflicting_alias_ids_and_incomplete_artist_evidence_are_rejected() {
        let (_, mut remote) = fixture("New Album", &["One"]);
        let mut conflicting = remote["artist-credit"][0].clone();
        conflicting["artist"]["id"] = json!("4c8ead39-b9df-4c56-a27c-51bc049cfd48");
        remote["artist-credit"]
            .as_array_mut()
            .unwrap()
            .push(conflicting);
        assert!(provider_credits(&remote).is_none());
        remote["artist-credit"][1]["artist"]["id"] = json!("");
        assert!(provider_credits(&remote).is_none());
    }

    #[test]
    fn identical_titles_cannot_hide_different_performance_durations() {
        let (local, mut remote) = fixture("New Album", &["First", "Second"]);
        remote["media"][0]["tracks"][1]["length"] = json!(501000);
        assert!(validate_programme(&local, &remote).is_err());
    }

    #[test]
    fn original_base_and_classical_work_each_retain_discovery_budget() {
        let (mut local, _) = fixture(
            "Chopin : Nocturnes (1999 remastered)",
            &["Nocturnes Op. 9 No. 1"],
        );
        local.discovery_names = vec!["Arthur Rubinstein".into()];
        let planned = queries(&local);
        assert!(planned.len() <= MAX_SEARCHES);
        assert!(planned
            .iter()
            .any(|(group, query)| *group && query.contains("releasegroup:\"Nocturnes\"")));
        assert!(planned
            .iter()
            .any(|(group, query)| !*group && query.contains("release:\"Nocturnes\"")));
    }

    #[test]
    fn group_binding_requires_declared_identity_and_verified_redirect() {
        let (_, mut release) = fixture("Side B (Deluxe)", &["One"]);
        let group = json!({"id":"bc217c64-db43-4311-9b73-fca6d25c4ac5","title":"Parent Album","artist-credit":release["artist-credit"]});
        release["release-group"] =
            json!({"id":"23b13e7b-ad85-4aaa-9441-6707503efe11","title":"Side B (Deluxe)"});
        assert!(!group_is_bound(&release, &group, false));
        assert!(group_is_bound(&release, &group, true));
        release["release-group"]["title"] = json!("Unrelated Album");
        assert!(!group_is_bound(&release, &group, true));
        assert!(!public_id("00000000-0000-0000-0000-000000000000"));
        assert!(!public_id("BC217C64-DB43-4311-9B73-FCA6D25C4AC5"));
    }

    #[test]
    fn captured_seventeen_programmes_replay_without_http() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/musicbrainz_programmes.json"
        ))
        .unwrap();
        let cases = fixture["albums"].as_array().unwrap();
        assert_eq!(cases.len(), 17);
        for case in cases {
            let local = IdentityContext {
                title: case["album"].as_str().unwrap().into(),
                names: serde_json::from_value(case["names"].clone()).unwrap(),
                display: case["display"].as_str().unwrap().into(),
                allow_display_resolution: case["allow_display_resolution"].as_bool().unwrap(),
                discovery_names: serde_json::from_value(case["names"].clone()).unwrap(),
                tracks: serde_json::from_value(case["local_tracks"].clone()).unwrap(),
                fingerprint: "captured-fixture".into(),
            };
            let results: Vec<_> = case["provider_releases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| validate_programme(&local, r))
                .collect();
            let accepted = results.iter().any(Result::is_ok);
            assert_eq!(
                accepted,
                case["expected_accepted"].as_bool().unwrap(),
                "{}: {:?}",
                local.title,
                results
            );
            // Changing the independent performer must reject the very same
            // programme and composer/catalogue identity.
            let mut wrong_performer = local.clone();
            wrong_performer.names = vec!["Unmentioned Different Performer".into()];
            wrong_performer.display = "Unmentioned Different Performer".into();
            assert!(case["provider_releases"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| validate_programme(&wrong_performer, r).is_err()));
        }
        for control in fixture["wrong_controls"].as_array().unwrap() {
            let case = cases
                .iter()
                .find(|c| c["album"] == control["context_album"])
                .unwrap();
            let local = IdentityContext {
                title: case["album"].as_str().unwrap().into(),
                names: serde_json::from_value(case["names"].clone()).unwrap(),
                display: case["display"].as_str().unwrap().into(),
                allow_display_resolution: case["allow_display_resolution"].as_bool().unwrap(),
                discovery_names: serde_json::from_value(case["names"].clone()).unwrap(),
                tracks: serde_json::from_value(case["local_tracks"].clone()).unwrap(),
                fingerprint: "captured-negative-fixture".into(),
            };
            assert!(
                validate_programme(&local, &control["provider_release"]).is_err(),
                "{}",
                control["expected_rejection"]
            );
        }
    }

    #[test]
    fn composer_performer_dual_role_is_independent_but_composer_only_is_not() {
        let mut names = vec!["Known Performer".into()];
        retain_independent_performers(
            &mut names,
            &["Known Performer".into()],
            Some("known performer"),
        );
        assert_eq!(names, ["Known Performer"]);
        let mut names = vec!["Chopin".into(), "Arthur Rubinstein".into()];
        retain_independent_performers(
            &mut names,
            &["Frédéric Chopin".into()],
            Some("arthur rubinstein"),
        );
        assert_eq!(names, ["Arthur Rubinstein"]);
    }
}
