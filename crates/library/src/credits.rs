//! Independent source credits and artist-level release relationships.
use std::collections::{HashMap, HashSet};

use common::{artist_identity_id, artist_identity_key, Album, Artist, ArtistCredit, ArtistCredits};
use redb::{Database, ReadableTable, TableDefinition, TableError};

use crate::storage::{
    decode_value, ALBUMS_TABLE, ALBUM_CREDITS_TABLE, TRACKS_TABLE, TRACK_CREDITS_TABLE,
};
use crate::{ArtistReleaseContribution, ArtistReleaseRelationships, Library, LibraryError};

pub(super) const DISPLAY_CREDIT_RESOLUTIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("display_credit_resolutions_v1");

pub(super) fn display_credit_key(value: &str) -> String {
    // Punctuation is meaningful here: a list and a band's name can contain
    // the same words. Artist identity normalization would conflate them.
    value.trim().to_lowercase()
}

pub(super) fn display_credit_resolutions(
    db: &Database,
) -> Result<HashMap<String, ArtistCredits>, LibraryError> {
    let read = db.begin_read()?;
    let table = match read.open_table(DISPLAY_CREDIT_RESOLUTIONS) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };
    let resolutions = table
        .iter()?
        .map(|entry| {
            let (key, value) = entry?;
            Ok((key.value().to_owned(), decode_value(value.value())?))
        })
        .collect();
    resolutions
}

fn raw_credits(
    db: &Database,
    definition: TableDefinition<&str, &[u8]>,
    media_id: &str,
) -> Result<ArtistCredits, LibraryError> {
    let read = db.begin_read()?;
    let table = match read.open_table(definition) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(ArtistCredits::default()),
        Err(err) => return Err(err.into()),
    };
    let result = match table.get(media_id)? {
        Some(value) => decode_value(value.value()),
        None => Ok(ArtistCredits::default()),
    };
    result
}

pub(super) fn resolved_credit_names(
    display: Option<&str>,
    explicit: &[String],
    structured: bool,
    known: &HashSet<String>,
    resolutions: &HashMap<String, ArtistCredits>,
) -> Vec<String> {
    if !structured && explicit.len() <= 1 {
        if let Some(resolved) = display
            .or_else(|| explicit.first().map(String::as_str))
            .and_then(|value| resolutions.get(&display_credit_key(value)))
        {
            return resolved
                .credits
                .iter()
                .map(|credit| credit.name.clone())
                .collect();
        }
    }
    credit_names(display, explicit, structured, known)
}

pub(super) fn credit_names(
    display: Option<&str>,
    explicit: &[String],
    structured: bool,
    known: &HashSet<String>,
) -> Vec<String> {
    // Explicit identity tags (including a single band with punctuation) win.
    // A lone ARTIST/ALBUMARTIST value is a display credit, not such a tag.
    let values = if structured || explicit.len() > 1 {
        explicit.to_vec()
    } else {
        display
            .or_else(|| explicit.first().map(String::as_str))
            .into_iter()
            .flat_map(|value| split_display_credit(value, known))
            .collect()
    };
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|name| !name.is_empty() && seen.insert(artist_identity_key(name)))
        .collect()
}

fn split_display_credit(value: &str, known: &HashSet<String>) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if known.contains(&artist_identity_key(value)) {
        return vec![value.to_owned()];
    }
    // Match complete identities before interpreting their punctuation as joins.
    // This also preserves a band inside a larger comma-delimited credit.
    if let Some(candidates) = match_known_credit(value, known) {
        return candidates;
    }
    let lower = value.to_ascii_lowercase();
    for join in [" featuring ", " feat. ", " feat ", " ft. ", " ft "] {
        if let Some(index) = lower.find(join) {
            let left = value[..index].trim_end().trim_end_matches('(').trim();
            let mut right = value[index + join.len()..].trim();
            if value[..index].trim_end().ends_with('(') {
                right = right.trim_end_matches(')').trim();
            }
            if !left.is_empty() && !right.is_empty() {
                return [left, right]
                    .into_iter()
                    .flat_map(|part| split_display_credit(part, known))
                    .collect();
            }
        }
    }
    if value.contains([';', '\0']) {
        return value
            .split([';', '\0'])
            .flat_map(|part| split_display_credit(part, known))
            .collect();
    }
    // Plain comma lists are a widespread unstructured artist tag format.
    // Mixed conjunction names and comma-stage names remain one identity unless
    // structured tags or independent artist evidence identifies their parts.
    if value.contains(',')
        && !lower.contains(" & ")
        && !lower.contains(" and ")
        && !lower.contains(" + ")
        && !lower.contains(" with ")
        && !lower.contains(", the ")
    {
        let parts: Vec<_> = value.split(',').map(str::trim).collect();
        if parts.iter().all(|part| !part.is_empty()) {
            return parts.into_iter().map(str::to_owned).collect();
        }
    }
    vec![value.to_owned()]
}

fn match_known_credit(value: &str, known: &HashSet<String>) -> Option<Vec<String>> {
    if value.is_empty() || value.len() > 8192 {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let mut boundaries = Vec::new();
    for join in [
        ",",
        ";",
        "\0",
        " & ",
        " and ",
        " x ",
        " / ",
        " + ",
        " | ",
        " with ",
        " conducting the ",
        " vs. ",
        " vs ",
        " featuring ",
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
    ] {
        boundaries.extend(
            lower
                .match_indices(join)
                .map(|(start, _)| (start, start + join.len())),
        );
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    if boundaries.len() > 128 {
        return None;
    }
    fn visit(
        value: &str,
        start: usize,
        boundaries: &[(usize, usize)],
        known: &HashSet<String>,
        memo: &mut HashMap<usize, Option<Vec<String>>>,
    ) -> Option<Vec<String>> {
        if let Some(result) = memo.get(&start) {
            return result.clone();
        }
        let tail = value[start..].trim();
        if !tail.is_empty() && known.contains(&artist_identity_key(tail)) {
            return Some(vec![tail.to_owned()]);
        }
        // Longest complete name wins, provided the remaining credit also
        // resolves. Memoization bounds work for long multi-artist listings.
        for &(end, next) in boundaries.iter().rev().filter(|(end, _)| *end > start) {
            let name = value[start..end].trim();
            if known.contains(&artist_identity_key(name)) {
                if let Some(mut rest) = visit(value, next, boundaries, known, memo) {
                    rest.insert(0, name.to_owned());
                    memo.insert(start, Some(rest.clone()));
                    return Some(rest);
                }
            }
        }
        memo.insert(start, None);
        None
    }
    visit(value, 0, &boundaries, known, &mut HashMap::new())
}

/// Resolve a display against the identities of one verified provider credit.
/// Provider IDs are used for deduplication; aliases cannot create new artists.
pub fn resolve_display_credit(value: &str, credits: &[ArtistCredit]) -> Option<Vec<ArtistCredit>> {
    if credits.is_empty()
        || credits
            .iter()
            .any(|credit| credit.artist_id.trim().is_empty() || credit.name.trim().is_empty())
    {
        return None;
    }
    let mut identities = HashMap::new();
    let mut ambiguous = HashSet::new();
    for credit in credits {
        for name in [&credit.name, &credit.credited_name] {
            if name.trim().is_empty() {
                continue;
            }
            let key = artist_identity_key(name);
            if identities
                .get(&key)
                .is_some_and(|old: &&ArtistCredit| old.artist_id != credit.artist_id)
            {
                ambiguous.insert(key.clone());
            }
            identities.insert(key, credit);
        }
    }
    identities.retain(|key, _| !ambiguous.contains(key));
    let known = identities.keys().cloned().collect();
    let names = match_known_credit(value.trim(), &known)?;
    let mut seen = HashSet::new();
    let resolved: Vec<_> = names
        .iter()
        .filter_map(|name| {
            let credit = identities.get(&artist_identity_key(name))?;
            seen.insert(credit.artist_id.clone())
                .then(|| (*credit).clone())
        })
        .collect();
    // A solo artist's name must not authenticate unmentioned collaborators.
    credits
        .iter()
        .all(|credit| seen.contains(&credit.artist_id))
        .then_some(resolved)
}

pub(super) fn source_credits(
    display: &str,
    names: &[String],
    role: &str,
    source: &str,
) -> ArtistCredits {
    // Derive join wording only after structured identities independently tell us
    // where each artist ends. A free-form band name is never split here.
    let mut boundaries = Vec::new();
    let mut cursor = 0usize;
    for name in names {
        if let Some(relative) = display[cursor..].find(name) {
            let start = cursor + relative;
            let end = start + name.len();
            boundaries.push((start, end));
            cursor = end;
        } else {
            boundaries.clear();
            break;
        }
    }
    let joins: Vec<String> = (0..names.len())
        .map(|index| {
            if index + 1 == names.len() {
                return String::new();
            }
            if boundaries.len() == names.len() {
                let value = &display[boundaries[index].1..boundaries[index + 1].0];
                if !value.contains('\0') && !value.is_empty() {
                    return value.to_owned();
                }
            }
            "; ".into()
        })
        .collect();
    let credits = names
        .iter()
        .enumerate()
        .map(|(index, name)| ArtistCredit {
            artist_id: artist_identity_id(name),
            name: name.clone(),
            credited_name: name.clone(),
            role: if role == "track_artist"
                && index > 0
                && matches!(
                    joins[index - 1]
                        .trim()
                        .trim_matches(['(', ')'])
                        .trim()
                        .to_lowercase()
                        .as_str(),
                    "feat." | "feat" | "ft." | "ft" | "featuring"
                ) {
                "featured_artist".into()
            } else {
                role.into()
            },
            join_phrase: joins[index].clone(),
            source: source.into(),
        })
        .collect();
    ArtistCredits {
        display_name: display.to_owned(),
        credits,
    }
}

pub(super) fn track_credit_search_text(credits: &ArtistCredits) -> String {
    let displayed = credits.display_name.to_lowercase();
    let names = credits
        .credits
        .iter()
        .filter(|credit| matches!(credit.role.as_str(), "track_artist" | "featured_artist"))
        .filter(|credit| !displayed.contains(&credit.name.to_lowercase()))
        .map(|credit| credit.name.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {}", credits.display_name, names)
        .trim()
        .to_owned()
}

// Keep the persisted source wording for identity checks and provenance. Views
// use verified joining words, or remove redundant names only when every part of
// the local display is accounted for by independently indexed identities.
fn presented_credits(
    read: &redb::ReadTransaction,
    mut credits: ArtistCredits,
) -> Result<ArtistCredits, LibraryError> {
    let billed: Vec<_> = credits
        .credits
        .iter()
        .enumerate()
        .filter(|(_, credit)| credit.is_billed_artist())
        .map(|(index, _)| index)
        .collect();
    if billed.len() < 2 {
        return Ok(credits);
    }
    if billed
        .iter()
        .all(|&index| credits.credits[index].source != "local_tags_structured")
    {
        match read.open_table(DISPLAY_CREDIT_RESOLUTIONS) {
            Ok(table) => {
                let key = display_credit_key(&credits.display_name);
                if let Some(value) = table.get(key.as_str())? {
                    let resolved: ArtistCredits = decode_value(value.value())?;
                    if resolved.credits.len() == billed.len()
                        && billed
                            .iter()
                            .zip(&resolved.credits)
                            .all(|(&index, provider)| {
                                artist_identity_key(&credits.credits[index].name)
                                    == artist_identity_key(&provider.name)
                            })
                    {
                        for (position, (&index, provider)) in
                            billed.iter().zip(&resolved.credits).enumerate()
                        {
                            let credit = &mut credits.credits[index];
                            credit.credited_name = if provider.credited_name.trim().is_empty() {
                                provider.name.clone()
                            } else {
                                provider.credited_name.clone()
                            };
                            credit.join_phrase = if position + 1 == billed.len() {
                                String::new()
                            } else if provider.join_phrase.is_empty() {
                                ", ".into()
                            } else {
                                provider.join_phrase.clone()
                            };
                        }
                        credits.display_name = render_billed_credit(&credits, &billed);
                        return Ok(credits);
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
    }

    let known: HashSet<_> = billed
        .iter()
        .map(|&index| artist_identity_key(&credits.credits[index].name))
        .collect();
    if known.len() != billed.len() {
        return Ok(credits);
    }
    let display = credits.display_name.trim();
    let Some(names) = match_known_credit(display, &known) else {
        return Ok(credits);
    };
    let ordered: Vec<_> = billed
        .iter()
        .map(|&index| artist_identity_key(&credits.credits[index].name))
        .collect();
    let parsed: Vec<_> = names.iter().map(|name| artist_identity_key(name)).collect();
    if parsed.iter().collect::<HashSet<_>>() != ordered.iter().collect::<HashSet<_>>() {
        return Ok(credits);
    }
    // Distinct source wording can intentionally order the display differently
    // from structured billing. Only repetitions justify removing such text.
    if parsed.len() == ordered.len() && parsed != ordered {
        return Ok(credits);
    }
    // Pick a complete contiguous credit in the original billing order. This
    // retains "and" in "Prince, Prince and the Revolution" and cannot change
    // the primary artist by taking a differently ordered trailing credit.
    let mut cursor = 0;
    let mut spans = Vec::new();
    for name in &names {
        let Some(relative) = display[cursor..].find(name) else {
            return Ok(credits);
        };
        let start = cursor + relative;
        cursor = start + name.len();
        spans.push((start, cursor));
    }
    let window = parsed
        .windows(ordered.len())
        .enumerate()
        .filter(|(_, window)| *window == ordered.as_slice())
        .min_by_key(|(start, _)| spans[start + ordered.len() - 1].1 - spans[*start].0)
        .map(|(start, _)| start);
    let (display, names) = if let Some(start) = window {
        (
            display[spans[start].0..spans[start + ordered.len() - 1].1].to_owned(),
            names[start..start + ordered.len()].to_vec(),
        )
    } else {
        // All identities are supported, but repetitions have interleaved them.
        // Use their established order without guessing new joining words.
        let names: Vec<_> = billed
            .iter()
            .map(|&index| credits.credits[index].name.clone())
            .collect();
        (names.join("; "), names)
    };
    let visible = source_credits(&display, &names, "album_artist", "local_tags");
    for (&index, visible) in billed.iter().zip(visible.credits) {
        credits.credits[index].credited_name = visible.credited_name;
        credits.credits[index].join_phrase = visible.join_phrase;
    }
    credits.display_name = display;
    Ok(credits)
}

fn render_billed_credit(credits: &ArtistCredits, billed: &[usize]) -> String {
    billed
        .iter()
        .map(|&index| {
            let credit = &credits.credits[index];
            format!("{}{}", credit.credited_name, credit.join_phrase)
        })
        .collect::<String>()
}

impl Library {
    /// Remember a verified interpretation of an unstructured album display.
    /// Rebuilding through the regular scan updates all credits and indices and
    /// removes obsolete inferred artists without moving any media IDs.
    pub fn resolve_album_display_credit(
        &self,
        album_id: &str,
        provider_credits: &[ArtistCredit],
    ) -> Result<bool, LibraryError> {
        let started = std::time::Instant::now();
        let _scan = crate::indexing::SCAN_LOCK.lock();
        let write = self.db.begin_write()?;
        {
            let sources = write.open_table(ALBUM_CREDITS_TABLE)?;
            let Some(value) = sources.get(album_id)? else {
                return Ok(false);
            };
            let source: ArtistCredits = decode_value(value.value())?;
            let billed: Vec<_> = source
                .credits
                .iter()
                .filter(|credit| credit.is_album_artist())
                .collect();
            if billed.is_empty()
                || billed
                    .iter()
                    .any(|credit| credit.source == "local_tags_structured")
            {
                return Ok(false);
            }
            let Some(resolved) = resolve_display_credit(&source.display_name, provider_credits)
            else {
                return Ok(false);
            };
            // A single identity's alias is not a broken collaboration credit.
            // Preserve that local primary instead of renaming it remotely.
            if billed.len() == 1 && resolved.len() == 1 {
                return Ok(false);
            }
            if billed
                .iter()
                .map(|credit| artist_identity_key(&credit.name))
                .eq(resolved
                    .iter()
                    .map(|credit| artist_identity_key(&credit.name)))
            {
                return Ok(false);
            }
            let key = display_credit_key(&source.display_name);
            let mut resolutions = write.open_table(DISPLAY_CREDIT_RESOLUTIONS)?;
            if let Some(value) = resolutions.get(key.as_str())? {
                let previous: ArtistCredits = decode_value(value.value())?;
                let previous_ids: HashSet<_> = previous
                    .credits
                    .iter()
                    .map(|credit| &credit.artist_id)
                    .collect();
                let incoming_ids: HashSet<_> =
                    resolved.iter().map(|credit| &credit.artist_id).collect();
                if previous_ids != incoming_ids {
                    return Ok(false);
                }
            }
            let bytes = crate::storage::encode_value(&ArtistCredits {
                display_name: source.display_name,
                credits: resolved,
            })?;
            resolutions.insert(key.as_str(), bytes.as_slice())?;
            write.open_table(crate::storage::META_TABLE)?.insert(
                crate::indexing::DISPLAY_CREDIT_RESOLUTIONS_DIRTY,
                &[1u8][..],
            )?;
        }
        write.commit()?;
        crate::indexing::scan_locked(&self.roots, &self.db, false, None, started, None)?;
        Ok(true)
    }

    /// Stored source credits before display normalization. The original
    /// display wording is suitable for source-context fingerprints; names and
    /// app IDs may be reorganized by a verified identity repair during a scan.
    /// Missing source rows return empty credits instead of synthesizing billing
    /// from an enriched Album record.
    pub fn raw_album_credits(&self, album_id: &str) -> Result<ArtistCredits, LibraryError> {
        raw_credits(&self.db, ALBUM_CREDITS_TABLE, album_id)
    }

    /// Stored song credits before display normalization, with the same source
    /// provenance and missing-row behavior as `raw_album_credits`.
    pub fn raw_track_credits(&self, track_id: &str) -> Result<ArtistCredits, LibraryError> {
        raw_credits(&self.db, TRACK_CREDITS_TABLE, track_id)
    }

    pub fn album_credits(&self, album_id: &str) -> Result<ArtistCredits, LibraryError> {
        let read = self.db.begin_read()?;
        match read.open_table(ALBUM_CREDITS_TABLE) {
            Ok(table) => {
                if let Some(value) = table.get(album_id)? {
                    return presented_credits(&read, decode_value(value.value())?);
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        let Some(album) = self.get_album(album_id)? else {
            return Ok(ArtistCredits::default());
        };
        let mut names = album.artist_names.clone();
        if names.is_empty() {
            if let Some(artist) = self.get_artist(&album.artist_id)? {
                names.push(artist.name);
            }
        }
        let mut credits = source_credits(&names.join(", "), &names, "album_artist", "legacy");
        for (credit, id) in credits.credits.iter_mut().zip(album.all_artist_ids()) {
            credit.artist_id = id.clone();
        }
        Ok(credits)
    }

    pub fn track_credits(&self, track_id: &str) -> Result<ArtistCredits, LibraryError> {
        let read = self.db.begin_read()?;
        match read.open_table(TRACK_CREDITS_TABLE) {
            Ok(table) => {
                if let Some(value) = table.get(track_id)? {
                    return presented_credits(&read, decode_value(value.value())?);
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        let Some(track) = self.get_track(track_id)? else {
            return Ok(ArtistCredits::default());
        };
        let name = self
            .get_artist(&track.artist_id)?
            .map(|artist| artist.name)
            .unwrap_or_default();
        let mut credits = source_credits(&name, &[name.clone()], "track_artist", "legacy");
        if let Some(credit) = credits.credits.first_mut() {
            credit.artist_id = track.artist_id;
        }
        Ok(credits)
    }

    pub fn track_artist_ids(&self, track_id: &str) -> Result<Vec<String>, LibraryError> {
        Ok(self
            .track_credits(track_id)?
            .credits
            .into_iter()
            .filter(|credit| matches!(credit.role.as_str(), "track_artist" | "featured_artist"))
            .map(|credit| credit.artist_id)
            .collect())
    }

    /// Identities billed on at least one album or song. Supporting credits are
    /// retained in the catalog, but do not by themselves qualify an artist.
    pub fn billed_artist_ids(&self) -> Result<HashSet<String>, LibraryError> {
        let read = self.db.begin_read()?;
        let mut ids = HashSet::new();
        for entry in read.open_table(ALBUMS_TABLE)?.iter()? {
            let (_, value) = entry?;
            let album: Album = decode_value(value.value())?;
            ids.extend(album.all_artist_ids().iter().cloned());
        }
        match read.open_table(TRACK_CREDITS_TABLE) {
            Ok(table) => {
                for entry in table.iter()? {
                    let (_, value) = entry?;
                    let credits: ArtistCredits = decode_value(value.value())?;
                    ids.extend(
                        credits
                            .credits
                            .into_iter()
                            .filter(ArtistCredit::is_billed_artist)
                            .map(|credit| credit.artist_id),
                    );
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {
                for entry in read.open_table(TRACKS_TABLE)?.iter()? {
                    let (_, value) = entry?;
                    let track: common::Track = decode_value(value.value())?;
                    ids.insert(track.artist_id);
                }
            }
            Err(err) => return Err(err.into()),
        }
        Ok(ids)
    }

    pub fn list_artists_for_browse(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
        include_contributors: bool,
    ) -> Result<(Vec<Artist>, usize), LibraryError> {
        if include_contributors {
            return self.list_artists(search, limit, offset);
        }
        let billed = self.billed_artist_ids()?;
        // Filter before pagination so supporting credits cannot shorten pages
        // or inflate the total and make clients stop loading early.
        self.list_artists_matching(search, limit, offset, Some(&billed))
    }

    pub fn headline_album_counts(
        &self,
        artist_ids: &[String],
    ) -> Result<HashMap<String, usize>, LibraryError> {
        let wanted: HashSet<_> = artist_ids.iter().cloned().collect();
        let mut counts: HashMap<_, _> = artist_ids.iter().cloned().map(|id| (id, 0)).collect();
        let read = self.db.begin_read()?;
        let table = match read.open_table(ALBUM_CREDITS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => {
                for album in self.list_albums(None, usize::MAX, 0)?.0 {
                    for id in album.all_artist_ids() {
                        if wanted.contains(id) {
                            *counts.entry(id.clone()).or_default() += 1;
                        }
                    }
                }
                return Ok(counts);
            }
            Err(err) => return Err(err.into()),
        };
        for entry in table.iter()? {
            let (_, value) = entry?;
            let credits: ArtistCredits = decode_value(value.value())?;
            let mut counted = HashSet::new();
            for credit in credits.credits {
                if credit.role == "album_artist"
                    && wanted.contains(&credit.artist_id)
                    && counted.insert(credit.artist_id.clone())
                {
                    *counts.entry(credit.artist_id).or_default() += 1;
                }
            }
        }
        Ok(counts)
    }

    pub fn artist_release_relationships(
        &self,
        artist_id: &str,
    ) -> Result<ArtistReleaseRelationships, LibraryError> {
        let mut result = ArtistReleaseRelationships::default();
        for album in self.list_artist_albums(artist_id)? {
            let album_credits = self.album_credits(&album.id)?.credits;
            let is_headline = album_credits
                .iter()
                .any(|credit| credit.artist_id == artist_id && credit.role == "album_artist")
                || album.all_artist_ids().iter().any(|id| id == artist_id);
            let shared_headline = album_credits
                .iter()
                .filter(|credit| credit.role == "album_artist")
                .map(|credit| &credit.artist_id)
                .chain(album.all_artist_ids().iter())
                .collect::<HashSet<_>>()
                .len()
                > 1;
            let mut tracks = self.get_album_tracks(&album.id)?;
            tracks.sort_by_key(|track| {
                (
                    track.disc_no.unwrap_or(u16::MAX),
                    track.track_no.unwrap_or(u16::MAX),
                )
            });
            let mut appearance = contribution(&album);
            let mut other = contribution(&album);
            let mut collab = contribution(&album);
            for credit in &album_credits {
                if credit.artist_id == artist_id {
                    if credit.is_album_artist() {
                        push_unique(&mut collab.roles, credit.role.clone());
                    }
                    if credit.role != "album_artist" {
                        push_unique(&mut other.roles, credit.role.clone());
                    }
                }
            }
            // Verified provider album credits can add a release-level identity
            // while the local display wording remains in the source table.
            if is_headline {
                push_unique(&mut collab.roles, "album_artist".into());
            }
            for track in tracks {
                let mut appearance_roles = Vec::new();
                let mut other_roles = Vec::new();
                for credit in self.track_credits(&track.id)?.credits {
                    if credit.artist_id != artist_id {
                        continue;
                    }
                    if matches!(credit.role.as_str(), "track_artist" | "featured_artist") {
                        push_unique(&mut appearance_roles, credit.role);
                    } else {
                        push_unique(&mut other_roles, credit.role);
                    }
                }
                if !appearance_roles.is_empty() {
                    appearance.track_ids.push(track.id.clone());
                    appearance.track_titles.push(track.title.clone());
                    for role in appearance_roles {
                        push_unique(&mut appearance.roles, role);
                    }
                }
                if !other_roles.is_empty() {
                    other.track_ids.push(track.id.clone());
                    other.track_titles.push(track.title.clone());
                    for role in &other_roles {
                        push_unique(&mut other.roles, role.clone());
                    }
                }
                // Joint album billing applies to the entire release. Song
                // guests and supporting roles retain their separate scope.
                if is_headline && shared_headline {
                    collab.track_ids.push(track.id);
                    collab.track_titles.push(track.title);
                }
            }
            if is_headline {
                if !shared_headline {
                    result.albums.push(album);
                }
            } else if !appearance.track_ids.is_empty() {
                result.appears_on.push(appearance);
            }
            // Headline releases already have a section; retain their supporting
            // roles in detailed album/song credits without another album card.
            if !is_headline && !other.roles.is_empty() {
                result.other_credits.push(other);
            }
            if is_headline && shared_headline {
                result.collab_albums.push(collab);
            }
        }
        Ok(result)
    }
}

fn contribution(album: &Album) -> ArtistReleaseContribution {
    ArtistReleaseContribution {
        album: album.clone(),
        track_ids: Vec::new(),
        track_titles: Vec::new(),
        roles: Vec::new(),
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::{credit_names, resolve_display_credit};
    use common::{artist_identity_key, ArtistCredit};
    use std::collections::HashSet;

    #[test]
    fn parses_display_lists_and_features_without_splitting_authoritative_band_names() {
        let known = HashSet::new();
        for (display, expected) in [
            ("Artist One,Artist Two", vec!["Artist One", "Artist Two"]),
            (
                "Artist One; Artist Two; artist one",
                vec!["Artist One", "Artist Two"],
            ),
            ("Artist One FEAT. Guest", vec!["Artist One", "Guest"]),
            ("Earth, Wind & Fire", vec!["Earth, Wind & Fire"]),
            ("Tyler, The Creator", vec!["Tyler, The Creator"]),
            ("AC/DC", vec!["AC/DC"]),
            ("Simon & Garfunkel", vec!["Simon & Garfunkel"]),
        ] {
            assert_eq!(credit_names(Some(display), &[], false, &known), expected);
        }
        assert_eq!(
            credit_names(
                Some("Artist One, Artist Two"),
                &["Artist One, Artist Two".into()],
                true,
                &known
            ),
            ["Artist One, Artist Two"]
        );
    }

    #[test]
    fn resolves_ambiguous_joins_only_against_complete_independent_artist_evidence() {
        let known: HashSet<_> = ["Artist One", "Artist Two", "Artist Three"]
            .into_iter()
            .map(artist_identity_key)
            .collect();
        for display in [
            "Artist One & Artist Two",
            "Artist One X Artist Two",
            "Artist One, Artist Two & Artist Three",
        ] {
            assert!(credit_names(Some(display), &[], false, &known).len() > 1);
        }
        assert_eq!(
            credit_names(Some("Artist One & Unknown Band"), &[], false, &known),
            ["Artist One & Unknown Band"]
        );
        let known = [artist_identity_key("Name, With Comma")]
            .into_iter()
            .collect();
        assert_eq!(
            credit_names(Some("Name, With Comma"), &[], false, &known),
            ["Name, With Comma"]
        );
    }

    fn provider_credit(id: &str, name: &str, credited_name: &str) -> ArtistCredit {
        ArtistCredit {
            artist_id: id.into(),
            name: name.into(),
            credited_name: credited_name.into(),
            role: "album_artist".into(),
            join_phrase: String::new(),
            source: "musicbrainz".into(),
        }
    }

    #[test]
    fn matches_complete_names_inside_cross_genre_collaboration_displays() {
        for (display, names) in [
            (
                "Prince, Prince and the Revolution",
                vec!["Prince", "Prince and the Revolution"],
            ),
            (
                "Guest, Earth, Wind & Fire",
                vec!["Guest", "Earth, Wind & Fire"],
            ),
            (
                "Scott Walker + Sunn O)))",
                vec!["Scott Walker", "Sunn O)))"],
            ),
            ("Lou Reed / John Cale", vec!["Lou Reed", "John Cale"]),
            (
                "Metallica with Michael Kamen conducting the San Francisco Symphony",
                vec!["Metallica", "Michael Kamen", "San Francisco Symphony"],
            ),
            (
                "Maverick City Music & Kirk Franklin",
                vec!["Maverick City Music", "Kirk Franklin"],
            ),
            (
                "Анна Нетребко & Rolando Villazón",
                vec!["Анна Нетребко", "Rolando Villazón"],
            ),
            (
                "Nusrat Fateh Ali Khan & Michael Brook",
                vec!["Nusrat Fateh Ali Khan", "Michael Brook"],
            ),
        ] {
            let known = names.iter().map(|name| artist_identity_key(name)).collect();
            assert_eq!(
                credit_names(Some(display), &[], false, &known),
                names,
                "{display}"
            );
        }
        let known = ["Prince", "The Revolution"]
            .into_iter()
            .map(artist_identity_key)
            .collect();
        assert_eq!(
            credit_names(
                Some("Prince, Prince and the Revolution"),
                &[],
                false,
                &known
            ),
            ["Prince", "the Revolution"]
        );
    }

    #[test]
    fn verified_credits_deduplicate_ids_and_require_every_remote_identity_to_be_named() {
        let credits = [
            provider_credit("prince", "Prince", "Prince"),
            provider_credit("revolution", "The Revolution", "the Revolution"),
        ];
        let resolved =
            resolve_display_credit("Prince, Prince and the Revolution", &credits).unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(|credit| credit.artist_id.as_str())
                .collect::<Vec<_>>(),
            ["prince", "revolution"]
        );
        assert!(resolve_display_credit("Prince", &credits).is_none());
        assert!(resolve_display_credit("Prince & Unknown Band", &credits).is_none());

        let mut repeated = credits.to_vec();
        repeated.push(provider_credit("prince", "Prince", "Prince"));
        assert_eq!(
            resolve_display_credit("Prince and the Revolution", &repeated)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn provider_group_id_preserves_duos_and_conflicting_aliases_are_rejected() {
        for (name, credited) in [
            ("BeBe & CeCe Winans", "BeBe & CeCe Winans"),
            (
                "Robert Plant & Alison Krauss",
                "Robert Plant | Alison Krauss",
            ),
            ("Jimmy Page & Robert Plant", "Jimmy Page & Robert Plant"),
            ("Earth, Wind & Fire", "Earth, Wind & Fire"),
        ] {
            let credits = [provider_credit("group", name, credited)];
            assert_eq!(resolve_display_credit(credited, &credits).unwrap(), credits);
        }
        let credits = [
            provider_credit("one", "First Artist", "Shared Name"),
            provider_credit("two", "Second Artist", "Shared Name"),
        ];
        assert!(resolve_display_credit("Shared Name & Second Artist", &credits).is_none());
        assert_eq!(
            resolve_display_credit("First Artist & Second Artist", &credits)
                .unwrap()
                .len(),
            2
        );
        assert!(
            resolve_display_credit("First Artist", &[provider_credit("", "First Artist", "")])
                .is_none()
        );
    }
}
