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
    pub title: String,
    pub year: Option<i32>,
    pub folder_relpath: String,
    pub cover_ref: Option<CoverRef>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub summary: Option<String>,
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
    use super::stable_id;

    #[test]
    fn stable_id_is_deterministic() {
        let first = stable_id("Artist/Album/Track.mp3");
        let second = stable_id("Artist/Album/Track.mp3");
        assert_eq!(first, second);
        assert_ne!(first, stable_id("Artist/Album/Track2.mp3"));
    }
}
