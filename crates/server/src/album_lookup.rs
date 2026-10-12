//! Shared, bounded discovery queries for album metadata providers.
//!
//! These helpers normalize release labels and typography, not recording identity.
//! A matching title still requires independent artist and complete-program evidence.

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

const MAX_CANDIDATES: usize = 8;

/// Keep the supplied title first, then try only general release/typography variants.
/// Candidate deduplication preserves punctuation and internal spacing differences.
pub(crate) fn title_candidates(title: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut release_titles = Vec::new();
    let mut current = title.trim().to_string();
    add_candidate(&mut candidates, &current);
    if current.is_empty() {
        return candidates;
    }
    release_titles.push(current.clone());

    while let Some(stripped) = strip_trailing_release_qualifier(&current) {
        current = stripped;
        // Reserve a slot for the final base even when unusually many labels exist.
        if candidates.len() < MAX_CANDIDATES - 1 {
            add_candidate(&mut candidates, &current);
            release_titles.push(current.clone());
        }
    }
    add_candidate(&mut candidates, &current);
    if release_titles.last() != Some(&current) {
        release_titles.push(current.clone());
    }

    // Apply spelling variants to both the complete title and its release-label base.
    for value in &release_titles {
        add_candidate(&mut candidates, &space_letter_digit_boundaries(value));
    }
    let base = strip_release_qualifiers(title);
    if let Some(unwrapped) = unwrap_whole_title(&base) {
        add_candidate(&mut candidates, unwrapped);
    } else if !base.is_empty() {
        // [untitled] is one example of a provider title convention; no name-specific alias.
        add_candidate(&mut candidates, &format!("[{base}]"));
    }
    add_candidate(&mut candidates, &compact_separator_spacing(&base));
    add_candidate(&mut candidates, &ampersands_to_and(&base));
    add_candidate(&mut candidates, &and_to_ampersands(&base));
    candidates
}

/// Remove known trailing release labels, retaining performance/version words.
pub(crate) fn strip_release_qualifiers(title: &str) -> String {
    let mut current = title.trim().to_string();
    while let Some(stripped) = strip_trailing_release_qualifier(&current) {
        current = stripped;
    }
    current
}

/// Remove one complete trailing qualifier; nested brackets are handled as a unit.
pub(crate) fn strip_trailing_release_qualifier(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if let Some(start) = trailing_bracket_start(trimmed) {
        let last_len = trimmed.chars().last()?.len_utf8();
        let first_len = trimmed[start..].chars().next()?.len_utf8();
        let qualifier = &trimmed[start + first_len..trimmed.len() - last_len];
        if is_release_qualifier(qualifier) {
            return nonempty_prefix(&trimmed[..start]);
        }
        return None;
    }

    // Unbracketed qualifiers need an explicit separator so a title ending in an
    // otherwise meaningful word such as "Explicit" is retained.
    let separator = [" - ", " – ", " — "]
        .into_iter()
        .filter_map(|value| trimmed.rfind(value).map(|start| (start, value.len())))
        .max_by_key(|(start, _)| *start)?;
    let qualifier = &trimmed[separator.0 + separator.1..];
    is_release_qualifier(qualifier)
        .then(|| nonempty_prefix(&trimmed[..separator.0]))
        .flatten()
}

/// A release-title comparison key, never an artist or recording identity key.
pub(crate) fn release_title_key(title: &str) -> String {
    let base = strip_release_qualifiers(title);
    let with_words = ampersands_to_and(&base);
    let normalized: String = with_words
        .nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .flat_map(char::to_lowercase)
        .collect();
    let spaced = space_letter_digit_boundaries(&normalized);
    words_only(&spaced)
}

pub(crate) fn titles_equivalent(left: &str, right: &str) -> bool {
    let left = release_title_key(left);
    !left.is_empty() && left == release_title_key(right)
}

/// Discovery-only variants of an entire artist name. Never split contributor lists
/// or substitute band identities based on punctuation or the word "and".
pub(crate) fn artist_name_variants(name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    add_candidate(&mut candidates, name);
    let ascii_hyphens = map_hyphens(name.trim(), '-');
    let unicode_hyphens = map_hyphens(name.trim(), '\u{2010}');
    for value in [name.trim(), &ascii_hyphens, &unicode_hyphens] {
        add_candidate(&mut candidates, value);
        add_candidate(&mut candidates, &ampersands_to_and(value));
        add_candidate(&mut candidates, &and_to_ampersands(value));
    }
    candidates
}

fn add_candidate(candidates: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() || candidates.len() == MAX_CANDIDATES {
        return;
    }
    let key = trimmed.to_lowercase();
    if !candidates
        .iter()
        .any(|existing| existing.to_lowercase() == key)
    {
        candidates.push(trimmed.to_string());
    }
}

fn nonempty_prefix(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn trailing_bracket_start(value: &str) -> Option<usize> {
    if !matches!(value.chars().last()?, ')' | ']') {
        return None;
    }
    let mut expected_openers = Vec::new();
    for (index, ch) in value.char_indices().rev() {
        match ch {
            ')' => expected_openers.push('('),
            ']' => expected_openers.push('['),
            '(' | '[' => {
                if expected_openers.pop()? != ch {
                    return None;
                }
                if expected_openers.is_empty() {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn unwrap_whole_title(value: &str) -> Option<&str> {
    if trailing_bracket_start(value)? != 0 {
        return None;
    }
    let start = value.chars().next()?.len_utf8();
    let end = value.len() - value.chars().last()?.len_utf8();
    let inner = value[start..end].trim();
    (!inner.is_empty()).then_some(inner)
}

fn is_release_qualifier(value: &str) -> bool {
    let normalized: String = value
        .nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .flat_map(char::to_lowercase)
        .collect();
    let words = words_only(&normalized);
    let words: Vec<_> = words.split_whitespace().collect();
    let has_release_marker = words.iter().any(|word| {
        matches!(
            *word,
            "explicit" | "remaster" | "remastered" | "deluxe" | "edition" | "anniversary" | "hires"
        )
    }) || words.windows(2).any(|pair| {
        matches!(
            pair,
            ["hi", "res"] | ["high", "res"] | ["high", "resolution"]
        )
    });
    has_release_marker
        && words.iter().all(|word| {
            matches!(
                *word,
                "explicit"
                    | "remaster"
                    | "remastered"
                    | "deluxe"
                    | "edition"
                    | "anniversary"
                    | "hires"
                    | "hi"
                    | "res"
                    | "high"
                    | "resolution"
                    | "version"
                    | "digital"
                    | "expanded"
                    | "special"
                    | "limited"
                    | "super"
                    | "bonus"
                    | "tracks"
            ) || numeric_label(word)
        })
}

fn numeric_label(value: &str) -> bool {
    !value.is_empty()
        && (value.chars().all(|ch| ch.is_ascii_digit())
            || ["st", "nd", "rd", "th"].into_iter().any(|suffix| {
                value.strip_suffix(suffix).is_some_and(|number| {
                    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
                })
            }))
}

fn space_letter_digit_boundaries(value: &str) -> String {
    let mut out = String::new();
    let mut previous: Option<char> = None;
    for ch in value.chars() {
        if previous.is_some_and(|before| {
            (before.is_alphabetic() && ch.is_ascii_digit())
                || (before.is_ascii_digit() && ch.is_alphabetic())
        }) {
            out.push(' ');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

fn words_only(value: &str) -> String {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_separator_spacing(value: &str) -> String {
    let mut out = String::new();
    let mut add_space = false;
    for ch in value.chars() {
        if matches!(ch, ':' | ';') {
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
            out.push(ch);
            add_space = true;
        } else if add_space && ch.is_whitespace() {
            continue;
        } else {
            if add_space {
                out.push(' ');
                add_space = false;
            }
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn ampersands_to_and(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace('&', " and ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn and_to_ampersands(value: &str) -> String {
    let words: Vec<_> = value.split_whitespace().collect();
    if !words.iter().any(|word| word.eq_ignore_ascii_case("and")) {
        return value.to_string();
    }
    words
        .into_iter()
        .map(|word| {
            if word.eq_ignore_ascii_case("and") {
                "&"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn map_hyphens(value: &str, hyphen: char) -> String {
    value
        .chars()
        .map(|ch| {
            if matches!(ch, '-' | '\u{2010}' | '\u{2011}') {
                hyphen
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_sequential_release_labels_are_removed_without_losing_original_query() {
        let title = "Unseen LP7 (Super Deluxe Edition [Explicit])";
        let candidates = title_candidates(title);
        assert_eq!(candidates[0], title);
        assert!(candidates.contains(&"Unseen LP7".to_string()));
        assert!(candidates.contains(&"Unseen LP 7".to_string()));
        assert_eq!(strip_release_qualifiers(title), "Unseen LP7");
        assert_eq!(
            strip_release_qualifiers("New Record (Deluxe Edition) (Explicit)"),
            "New Record"
        );
        assert_eq!(
            strip_release_qualifiers("New Record — 2024 Digital Remastered"),
            "New Record"
        );
        assert_eq!(
            strip_release_qualifiers("New Record [High Resolution Version]"),
            "New Record"
        );
    }

    #[test]
    fn performance_and_unrecognized_parenthetical_words_remain_meaningful() {
        for suffix in [
            "Live",
            "Remix",
            "Acoustic",
            "Re-recorded",
            "Alternate Take",
            "Digital Version",
            "Album",
            "2020",
            "Side B",
            "Special Sauce Edition",
            "Live (Deluxe Edition)",
        ] {
            let title = format!("Synthetic Record ({suffix})");
            assert_eq!(strip_release_qualifiers(&title), title);
            assert!(!titles_equivalent(&title, "Synthetic Record"), "{suffix}");
        }
        assert_eq!(
            strip_release_qualifiers("New Record (Live) (Deluxe Edition)"),
            "New Record (Live)"
        );
        assert!(!titles_equivalent(
            "New Record - Side B (Deluxe Edition) (Explicit)",
            "New Record"
        ));
        assert_eq!(strip_release_qualifiers("An Explicit"), "An Explicit");
        assert_eq!(
            strip_release_qualifiers("New Record (Deluxe Edition]"),
            "New Record (Deluxe Edition]"
        );
    }

    #[test]
    fn title_keys_fold_typography_without_collapsing_numeric_or_work_identity() {
        assert!(titles_equivalent("Échos LP2 (Explicit)", "ECHOS LP 2"));
        assert!(titles_equivalent("Wind & Rain", "Wind and Rain"));
        assert!(titles_equivalent("[Untitled]", "Untitled (Deluxe)"));
        assert!(titles_equivalent(
            "Chopin : Nocturnes (1999 remastered)",
            "Chopin: Nocturnes"
        ));
        assert!(!titles_equivalent("Prelude Op9", "Prelude Op90"));
        assert!(!titles_equivalent("Prelude No. 1", "Prelude No. 2"));
        assert!(!titles_equivalent("Flower Boy", "Scum Fuck Flower Boy"));
        assert!(!titles_equivalent("[Love Symbol]", "Symbol Album"));
        assert!(!titles_equivalent("", "[Explicit]"));
    }

    #[test]
    fn queries_keep_spacing_and_punctuation_variants_and_are_bounded() {
        let candidates = title_candidates("An LP2 : Test (Deluxe Edition) (Explicit)");
        assert_eq!(candidates[0], "An LP2 : Test (Deluxe Edition) (Explicit)");
        assert!(candidates.contains(&"An LP2 : Test".to_string()));
        assert!(candidates.contains(&"An LP 2 : Test".to_string()));
        assert!(candidates.contains(&"An LP2: Test".to_string()));
        assert!(candidates.len() <= MAX_CANDIDATES);
        assert!(title_candidates("Untitled (Deluxe)").contains(&"[Untitled]".to_string()));
        assert!(title_candidates("[A New Title]").contains(&"A New Title".to_string()));

        let mut deduped = Vec::new();
        for value in ["An LP2", "an lp2", "An LP 2", "[An LP2]", "An  LP2"] {
            add_candidate(&mut deduped, value);
        }
        assert_eq!(deduped, ["An LP2", "An LP 2", "[An LP2]", "An  LP2"]);
        let many = "Title (Explicit) (Deluxe) (Remastered) (Edition) (HiRes) (Anniversary) (Explicit) (Deluxe)";
        assert!(title_candidates(many).len() <= MAX_CANDIDATES);
        assert!(title_candidates(many).contains(&"Title".to_string()));
        assert!(title_candidates("  ").is_empty());
    }

    #[test]
    fn artist_queries_change_typography_only_and_never_split_names() {
        let candidates = artist_name_variants("Example-Name & The Others");
        assert_eq!(candidates[0], "Example-Name & The Others");
        assert!(candidates.contains(&"Example‐Name & The Others".to_string()));
        assert!(candidates.contains(&"Example-Name and The Others".to_string()));
        assert!(candidates.len() <= MAX_CANDIDATES);
        assert_eq!(
            artist_name_variants("Anderson, The Players"),
            ["Anderson, The Players"]
        );
        assert_eq!(
            artist_name_variants("Prince, Prince And The Revolution")[0],
            "Prince, Prince And The Revolution"
        );
        assert!(!artist_name_variants("Prince, Prince And The Revolution")
            .iter()
            .any(|value| value == "Prince" || value == "The Revolution"));
        assert!(artist_name_variants("Jay-Z").contains(&"Jay‐Z".to_string()));
    }

    #[test]
    fn sanitized_seventeen_album_source_fixture_replays_discovery_policy() {
        let rows: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/metadata_lookup/album_title_policy.json"
        ))
        .unwrap();
        assert_eq!(rows.len(), 17);
        for row in rows {
            let title = row["local_title"].as_str().unwrap();
            let base = row["expected_base"].as_str().unwrap();
            assert_eq!(strip_release_qualifiers(title), base, "{title}");
            let queries = title_candidates(title);
            assert_eq!(queries[0], title, "{title}");
            assert!(queries.len() <= MAX_CANDIDATES, "{title}");
            for expected in row["expected_queries"].as_array().unwrap() {
                assert!(
                    queries
                        .iter()
                        .any(|value| value == expected.as_str().unwrap()),
                    "missing {expected} for {title}"
                );
            }
            let source = row["provider_title"].as_str().unwrap();
            assert_eq!(
                titles_equivalent(title, source),
                row["provider_title_equivalent"].as_bool().unwrap(),
                "{title} / {source}"
            );
        }
    }
}
