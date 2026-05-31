use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub logo_ref: Option<String>,
    #[serde(default)]
    pub banner_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub artist_id: String,
    #[serde(default)]
    pub artist_ids: Vec<String>,
    #[serde(default)]
    pub artist_names: Vec<String>,
    pub title: String,
    pub year: Option<i32>,
    pub folder_relpath: String,
    pub cover_ref: Option<CoverRef>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

impl Album {
    pub fn all_artist_ids(&self) -> &[String] {
        if self.artist_ids.is_empty() {
            std::slice::from_ref(&self.artist_id)
        } else {
            &self.artist_ids
        }
    }

    pub fn all_artist_names(&self) -> &[String] {
        &self.artist_names
    }

    pub fn artist_display_name(&self) -> String {
        if self.artist_names.is_empty() {
            String::new()
        } else {
            self.artist_names.join(", ")
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub album_id: String,
    pub artist_id: String,
    pub title: String,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub duration_ms: u32,
    pub codec: Codec,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub file_relpath: String,
    pub file_size: u64,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Mp3,
    Flac,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverRef {
    Embedded { track_id: String },
    File { relpath: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeekPoint {
    pub t_ms: u32,
    pub byte: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeekIndex {
    pub duration_ms: u32,
    pub points: Vec<SeekPoint>,
    pub hint: String,
}

pub fn stable_id(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

pub fn artist_identity_key(name: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in name.trim().chars() {
        for lower in ch.to_lowercase() {
            let mapped = fold_identity_char(lower);
            if mapped.is_ascii_alphanumeric() {
                out.push(mapped);
                last_space = false;
            } else if !last_space {
                out.push(' ');
                last_space = true;
            }
        }
    }
    out.trim().to_string()
}

pub fn artist_identity_id(name: &str) -> String {
    let key = artist_identity_key(name);
    if key.is_empty() {
        stable_id(name.trim())
    } else {
        stable_id(&key)
    }
}

fn fold_identity_char(ch: char) -> char {
    match ch {
        '\u{00e0}' | '\u{00e1}' | '\u{00e2}' | '\u{00e4}' | '\u{00e3}' | '\u{00e5}'
        | '\u{0101}' | '\u{0103}' | '\u{0105}' | '\u{01ce}' | '\u{00aa}' => 'a',
        '\u{00e6}' => 'a',
        '\u{00e7}' | '\u{0107}' | '\u{0109}' | '\u{010b}' | '\u{010d}' => 'c',
        '\u{010f}' | '\u{0111}' => 'd',
        '\u{00e8}' | '\u{00e9}' | '\u{00ea}' | '\u{00eb}' | '\u{0113}' | '\u{0115}'
        | '\u{0117}' | '\u{0119}' | '\u{011b}' => 'e',
        '\u{0192}' => 'f',
        '\u{011d}' | '\u{011f}' | '\u{0121}' | '\u{0123}' => 'g',
        '\u{0125}' | '\u{0127}' => 'h',
        '\u{00ec}' | '\u{00ed}' | '\u{00ee}' | '\u{00ef}' | '\u{0129}' | '\u{012b}'
        | '\u{012d}' | '\u{012f}' | '\u{0131}' => 'i',
        '\u{0135}' => 'j',
        '\u{0137}' => 'k',
        '\u{013a}' | '\u{013c}' | '\u{013e}' | '\u{0140}' | '\u{0142}' => 'l',
        '\u{00f1}' | '\u{0144}' | '\u{0146}' | '\u{0148}' | '\u{0149}' => 'n',
        '\u{00f2}' | '\u{00f3}' | '\u{00f4}' | '\u{00f6}' | '\u{00f5}' | '\u{014d}'
        | '\u{014f}' | '\u{0151}' | '\u{00f8}' | '\u{00ba}' => 'o',
        '\u{0153}' => 'o',
        '\u{0155}' | '\u{0157}' | '\u{0159}' => 'r',
        '\u{015b}' | '\u{015d}' | '\u{015f}' | '\u{0161}' | '\u{00df}' => 's',
        '\u{0163}' | '\u{0165}' | '\u{0167}' => 't',
        '\u{00f9}' | '\u{00fa}' | '\u{00fb}' | '\u{00fc}' | '\u{0169}' | '\u{016b}'
        | '\u{016d}' | '\u{016f}' | '\u{0171}' | '\u{0173}' => 'u',
        '\u{0175}' => 'w',
        '\u{00fd}' | '\u{00ff}' | '\u{0177}' => 'y',
        '\u{017a}' | '\u{017c}' | '\u{017e}' => 'z',
        _ => ch,
    }
}

pub fn relpath_from(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(path_to_slash_string(rel))
}

pub fn join_relpath(root: &Path, relpath: &str) -> PathBuf {
    let mut out = PathBuf::from(root);
    for part in relpath.split('/') {
        if part.is_empty() {
            continue;
        }
        out.push(part);
    }
    out
}

fn path_to_slash_string(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        artist_identity_id, artist_identity_key, join_relpath, relpath_from, stable_id, Album,
    };

    #[test]
    fn stable_id_is_deterministic() {
        let first = stable_id("Artist/Album/Track.mp3");
        let second = stable_id("Artist/Album/Track.mp3");
        assert_eq!(first, second);
        assert_ne!(first, stable_id("Artist/Album/Track2.mp3"));
    }

    #[test]
    fn artist_identity_normalizes_case_and_punctuation() {
        assert_eq!(
            artist_identity_key("System Of A Down"),
            artist_identity_key("System of a Down")
        );
        assert_eq!(artist_identity_key("Jay-Z"), artist_identity_key("jay z"));
        assert_eq!(
            artist_identity_id("System Of A Down"),
            artist_identity_id("System of a Down")
        );
    }

    #[test]
    fn relpath_round_trip_uses_forward_slashes() {
        let root = Path::new("/library");
        let track = Path::new("/library/Artist Name/Album Name/Track 01.flac");

        let rel = relpath_from(root, track).unwrap();

        assert_eq!(rel, "Artist Name/Album Name/Track 01.flac");
        assert_eq!(join_relpath(root, &rel), track);
    }

    #[test]
    fn album_artist_helpers_fall_back_and_join_values() {
        let single_artist_album = Album {
            id: "album-1".to_string(),
            artist_id: "artist-1".to_string(),
            artist_ids: Vec::new(),
            artist_names: vec!["Only Artist".to_string()],
            title: "First".to_string(),
            year: Some(2024),
            folder_relpath: "Only Artist/First".to_string(),
            cover_ref: None,
            genres: Vec::new(),
            summary: None,
        };
        assert_eq!(
            single_artist_album.all_artist_ids(),
            &["artist-1".to_string()]
        );
        assert_eq!(single_artist_album.artist_display_name(), "Only Artist");

        let compilation = Album {
            id: "album-2".to_string(),
            artist_id: "artist-1".to_string(),
            artist_ids: vec!["artist-1".to_string(), "artist-2".to_string()],
            artist_names: vec!["Artist One".to_string(), "Artist Two".to_string()],
            title: "Second".to_string(),
            year: None,
            folder_relpath: "Compilations/Second".to_string(),
            cover_ref: None,
            genres: Vec::new(),
            summary: None,
        };
        assert_eq!(
            compilation.all_artist_ids(),
            &["artist-1".to_string(), "artist-2".to_string()]
        );
        assert_eq!(compilation.artist_display_name(), "Artist One, Artist Two");
    }
}
