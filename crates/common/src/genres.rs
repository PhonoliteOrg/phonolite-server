/// Presentation casing for genre labels. Stored tags, filter keys and listening
/// event identities keep their original values.
pub fn genre_display_label(value: &str) -> String {
    let mut label = String::with_capacity(value.len());
    let mut word = String::new();
    for ch in value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
    {
        if ch.is_whitespace() || "-‐‑–—/&,;|()[]{}:".contains(ch) {
            label.push_str(&capitalize_word(&word));
            word.clear();
            label.push(ch);
        } else {
            word.push(ch);
        }
    }
    label.push_str(&capitalize_word(&word));
    label
}

fn capitalize_word(value: &str) -> String {
    let lower = value.to_lowercase();
    match lower.as_str() {
        "edm" | "idm" | "ebm" | "aor" | "mpb" | "ost" | "ccm" | "nwobhm" | "uk" | "us" | "dj" => {
            return lower.to_uppercase()
        }
        "dnb" => return "DnB".to_string(),
        "rnb" => return "RnB".to_string(),
        _ => {}
    }
    let mut capitalized = false;
    let mut word = String::with_capacity(value.len());
    for ch in lower.chars() {
        if !capitalized && (ch.is_lowercase() || ch.is_uppercase()) {
            capitalized = true;
            word.extend(ch.to_uppercase());
        } else {
            word.push(ch);
        }
    }
    word
}

#[cfg(test)]
mod tests {
    use super::genre_display_label;

    #[test]
    fn genre_display_casing_handles_words_acronyms_and_punctuation() {
        for (raw, expected) in [
            ("  alternative   ROCK\t", "Alternative Rock"),
            ("acid jazz", "Acid Jazz"),
            ("r&b", "R&B"),
            ("edm / idm / ebm", "EDM / IDM / EBM"),
            ("dnb & rnb", "DnB & RnB"),
            ("uk garage", "UK Garage"),
            ("j-pop; k-pop", "J-Pop; K-Pop"),
            ("post‐rock / nu-metal", "Post‐Rock / Nu-Metal"),
            (
                "électronique (música clásica)",
                "Électronique (Música Clásica)",
            ),
            ("2-step", "2-Step"),
            ("東京", "東京"),
            (" ", ""),
        ] {
            let actual = genre_display_label(raw);
            assert_eq!(actual, expected, "{raw}");
            assert_eq!(genre_display_label(&actual), expected, "idempotent: {raw}");
        }
    }
}
