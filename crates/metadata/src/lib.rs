use std::path::Path;

use lofty::error::LoftyError;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::{AudioFile, ItemKey, TaggedFileExt};

#[derive(Debug, Default, Clone)]
pub struct TagInfo {
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub year: Option<i32>,
    pub duration_ms: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub has_embedded_cover: bool,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CoverArt {
    pub data: Vec<u8>,
    pub mime: Option<String>,
}

#[derive(Debug)]
pub enum MetadataError {
    Io(std::io::Error),
    Lofty(LoftyError),
}

impl From<std::io::Error> for MetadataError {
    fn from(err: std::io::Error) -> Self {
        MetadataError::Io(err)
    }
}

impl From<LoftyError> for MetadataError {
    fn from(err: LoftyError) -> Self {
        MetadataError::Lofty(err)
    }
}

pub fn read_tags(path: &Path) -> Result<TagInfo, MetadataError> {
    let tagged_file = lofty::read_from_path(path)?;
    let properties = tagged_file.properties();

    let mut info = TagInfo::default();

    let duration_ms = properties.duration().as_millis();
    if duration_ms > 0 {
        let clamped = duration_ms.min(u128::from(u32::MAX)) as u32;
        info.duration_ms = Some(clamped);
    }

    info.sample_rate = properties.sample_rate();
    info.channels = properties.channels();
    info.bitrate = properties.audio_bitrate().or(properties.overall_bitrate());

    if let Some(tag) = tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
        info.title = tag.get_string(&ItemKey::TrackTitle).map(|v| v.to_string());
        info.album = tag.get_string(&ItemKey::AlbumTitle).map(|v| v.to_string());
        let album_artist = tag.get_string(&ItemKey::AlbumArtist).map(|v| v.to_string());
        let track_artist = tag.get_string(&ItemKey::TrackArtist).map(|v| v.to_string());
        info.artist = track_artist.or_else(|| album_artist.clone());
        info.album_artist = album_artist;
        info.track_no = tag
            .get_string(&ItemKey::TrackNumber)
            .and_then(parse_u16);
        info.disc_no = tag
            .get_string(&ItemKey::DiscNumber)
            .and_then(parse_u16);
        info.year = tag.get_string(&ItemKey::Year).and_then(parse_year);
        if let Some(value) = tag.get_string(&ItemKey::Genre) {
            info.genres = parse_genres(value);
        }
        info.summary = tag.get_string(&ItemKey::Comment).map(|s| s.to_string());
        info.has_embedded_cover = !tag.pictures().is_empty();
    }

    Ok(info)
}

pub fn read_cover(path: &Path) -> Result<Option<CoverArt>, MetadataError> {
    let tagged_file = lofty::read_from_path(path)?;
    let tag = match tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
        Some(tag) => tag,
        None => return Ok(None),
    };

    let picture = match pick_picture(tag.pictures()) {
        Some(picture) => picture,
        None => return Ok(None),
    };

    let data = picture.data().to_vec();
    let mime = guess_mime(&data);
    Ok(Some(CoverArt { data, mime }))
}

fn parse_u16(text: &str) -> Option<u16> {
    let head = text.split('/').next().unwrap_or(text).trim();
    head.parse().ok()
}

fn parse_year(text: &str) -> Option<i32> {
    let mut digits = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() == 4 {
                break;
            }
        } else if !digits.is_empty() {
            break;
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn parse_genres(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in text.split(&[';', ',', '/', '|', '\0'][..]) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(trimmed.to_string());
    }
    if out.is_empty() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn pick_picture(pictures: &[Picture]) -> Option<&Picture> {
    for picture in pictures {
        if picture.pic_type() == PictureType::CoverFront {
            return Some(picture);
        }
    }
    pictures.first()
}

fn guess_mime(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg".to_string())
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some("image/png".to_string())
    } else {
        None
    }
}
