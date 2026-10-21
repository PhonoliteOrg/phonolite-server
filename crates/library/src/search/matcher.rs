use std::collections::HashMap;

// Keep this algorithm and its conformance vectors aligned with the app's
// lib/entities/library_search_matcher.dart.
pub fn normalize_library_search(value: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_owned()
}

/// Prepare once per scan. Original metadata is only used for display.
pub struct LibrarySearchQuery {
    normalized: String,
    compact: String,
    term: SearchTerm,
    tokens: Vec<String>,
    token_terms: Vec<SearchTerm>,
}

impl LibrarySearchQuery {
    pub fn new(query: &str) -> Self {
        let normalized = normalize_library_search(query);
        let compact = normalized.replace(' ', "");
        let term = SearchTerm::new(&compact);
        let tokens: Vec<String> = normalized.split_whitespace().map(str::to_owned).collect();
        let token_terms = tokens.iter().map(|word| SearchTerm::new(word)).collect();
        Self {
            normalized,
            compact,
            term,
            tokens,
            token_terms,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    pub fn score(&self, candidate: &str) -> u32 {
        if self.is_empty() {
            return 0;
        }
        let target = normalize_library_search(candidate);
        if target.is_empty() {
            return 0;
        }
        let compact = target.replace(' ', "");
        if target == self.normalized {
            return 100;
        }
        if compact == self.compact {
            return 95;
        }
        if target.starts_with(&self.normalized) {
            return 90;
        }
        if compact.starts_with(&self.compact) {
            return 85;
        }
        if target.contains(&self.normalized) {
            return 80;
        }
        if compact.contains(&self.compact) {
            return 75;
        }
        if self.tokens.iter().all(|token| target.contains(token)) {
            return 70;
        }

        let words: Vec<Vec<char>> = target
            .split_whitespace()
            .map(|word| word.chars().collect())
            .collect();
        // Consecutive compact spans handle split/merged words. Token matching
        // also handles reordered words and searches across metadata fields.
        let mut best = span_distance(&self.term, &words);
        if self.token_terms.len() > 1 {
            let token_distance: Option<usize> = self
                .token_terms
                .iter()
                .map(|token| span_distance(token, &words))
                .sum();
            if let Some(distance) = token_distance {
                best = Some(best.map_or(distance, |current| current.min(distance)));
            }
        }
        best.map_or(0, |distance| 60 - distance.min(5) as u32)
    }
}

struct SearchTerm {
    letters: Vec<char>,
    counts: HashMap<char, usize>,
}

impl SearchTerm {
    fn new(value: &str) -> Self {
        let letters: Vec<_> = value.chars().collect();
        let mut counts = HashMap::new();
        for &letter in &letters {
            *counts.entry(letter).or_insert(0) += 1;
        }
        Self { letters, counts }
    }
}

fn span_distance(term: &SearchTerm, words: &[Vec<char>]) -> Option<usize> {
    let query = &term.letters;
    // Bound fuzzy allocations; literal matching still accepts long strings.
    // Short words must match exactly to avoid noisy one-letter substitutions.
    if query.is_empty() || query.len() > 64 {
        return None;
    }
    let budget = if query.len() < 4 {
        0
    } else if query.len() < 8 {
        1
    } else {
        2
    };
    let mut best = None;
    for start in 0..words.len() {
        let mut span = Vec::new();
        let mut counts = HashMap::new();
        let mut shared = 0;
        for word in &words[start..] {
            if span.len() + word.len() > query.len() + budget {
                break;
            }
            span.extend(word);
            for &letter in word {
                let count = counts.entry(letter).or_insert(0);
                *count += 1;
                if *count <= *term.counts.get(&letter).unwrap_or(&0) {
                    shared += 1;
                }
            }
            if query.len().abs_diff(span.len()) > budget {
                continue;
            }
            // Swaps preserve letter counts; other edits repair at most one
            // missing letter per side. This cannot reject an in-budget match.
            if query.len().max(span.len()) - shared > budget {
                continue;
            }
            let distance = damerau_levenshtein(query, &span);
            if distance <= budget && best.is_none_or(|current| distance < current) {
                best = Some(distance);
                if distance == 0 {
                    return best;
                }
            }
        }
    }
    best
}

// Unrestricted Damerau–Levenshtein on Unicode scalar values, including
// insertions/deletions around transposed characters.
fn damerau_levenshtein(left: &[char], right: &[char]) -> usize {
    let infinity = left.len() + right.len();
    let mut matrix = vec![vec![0; right.len() + 2]; left.len() + 2];
    matrix[0][0] = infinity;
    for i in 0..=left.len() {
        matrix[i + 1][0] = infinity;
        matrix[i + 1][1] = i;
    }
    for j in 0..=right.len() {
        matrix[0][j + 1] = infinity;
        matrix[1][j + 1] = j;
    }
    let mut last_seen = HashMap::new();
    for i in 1..=left.len() {
        let mut last_match = 0;
        for j in 1..=right.len() {
            let previous_row = *last_seen.get(&right[j - 1]).unwrap_or(&0);
            let previous_column = last_match;
            let cost = usize::from(left[i - 1] != right[j - 1]);
            if cost == 0 {
                last_match = j;
            }
            matrix[i + 1][j + 1] = (matrix[i][j] + cost)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j + 1] + 1)
                .min(
                    matrix[previous_row][previous_column]
                        + (i - previous_row - 1)
                        + 1
                        + (j - previous_column - 1),
                );
        }
        last_seen.insert(left[i - 1], i);
    }
    matrix[left.len() + 1][right.len() + 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformance_vectors_match_the_local_app() {
        let vectors: Vec<(String, String, u32)> =
            serde_json::from_str(include_str!("testdata/library_search_vectors.json")).unwrap();
        for (query, target, expected) in vectors {
            assert_eq!(
                LibrarySearchQuery::new(&query).score(&target),
                expected,
                "query={query:?}, target={target:?}"
            );
        }
    }

    #[test]
    fn every_single_letter_edit_is_supported() {
        let word = "radiohead";
        for i in 0..word.len() {
            let mut variants = vec![
                format!("{}{}", &word[..i], &word[i + 1..]),
                format!("{}x{}", &word[..i], &word[i + 1..]),
                format!("{}x{}", &word[..i], &word[i..]),
            ];
            if i + 1 < word.len() {
                variants.push(format!(
                    "{}{}{}{}",
                    &word[..i],
                    &word[i + 1..i + 2],
                    &word[i..i + 1],
                    &word[i + 2..]
                ));
            }
            for query in variants {
                assert!(LibrarySearchQuery::new(&query).score(word) > 0, "{query}");
            }
        }
    }

    #[test]
    fn distance_allows_edits_around_transpositions_and_unicode() {
        for (a, b, expected) in [
            ("ca", "abc", 2),
            ("raidohed", "radiohead", 2),
            ("東京の空", "東京空", 1),
        ] {
            assert_eq!(
                damerau_levenshtein(
                    &a.chars().collect::<Vec<_>>(),
                    &b.chars().collect::<Vec<_>>()
                ),
                expected
            );
        }
    }

    #[test]
    fn long_inputs_keep_literal_matches_without_unbounded_fuzzy_matrices() {
        let query = LibrarySearchQuery::new(&"a".repeat(5000));
        assert_eq!(query.score(&"a".repeat(5000)), 100);
        assert_eq!(query.score(&"b".repeat(5000)), 0);
    }
}
