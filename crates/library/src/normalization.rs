//! Pure metadata cleanup shared by local catalog assembly and external enrichment.
use std::collections::HashSet;

pub(super) fn merge_genres(target: &mut Vec<String>, incoming: &[String]) {
    if target.is_empty() && incoming.is_empty() {
        return;
    }
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for genre in target.iter().chain(incoming.iter()) {
        let genre = normalize_genre_separators(genre);
        // Provider and sidecar vectors already contain individual labels;
        // a slash can be part of a recognized label such as "funk / soul".
        for part in genre.split(&[';', ',', '|', '\0'][..]) {
            let trimmed = normalize_genre_label(part.trim());
            if trimmed.is_empty() {
                continue;
            }
            let key = normalize_genre_key(&trimmed);
            if seen.insert(key) {
                merged.push(trimmed);
            }
        }
    }
    *target = merged;
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

fn normalize_genre_label(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    let normalized = normalize_genre_probe(&lower);
    let exact = match lower.as_str() {
        "musique classique" => "Classical".to_string(),
        "musique de chambre" => "Chamber music".to_string(),
        "musique de film" => "Soundtrack".to_string(),
        "musiques de film" => "Soundtrack".to_string(),
        "musique électronique" => "Electronic".to_string(),
        "musique electronique" => "Electronic".to_string(),
        "musique instrumentale" => "Instrumental".to_string(),
        "musique ambient" => "Ambient".to_string(),
        "musique ambiante" => "Ambient".to_string(),
        "piano solo" => "Solo piano".to_string(),
        "classique" => "Classical".to_string(),
        "musica clasica" => "Classical".to_string(),
        "música clásica" => "Classical".to_string(),
        "musica classica" => "Classical".to_string(),
        "música clásica contemporánea" => "Contemporary classical".to_string(),
        "musica contemporanea" => "Contemporary classical".to_string(),
        "musica de camara" => "Chamber music".to_string(),
        "música de cámara" => "Chamber music".to_string(),
        "musica de pelicula" => "Soundtrack".to_string(),
        "música de película" => "Soundtrack".to_string(),
        "banda sonora" => "Soundtrack".to_string(),
        "colonna sonora" => "Soundtrack".to_string(),
        "musica elettronica" => "Electronic".to_string(),
        "musica elettronica sperimentale" => "Electronic".to_string(),
        "musica strumentale" => "Instrumental".to_string(),
        "musica ambient" => "Ambient".to_string(),
        "música instrumental" => "Instrumental".to_string(),
        "música electrónica" => "Electronic".to_string(),
        "música electronica" => "Electronic".to_string(),
        "klassik" => "Classical".to_string(),
        "klassische musik" => "Classical".to_string(),
        "filmmusik" => "Soundtrack".to_string(),
        "elektronische musik" => "Electronic".to_string(),
        "instrumentalmusik" => "Instrumental".to_string(),
        "kammermusik" => "Chamber music".to_string(),
        "zeitgenössische klassische musik" => "Contemporary classical".to_string(),
        "zeitgenossische klassische musik" => "Contemporary classical".to_string(),
        _ => String::new(),
    };
    if !exact.is_empty() {
        return exact;
    }
    match match_genre_probe(&normalized) {
        Some(mapped) => mapped,
        None => value.trim().to_string(),
    }
}

fn match_genre_probe(value: &str) -> Option<String> {
    let rules: [(&str, &[&str]); 6] = [
        (
            "Classical",
            &[
                "classical",
                "klassik",
                "musica clasica",
                "musique classique",
            ],
        ),
        (
            "Contemporary classical",
            &["contemporary classical", "zeitgenossisch", "contemporanea"],
        ),
        (
            "Chamber music",
            &[
                "chamber music",
                "musique de chambre",
                "musica de camara",
                "kammermusik",
            ],
        ),
        (
            "Soundtrack",
            &[
                "soundtrack",
                "musique de film",
                "banda sonora",
                "colonna sonora",
                "filmmusik",
            ],
        ),
        (
            "Electronic",
            &["electronic", "electronique", "electronica", "elektronisch"],
        ),
        (
            "Instrumental",
            &[
                "instrumental",
                "instrumentale",
                "strumentale",
                "instrumentalmusik",
            ],
        ),
    ];

    for (label, terms) in rules {
        for term in terms {
            if value == *term {
                return Some(label.to_string());
            }
        }
    }
    if value == "ambient" {
        return Some("Ambient".to_string());
    }
    if value == "solo piano" || value == "piano solo" {
        return Some("Solo piano".to_string());
    }
    None
}

fn normalize_genre_probe(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = false;
    for ch in value.chars() {
        let mapped = match ch {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'œ' => {
                out.push('o');
                'e'
            }
            'æ' => {
                out.push('a');
                'e'
            }
            _ => ch,
        };
        if mapped.is_ascii_alphanumeric() {
            out.push(mapped);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn normalize_genre_key(value: &str) -> String {
    value.trim().to_lowercase()
}

pub(super) fn clean_summary(summary: String) -> Option<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
