use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::error::LoftyError;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::{AudioFile, ItemKey, TaggedFileExt};
use lofty::probe::Probe;

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
    let tagged_file = Probe::open(path)?
        .options(ParseOptions::new().read_cover_art(false))
        .read()?;
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

    if let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    {
        info.title = tag.get_string(&ItemKey::TrackTitle).map(|v| v.to_string());
        info.album = tag.get_string(&ItemKey::AlbumTitle).map(|v| v.to_string());
        let album_artist = tag.get_string(&ItemKey::AlbumArtist).map(|v| v.to_string());
        let track_artist = tag.get_string(&ItemKey::TrackArtist).map(|v| v.to_string());
        info.artist = track_artist.or_else(|| album_artist.clone());
        info.album_artist = album_artist;
        info.track_no = tag.get_string(&ItemKey::TrackNumber).and_then(parse_u16);
        info.disc_no = tag.get_string(&ItemKey::DiscNumber).and_then(parse_u16);
        info.year = tag.get_string(&ItemKey::Year).and_then(parse_year);
        if let Some(value) = tag.get_string(&ItemKey::Genre) {
            info.genres = parse_genres(value);
        }
        info.summary = tag.get_string(&ItemKey::Comment).map(|s| s.to_string());
    }

    info.has_embedded_cover = has_embedded_cover(path)?;

    Ok(info)
}

pub fn read_cover(path: &Path) -> Result<Option<CoverArt>, MetadataError> {
    let tagged_file = lofty::read_from_path(path)?;
    let tag = match tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    {
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
    let text = normalize_genre_separators(text);
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

fn normalize_genre_separators(text: &str) -> String {
    text.replace("&bull;", "|")
        .replace("&#8226;", "|")
        .replace("&#x2022;", "|")
        .replace("&middot;", "|")
        .replace("\u{2022}", "|")
        .replace("\u{00B7}", "|")
        .replace("\u{00E2}\u{20AC}\u{00A2}", "|")
        .replace("\u{00E2}\u{0080}\u{00A2}", "|")
        .replace("\u{00C2}\u{20AC}\u{00A2}", "|")
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

fn has_embedded_cover(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 10];
    let read = file.read(&mut header)?;
    if read >= 10 && &header[..3] == b"ID3" {
        let (has_picture, tag_end) = id3v2_has_picture(&mut file, &header)?;
        if has_picture {
            return Ok(true);
        }
        file.seek(SeekFrom::Start(tag_end))?;
    } else {
        file.seek(SeekFrom::Start(0))?;
    }

    let mut magic = [0u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) if &magic == b"fLaC" => flac_has_picture_block(&mut file),
        Ok(()) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

fn id3v2_has_picture<R: Read + Seek>(reader: &mut R, header: &[u8; 10]) -> io::Result<(bool, u64)> {
    let major = header[3];
    if !(2..=4).contains(&major) {
        return Ok((false, 10));
    }

    let tag_size = synchsafe_u32(&header[6..10]).unwrap_or(0) as u64;
    let has_footer = major == 4 && header[5] & 0x10 != 0;
    let tag_end = 10u64.saturating_add(tag_size);
    let full_tag_end = tag_end.saturating_add(if has_footer { 10 } else { 0 });
    let mut pos = 10u64;

    if major >= 3 && header[5] & 0x40 != 0 {
        let mut ext_header = [0u8; 4];
        if !read_exact_or_eof(reader, &mut ext_header)? {
            return Ok((false, full_tag_end));
        }
        let ext_size = if major == 4 {
            synchsafe_u32(&ext_header).unwrap_or(0)
        } else {
            u32::from_be_bytes(ext_header)
        } as u64;
        let total_ext_size = if major == 4 {
            ext_size
        } else {
            ext_size.saturating_add(4)
        };
        if total_ext_size < 4 || pos.saturating_add(total_ext_size) > tag_end {
            return Ok((false, full_tag_end));
        }
        reader.seek(SeekFrom::Current((total_ext_size - 4) as i64))?;
        pos += total_ext_size;
    }

    while pos < tag_end {
        if major == 2 {
            let mut frame_header = [0u8; 6];
            if pos + frame_header.len() as u64 > tag_end
                || !read_exact_or_eof(reader, &mut frame_header)?
            {
                break;
            }
            pos += frame_header.len() as u64;
            if frame_header.iter().all(|byte| *byte == 0) {
                break;
            }
            let frame_size = u24(&frame_header[3..6]) as u64;
            if &frame_header[..3] == b"PIC" {
                return Ok((true, full_tag_end));
            }
            if frame_size == 0 || pos.saturating_add(frame_size) > tag_end {
                break;
            }
            reader.seek(SeekFrom::Current(frame_size as i64))?;
            pos += frame_size;
        } else {
            let mut frame_header = [0u8; 10];
            if pos + frame_header.len() as u64 > tag_end
                || !read_exact_or_eof(reader, &mut frame_header)?
            {
                break;
            }
            pos += frame_header.len() as u64;
            if frame_header.iter().all(|byte| *byte == 0) {
                break;
            }
            let frame_size = if major == 4 {
                synchsafe_u32(&frame_header[4..8]).unwrap_or(0)
            } else {
                u32::from_be_bytes([
                    frame_header[4],
                    frame_header[5],
                    frame_header[6],
                    frame_header[7],
                ])
            } as u64;
            if &frame_header[..4] == b"APIC" {
                return Ok((true, full_tag_end));
            }
            if frame_size == 0 || pos.saturating_add(frame_size) > tag_end {
                break;
            }
            reader.seek(SeekFrom::Current(frame_size as i64))?;
            pos += frame_size;
        }
    }

    Ok((false, full_tag_end))
}

fn flac_has_picture_block<R: Read + Seek>(reader: &mut R) -> io::Result<bool> {
    loop {
        let mut header = [0u8; 4];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(err) => return Err(err),
        }
        let is_last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let block_len = u24(&header[1..4]) as u64;
        if block_type == 6 {
            return Ok(true);
        }
        reader.seek(SeekFrom::Current(block_len as i64))?;
        if is_last {
            return Ok(false);
        }
    }
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<bool> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

fn synchsafe_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(bytes[0]) << 21)
            | (u32::from(bytes[1]) << 14)
            | (u32::from(bytes[2]) << 7)
            | u32::from(bytes[3]),
    )
}

fn u24(bytes: &[u8]) -> u32 {
    (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2])
}

#[cfg(test)]
mod tests {
    use super::{has_embedded_cover, parse_genres, synchsafe_u32};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_id3v24_apic_without_picture_payload() {
        let path = temp_path("id3v24-apic.mp3");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ID3\x04\0\0");
        bytes.extend_from_slice(&syncsafe_bytes(24));
        bytes.extend_from_slice(b"TIT2");
        bytes.extend_from_slice(&syncsafe_bytes(1));
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(b"APIC");
        bytes.extend_from_slice(&syncsafe_bytes(3));
        bytes.extend_from_slice(&[0, 0, 0, 0, 0]);
        fs::write(&path, bytes).unwrap();

        assert!(has_embedded_cover(&path).unwrap());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn detects_flac_picture_block_without_reading_block() {
        let path = temp_path("picture.flac");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"fLaC");
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]);
        bytes.extend_from_slice(&[0; 34]);
        bytes.extend_from_slice(&[0x86, 0x00, 0x00, 0x04]);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        fs::write(&path, bytes).unwrap();

        assert!(has_embedded_cover(&path).unwrap());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_invalid_synchsafe_values() {
        assert_eq!(synchsafe_u32(&[0, 0, 0, 1]), Some(1));
        assert_eq!(synchsafe_u32(&[0x80, 0, 0, 1]), None);
    }

    #[test]
    fn splits_bullet_and_mojibake_genres() {
        assert_eq!(
            parse_genres("Rock \u{2022} Alternative Rock \u{00E2}\u{20AC}\u{00A2} Industrial Rock"),
            vec!["Rock", "Alternative Rock", "Industrial Rock"]
        );
        assert_eq!(
            parse_genres("Metal \u{00C2}\u{20AC}\u{00A2} Nu Metal"),
            vec!["Metal", "Nu Metal"]
        );
    }

    fn syncsafe_bytes(value: u32) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("phonolite-metadata-{nonce}-{name}"))
    }
}
