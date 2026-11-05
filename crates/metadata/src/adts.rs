//! Read ADTS properties by following frame lengths, never scanning AAC payloads.
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::{synchsafe_u32, TagInfo};

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn read_properties(path: &Path, info: &mut TagInfo) -> io::Result<()> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut prefix = [0; 10];
    reader.read_exact(&mut prefix)?;
    let mut offset = if &prefix[..3] == b"ID3" {
        let size = synchsafe_u32(&prefix[6..10]).ok_or_else(|| invalid("Invalid ID3 size"))?;
        10 + u64::from(size)
            + if prefix[3] == 4 && prefix[5] & 0x10 != 0 {
                10
            } else {
                0
            }
    } else {
        0
    };
    let first_possible_frame = offset;
    let mut sample_rate = None;
    let mut channels = None;
    let mut samples = 0u64;
    let mut audio_bytes = 0u64;

    while offset < length {
        reader.seek(SeekFrom::Start(offset))?;
        let mut header = [0; 7];
        reader.read_exact(&mut header)?;
        if &header[..3] == b"TAG" && length - offset == 128 {
            break;
        }
        if header[0] != 0xff || header[1] & 0xf6 != 0xf0 {
            // Allow the same small leading padding as the format probe, but
            // reject broken frame boundaries after audio has begun.
            if samples == 0 && offset - first_possible_frame < 1024 {
                offset += 1;
                continue;
            }
            return Err(invalid("Invalid ADTS frame boundary"));
        }
        let rate = SAMPLE_RATES
            .get(usize::from((header[2] >> 2) & 0x0f))
            .copied()
            .ok_or_else(|| invalid("Invalid ADTS sample rate"))?;
        if sample_rate.is_some_and(|previous| previous != rate) {
            return Err(invalid("ADTS sample rate changed within the stream"));
        }
        let frame_length = (u64::from(header[3] & 3) << 11)
            | (u64::from(header[4]) << 3)
            | u64::from(header[5] >> 5);
        let header_length = if header[1] & 1 == 0 { 9 } else { 7 };
        if frame_length < header_length || frame_length > length - offset {
            return Err(invalid("Truncated ADTS frame"));
        }
        let channel_configuration = ((header[2] & 1) << 2) | (header[3] >> 6);
        channels = match channel_configuration {
            1..=6 => Some(channel_configuration),
            7 => Some(8),
            // The channel layout lives in a program config element in this case.
            _ => None,
        };
        sample_rate = Some(rate);
        samples += 1024 * u64::from((header[6] & 3) + 1);
        audio_bytes += frame_length;
        offset += frame_length;
    }

    let rate = sample_rate.ok_or_else(|| invalid("No ADTS audio frames found"))?;
    info.sample_rate = Some(rate);
    info.channels = channels;
    info.duration_ms = Some((samples * 1000 / u64::from(rate)).min(u64::from(u32::MAX)) as u32);
    info.bitrate =
        Some((audio_bytes * 8 * u64::from(rate) / samples / 1000).min(u64::from(u32::MAX)) as u32);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::read_properties;
    use crate::TagInfo;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn frame(crc: bool, extra_blocks: u8) -> Vec<u8> {
        let header_length = if crc { 9 } else { 7 };
        let length = header_length + 16;
        let mut bytes = vec![
            0xff,
            if crc { 0xf0 } else { 0xf1 },
            0x4c,
            0x80,
            (length >> 3) as u8,
            ((length & 7) << 5) as u8 | 0x1f,
            0xfc | extra_blocks,
        ];
        bytes.resize(header_length, 0);
        // These payload bytes resemble invalid ADTS headers and must be skipped.
        bytes.extend_from_slice(&[0xff; 16]);
        bytes
    }

    fn properties(bytes: &[u8]) -> std::io::Result<TagInfo> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "phonolite-adts-{}-{nonce}-{}.aac",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::write(&path, bytes).unwrap();
        let mut info = TagInfo::default();
        let result = read_properties(&path, &mut info);
        fs::remove_file(path).unwrap();
        result.map(|()| info)
    }

    #[test]
    fn follows_frame_lengths_and_counts_crc_frames_and_raw_blocks() {
        let mut bytes = frame(false, 0);
        bytes.extend(frame(true, 1));
        let info = properties(&bytes).unwrap();
        assert_eq!(info.sample_rate, Some(48_000));
        assert_eq!(info.channels, Some(2));
        assert_eq!(info.duration_ms, Some(64));
    }

    #[test]
    fn skips_id3_tags_and_rejects_truncated_or_invalid_frames() {
        let mut bytes = b"ID3\x04\0\0\0\0\0\x03abc".to_vec();
        bytes.extend(frame(false, 0));
        bytes.extend_from_slice(b"TAG");
        bytes.resize(bytes.len() + 125, 0);
        assert_eq!(properties(&bytes).unwrap().duration_ms, Some(21));

        let mut truncated = frame(false, 0);
        truncated.pop();
        assert!(properties(&truncated).is_err());
        let mut invalid = frame(false, 0);
        invalid[2] = 0x7c; // Reserved sampling-frequency index 15.
        assert!(properties(&invalid).is_err());
    }
}
