use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::error::LoftyError;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::{AudioFile, ItemKey, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Tag;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub album_artists: Vec<String>,
    /// True when the source supplies artist identities rather than a display credit.
    #[serde(default)]
    pub artists_structured: bool,
    #[serde(default)]
    pub album_artists_structured: bool,
    #[serde(default)]
    pub contributions: Vec<TagContribution>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagContribution {
    pub name: String,
    pub role: String,
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
        read_tag_fields(tag, &mut info);
    }

    info.has_embedded_cover = has_embedded_cover(path)?;

    Ok(info)
}

pub fn read_cover(path: &Path) -> Result<Option<CoverArt>, MetadataError> {
    let tagged_file = lofty::read_from_path(path)?;
    let picture = match pick_tag_picture(tagged_file.tags()) {
        Some(picture) => picture,
        None => return Ok(None),
    };

    let data = picture.data().to_vec();
    let mime = guess_mime(&data);
    Ok(Some(CoverArt { data, mime }))
}

fn read_tag_fields(tag: &Tag, info: &mut TagInfo) {
    info.title = first_text(tag, &ItemKey::TrackTitle);
    info.album = first_text(tag, &ItemKey::AlbumTitle);
    info.album_artist = artist_text(tag, &ItemKey::AlbumArtist);
    info.artist = artist_text(tag, &ItemKey::TrackArtist).or_else(|| info.album_artist.clone());
    // ARTISTS and ALBUMARTISTS are explicit identity lists, commonly written by
    // Picard. Keep ARTIST/ALBUMARTIST as the display credit when both are present.
    info.artists = explicit_artist_values(tag, "ARTISTS");
    info.artists_structured = !info.artists.is_empty();
    if info.artists.is_empty() {
        info.artists = artist_values(tag, &ItemKey::TrackArtist);
        info.artists_structured = info.artists.len() > 1;
    }
    info.album_artists = explicit_artist_values(tag, "ALBUMARTISTS");
    info.album_artists_structured = !info.album_artists.is_empty();
    if info.album_artists.is_empty() {
        info.album_artists = artist_values(tag, &ItemKey::AlbumArtist);
        info.album_artists_structured = info.album_artists.len() > 1;
    }
    if info.album_artist.is_none() && !info.album_artists.is_empty() {
        info.album_artist = Some(info.album_artists.join("; "));
    }
    if info.artists.is_empty() {
        info.artists = info.album_artists.clone();
        info.artists_structured = info.album_artists_structured;
    }
    if info.artist.is_none() && !info.artists.is_empty() {
        info.artist = Some(info.artists.join("; "));
    }
    info.contributions.clear();
    for (key, role) in [
        (ItemKey::Composer, "composer"),
        (ItemKey::Conductor, "conductor"),
        (ItemKey::Performer, "performer"),
        (ItemKey::Producer, "producer"),
    ] {
        for name in artist_values(tag, &key) {
            let (name, role) = if role == "performer" {
                performer_credit(name)
            } else {
                (name, role.to_owned())
            };
            info.contributions.push(TagContribution { name, role });
        }
    }
    info.track_no = tag.get_strings(&ItemKey::TrackNumber).find_map(parse_u16);
    info.disc_no = tag.get_strings(&ItemKey::DiscNumber).find_map(parse_u16);
    // DATE in Vorbis, TDRC in ID3v2, and the MP4 date atom map to RecordingDate.
    info.year = [ItemKey::Year, ItemKey::RecordingDate, ItemKey::ReleaseDate]
        .iter()
        .find_map(|key| tag.get_strings(key).find_map(parse_year));
    for value in tag.get_strings(&ItemKey::Genre) {
        for genre in parse_genres(value) {
            if !info
                .genres
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&genre))
            {
                info.genres.push(genre);
            }
        }
    }
    info.summary = first_text(tag, &ItemKey::Comment);
}

fn performer_credit(name: String) -> (String, String) {
    if let Some((artist, suffix)) = name.rsplit_once(" (") {
        if let Some(instrument) = suffix.strip_suffix(')') {
            let role = instrument.trim().to_lowercase();
            if !artist.trim().is_empty()
                && matches!(
                    role.as_str(),
                    "violin"
                        | "viola"
                        | "cello"
                        | "double bass"
                        | "piano"
                        | "organ"
                        | "harpsichord"
                        | "guitar"
                        | "bass"
                        | "bass guitar"
                        | "drums"
                        | "percussion"
                        | "vocals"
                        | "voice"
                        | "flute"
                        | "oboe"
                        | "clarinet"
                        | "bassoon"
                        | "saxophone"
                        | "trumpet"
                        | "trombone"
                        | "horn"
                        | "tuba"
                        | "harp"
                        | "synthesizer"
                        | "keyboards"
                )
            {
                return (artist.trim().to_owned(), role);
            }
        }
    }
    (name, "performer".into())
}

fn first_text(tag: &Tag, key: &ItemKey) -> Option<String> {
    tag.get_strings(key)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn artist_text(tag: &Tag, key: &ItemKey) -> Option<String> {
    let values = artist_values(tag, key);
    (!values.is_empty()).then(|| values.join("; "))
}

fn artist_values(tag: &Tag, key: &ItemKey) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for value in tag
        .get_strings(key)
        .flat_map(|value| value.split('\0'))
        .map(str::trim)
    {
        if !value.is_empty()
            && !values
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(value))
        {
            values.push(value.to_owned());
        }
    }
    values
}

fn explicit_artist_values(tag: &Tag, key: &str) -> Vec<String> {
    let mut names = Vec::new();
    for item in tag.items() {
        if matches!(item.key(), ItemKey::Unknown(name) if name.eq_ignore_ascii_case(key)) {
            if let lofty::tag::ItemValue::Text(value) = item.value() {
                for name in value
                    .split('\0')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    if !names
                        .iter()
                        .any(|existing: &String| existing.eq_ignore_ascii_case(name))
                    {
                        names.push(name.to_owned());
                    }
                }
            }
        }
    }
    names
}

fn parse_u16(text: &str) -> Option<u16> {
    let head = text.split('/').next().unwrap_or(text).trim();
    head.parse().ok().filter(|number| *number > 0)
}

fn parse_year(text: &str) -> Option<i32> {
    let text = text.trim();
    let year = text.get(..4)?;
    if !year.bytes().all(|byte| byte.is_ascii_digit())
        || text.as_bytes().get(4).is_some_and(u8::is_ascii_digit)
    {
        return None;
    }
    year.parse().ok().filter(|year| *year > 0)
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

fn pick_tag_picture(tags: &[Tag]) -> Option<&Picture> {
    let pictures = tags.iter().flat_map(|tag| tag.pictures());
    pictures
        .clone()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.into_iter().next())
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
    use super::{
        has_embedded_cover, parse_genres, parse_year, pick_tag_picture, read_tag_fields,
        synchsafe_u32, TagInfo,
    };
    use lofty::ogg::VorbisComments;
    use lofty::picture::{Picture, PictureType};
    use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};
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

    #[test]
    fn blank_text_tags_do_not_suppress_artist_and_filename_fallbacks() {
        let mut tag = Tag::new(TagType::VorbisComments);
        for key in [
            ItemKey::TrackTitle,
            ItemKey::AlbumTitle,
            ItemKey::TrackArtist,
            ItemKey::Comment,
        ] {
            tag.insert_text(key, "  \t ".into());
        }
        tag.insert_text(ItemKey::AlbumArtist, "  Janine Jansen  ".into());
        let mut info = TagInfo::default();
        read_tag_fields(&tag, &mut info);

        assert_eq!(info.artist.as_deref(), Some("Janine Jansen"));
        assert_eq!(info.album_artist.as_deref(), Some("Janine Jansen"));
        assert_eq!(info.title, None);
        assert_eq!(info.album, None);
        assert_eq!(info.summary, None);
    }

    #[test]
    fn reads_all_artist_credits_and_genres_from_repeated_vorbis_fields() {
        let mut comments = VorbisComments::new();
        for (key, value) in [
            ("ARTIST", "  "),
            ("ARTIST", "Janine Jansen"),
            ("ARTIST", "Chamber Orchestra"),
            ("ARTIST", "Janine Jansen"),
            ("ALBUMARTIST", "Janine Jansen"),
            ("ALBUMARTIST", "Chamber Orchestra"),
            ("GENRE", " "),
            ("GENRE", "Classical; Baroque"),
            ("GENRE", "Concerto"),
            ("GENRE", "classical"),
        ] {
            comments.push(key.into(), value.into());
        }
        let mut info = TagInfo::default();
        read_tag_fields(&Tag::from(comments), &mut info);

        assert_eq!(
            info.artist.as_deref(),
            Some("Janine Jansen; Chamber Orchestra")
        );
        assert_eq!(info.album_artist, info.artist);
        assert_eq!(info.artists, ["Janine Jansen", "Chamber Orchestra"]);
        assert_eq!(info.album_artists, info.artists);
        assert!(info.artists_structured);
        assert!(info.album_artists_structured);
        assert_eq!(info.genres, ["Classical", "Baroque", "Concerto"]);
    }

    #[test]
    fn structured_artist_lists_preserve_display_wording_and_explicit_roles() {
        let mut comments = VorbisComments::new();
        for (key, value) in [
            ("ARTIST", "Mara & Theo feat. Guest"),
            ("ARTISTS", "Mara\0Theo\0Guest"),
            ("ALBUMARTIST", "Mara & Theo"),
            ("ALBUMARTISTS", "Mara"),
            ("ALBUMARTISTS", "Theo"),
            ("COMPOSER", "Composer"),
            ("CONDUCTOR", "Conductor"),
            ("PERFORMER", "Earth, Wind & Fire"),
            ("PRODUCER", "Producer"),
        ] {
            comments.push(key.into(), value.into());
        }
        let mut info = TagInfo::default();
        read_tag_fields(&Tag::from(comments), &mut info);
        assert_eq!(info.artist.as_deref(), Some("Mara & Theo feat. Guest"));
        assert_eq!(info.artists, ["Mara", "Theo", "Guest"]);
        assert_eq!(info.album_artists, ["Mara", "Theo"]);
        assert!(info.artists_structured);
        assert!(info.album_artists_structured);
        assert_eq!(
            info.contributions
                .iter()
                .map(|credit| (credit.name.as_str(), credit.role.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Composer", "composer"),
                ("Conductor", "conductor"),
                ("Earth, Wind & Fire", "performer"),
                ("Producer", "producer")
            ]
        );
    }

    #[test]
    fn punctuation_does_not_split_a_band_identity() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::TrackArtist, "Earth, Wind & Fire".into());
        let mut info = TagInfo::default();
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.artists, ["Earth, Wind & Fire"]);
        assert!(!info.artists_structured);
        tag.insert_text(ItemKey::TrackArtist, "AC/DC\0Guest".into());
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.artists, ["AC/DC", "Guest"]);
        assert!(info.artists_structured);
    }

    #[test]
    fn single_explicit_identity_tag_is_authoritative_even_when_it_contains_a_comma() {
        let mut comments = VorbisComments::new();
        comments.push("ARTIST".into(), "Tyler, The Creator".into());
        comments.push("ARTISTS".into(), "Tyler, The Creator".into());
        let mut info = TagInfo::default();
        read_tag_fields(&Tag::from(comments), &mut info);
        assert_eq!(info.artists, ["Tyler, The Creator"]);
        assert!(info.artists_structured);
    }

    #[test]
    fn performer_instrument_suffix_is_a_role_without_splitting_other_parentheses() {
        let mut comments = VorbisComments::new();
        comments.push("PERFORMER".into(), "Elena Hart (violin)".into());
        comments.push("PERFORMER".into(), "Band (The Original Lineup)".into());
        let mut info = TagInfo::default();
        read_tag_fields(&Tag::from(comments), &mut info);
        assert_eq!(info.contributions[0].name, "Elena Hart");
        assert_eq!(info.contributions[0].role, "violin");
        assert_eq!(info.contributions[1].name, "Band (The Original Lineup)");
        assert_eq!(info.contributions[1].role, "performer");
    }

    #[test]
    fn reads_date_year_from_vorbis_date_mapping() {
        let mut comments = VorbisComments::new();
        comments.insert("DATE".into(), "2005-08-29".into());
        let tag = Tag::from(comments);
        assert_eq!(tag.get_string(&ItemKey::Year), None);
        let mut info = TagInfo::default();
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.year, Some(2005));
    }

    #[test]
    fn valid_year_has_priority_and_bad_year_can_fall_back_to_date() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::Year, "2005".into());
        tag.insert_text(ItemKey::RecordingDate, "2010-12-31".into());
        let mut info = TagInfo::default();
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.year, Some(2005));

        for invalid in [
            " ",
            "0",
            "99",
            "0000",
            "12345",
            "unknown 2005",
            "05/29/2005",
        ] {
            tag.insert_text(ItemKey::Year, invalid.into());
            read_tag_fields(&tag, &mut info);
            assert_eq!(info.year, Some(2010), "invalid year: {invalid}");
        }
    }

    #[test]
    fn rejects_malformed_years_and_accepts_iso_dates() {
        assert_eq!(parse_year(" 2005-08-29 "), Some(2005));
        assert_eq!(parse_year("2005"), Some(2005));
        for invalid in [
            "",
            "200",
            "0000",
            "20050",
            "unknown 2005",
            "05/29/2005",
            "\u{00E9}2005",
        ] {
            assert_eq!(parse_year(invalid), None, "invalid year: {invalid}");
        }
    }

    #[test]
    fn treats_zero_numbers_as_missing_and_keeps_number_total_tags() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::TrackNumber, "0/10".into());
        tag.insert_text(ItemKey::DiscNumber, " 0 ".into());
        let mut info = TagInfo::default();
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.track_no, None);
        assert_eq!(info.disc_no, None);

        tag.push(TagItem::new(
            ItemKey::TrackNumber,
            ItemValue::Text(" 3/10 ".into()),
        ));
        tag.push(TagItem::new(
            ItemKey::DiscNumber,
            ItemValue::Text("2/2".into()),
        ));
        read_tag_fields(&tag, &mut info);
        assert_eq!(info.track_no, Some(3));
        assert_eq!(info.disc_no, Some(2));
    }

    #[test]
    fn chooses_front_cover_across_all_tags() {
        let back = Picture::new_unchecked(PictureType::CoverBack, None, None, vec![1]);
        let front = Picture::new_unchecked(PictureType::CoverFront, None, None, vec![2]);
        let mut primary = Tag::new(TagType::Id3v2);
        primary.push_picture(back);
        let mut secondary = Tag::new(TagType::Ape);
        secondary.push_picture(front);

        let tags = [primary, secondary];
        assert_eq!(pick_tag_picture(&tags).unwrap().data(), &[2]);
        assert_eq!(pick_tag_picture(&tags[1..]).unwrap().data(), &[2]);
        assert_eq!(pick_tag_picture(&tags[..1]).unwrap().data(), &[1]);
        assert!(pick_tag_picture(&[]).is_none());
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
