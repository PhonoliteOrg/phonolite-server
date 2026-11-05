//! Continuous, bounded-memory Ogg Opus generation for durable downloads.
use std::io::{self, Write};
use std::path::Path;

use codecs_ffi::OpusEncoderWrapper;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error;

use super::{interleaved_samples, open_format, opus, SincResampler, TARGET_SAMPLE_RATE};

const FRAME_SIZE: usize = 960;

/// Encode all decoded audible samples, rather than relying on rounded library duration.
/// The callback is checked at read/encode boundaries and can cancel a paused job.
pub(crate) fn encode(
    source: &Path,
    bitrate: u32,
    output: &mut impl Write,
    cancelled: &impl Fn() -> bool,
) -> Result<u64, String> {
    let bundle = open_format(source)?;
    let expected_frames = super::source_output_frames(&bundle.track_codec_params);
    let mut format = bundle.format;
    let mut decoder = opus::make_decoder(&bundle.track_codec_params, &DecoderOptions::default())
        .map_err(|error| error.to_string())?;
    let mut encoder = None;
    let mut writer = None;
    let mut resampler = None;
    let mut samples = None;
    let mut rate = 0;
    let mut channels = 0;
    let mut pcm = Vec::new();
    let mut audible = 0u64;
    let mut pre_skip = 0u16;
    loop {
        check(cancelled)?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id() != bundle.track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).map_err(|error| error.to_string())?;
        let (trim_start, trim_end) =
            super::packet_trim(bundle.track_codec_params.codec, &packet, decoded.frames())?;
        if decoded.frames() == trim_start + trim_end {
            continue;
        }
        let spec = *decoded.spec();
        if encoder.is_none() {
            rate = spec.rate;
            channels = spec.channels.count();
            if !matches!(channels, 1 | 2) {
                return Err("offline Opus supports mono and stereo audio".into());
            }
            let mut created = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels as u8, bitrate)
                .map_err(|error| error.to_string())?;
            created.set_vbr(true).map_err(|error| error.to_string())?;
            pre_skip = u16::try_from(created.lookahead().map_err(|error| error.to_string())?)
                .map_err(|_| "Opus encoder delay exceeds file limit")?;
            writer = Some(OggWriter::new(output, channels as u8, pre_skip).map_err(io_error)?);
            encoder = Some(created);
            if rate != TARGET_SAMPLE_RATE {
                resampler = Some(SincResampler::new(
                    rate,
                    TARGET_SAMPLE_RATE,
                    channels as u8,
                    0,
                ));
            }
        } else if rate != spec.rate || channels != spec.channels.count() {
            return Err("source audio format changes within a track".into());
        }
        let input = interleaved_samples(&mut samples, decoded);
        let input = &input[trim_start * channels..input.len() - trim_end * channels];
        let converted;
        let input = if let Some(resampler) = resampler.as_mut() {
            converted = resampler.process(input);
            converted.as_slice()
        } else {
            input
        };
        let take = audible_samples(input, channels, audible, expected_frames);
        audible += (take.len() / channels) as u64;
        pcm.extend_from_slice(take);
        encode_ready(
            encoder.as_mut().unwrap(),
            writer.as_mut().unwrap(),
            &mut pcm,
            channels,
            cancelled,
        )?;
    }
    let Some(mut encoder) = encoder else {
        return Err("source has no decodable audio".into());
    };
    let mut writer = writer.unwrap();
    if let Some(resampler) = resampler.as_mut() {
        let tail = resampler.finish();
        let take = audible_samples(&tail, channels, audible, expected_frames);
        audible += (take.len() / channels) as u64;
        pcm.extend_from_slice(take);
    }
    if audible == 0 {
        return Err("source has no audible samples".into());
    }
    if expected_frames.is_some_and(|expected| expected != audible) {
        return Err(
            "decoded source duration differs from its declared audible sample count".into(),
        );
    }
    // Pad only to drain encoder lookahead. The final granule removes this padding.
    pcm.resize(pcm.len() + usize::from(pre_skip) * channels, 0);
    let frame_samples = FRAME_SIZE * channels;
    pcm.resize(pcm.len().div_ceil(frame_samples) * frame_samples, 0);
    encode_ready(&mut encoder, &mut writer, &mut pcm, channels, cancelled)?;
    check(cancelled)?;
    writer
        .finish(audible + u64::from(pre_skip))
        .map_err(io_error)?;
    Ok(audible)
}

fn check(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("download preparation interrupted".into())
    } else {
        Ok(())
    }
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}

fn audible_samples(input: &[i16], channels: usize, written: u64, expected: Option<u64>) -> &[i16] {
    // Vorbis and some containers decode a final overlap/padded tail beyond the
    // declared audible boundary. Keep that trusted boundary, while still
    // rejecting inputs that end before it. Unknown lengths run to decoded EOF.
    let frames = (input.len() / channels) as u64;
    let take = expected
        .map(|total| total.saturating_sub(written).min(frames))
        .unwrap_or(frames) as usize;
    &input[..take * channels]
}

fn encode_ready(
    encoder: &mut OpusEncoderWrapper,
    writer: &mut OggWriter<'_, impl Write>,
    pcm: &mut Vec<i16>,
    channels: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<(), String> {
    let needed = FRAME_SIZE * channels;
    let mut consumed = 0;
    while pcm.len() - consumed >= needed {
        check(cancelled)?;
        let packet = encoder
            .encode(&pcm[consumed..consumed + needed], FRAME_SIZE)
            .map_err(|error| error.to_string())?;
        writer.packet(packet).map_err(io_error)?;
        consumed += needed;
    }
    pcm.copy_within(consumed.., 0);
    pcm.truncate(pcm.len() - consumed);
    Ok(())
}

/// Retain at most one second of packets for efficient pages and exact end trimming.
struct OggWriter<'a, W: Write> {
    output: &'a mut W,
    sequence: u32,
    granule: u64,
    pending: Vec<Vec<u8>>,
}

impl<'a, W: Write> OggWriter<'a, W> {
    fn new(output: &'a mut W, channels: u8, pre_skip: u16) -> io::Result<Self> {
        let mut writer = Self {
            output,
            sequence: 0,
            granule: 0,
            pending: Vec::new(),
        };
        let mut head = b"OpusHead\x01".to_vec();
        head.push(channels);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        writer.page(&[head], 0, 2)?;
        let vendor = b"Phonolite";
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        writer.page(&[tags], 0, 0)?;
        Ok(writer)
    }

    fn packet(&mut self, packet: Vec<u8>) -> io::Result<()> {
        let segments: usize = self
            .pending
            .iter()
            .map(|packet| packet.len() / 255 + 1)
            .sum();
        if self.pending.len() >= 50 || segments + packet.len() / 255 + 1 > 255 {
            let previous = std::mem::take(&mut self.pending);
            self.granule += previous.len() as u64 * FRAME_SIZE as u64;
            self.page(&previous, self.granule, 0)?;
        }
        self.pending.push(packet);
        Ok(())
    }

    fn finish(mut self, final_granule: u64) -> io::Result<()> {
        let packets = std::mem::take(&mut self.pending);
        if packets.is_empty() {
            return Err(io::Error::other("no encoded audio"));
        }
        if final_granule <= self.granule
            || final_granule > self.granule + packets.len() as u64 * FRAME_SIZE as u64
        {
            return Err(io::Error::other("invalid final Opus granule"));
        }
        self.page(&packets, final_granule, 4)?;
        self.output.flush()
    }

    fn page(&mut self, packets: &[Vec<u8>], granule: u64, flags: u8) -> io::Result<()> {
        let segments: usize = packets.iter().map(|packet| packet.len() / 255 + 1).sum();
        let length: usize = packets.iter().map(Vec::len).sum();
        if segments > 255 {
            return Err(io::Error::other("Opus packet exceeds Ogg page"));
        }
        let mut page = Vec::with_capacity(27 + segments + length);
        page.extend_from_slice(b"OggS\x00");
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&0x50484f4eu32.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(segments as u8);
        for packet in packets {
            page.resize(page.len() + packet.len() / 255, 255);
            page.push((packet.len() % 255) as u8);
        }
        for packet in packets {
            page.extend_from_slice(packet);
        }
        let mut crc = 0u32;
        for byte in &page {
            crc ^= u32::from(*byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x80000000 != 0 {
                    (crc << 1) ^ 0x04c11db7
                } else {
                    crc << 1
                };
            }
        }
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.output.write_all(&page)?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    #[test]
    fn all_presets_are_standard_ogg_with_exact_audible_duration() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/tone.wav");
        for bitrate in [64_000, 96_000, 128_000, 192_000] {
            let mut bytes = Vec::new();
            let frames = encode(&source, bitrate, &mut bytes, &|| false).unwrap();
            assert_eq!(&bytes[..4], b"OggS");
            let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/offline-download-fixtures");
            std::fs::create_dir_all(&fixture_dir).unwrap();
            std::fs::write(fixture_dir.join(format!("tone_{bitrate}.opus")), &bytes).unwrap();
            let stream =
                MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
            let mut hint = Hint::new();
            hint.with_extension("opus");
            let mut format = symphonia::default::get_probe()
                .format(
                    &hint,
                    stream,
                    &FormatOptions {
                        enable_gapless: true,
                        ..Default::default()
                    },
                    &MetadataOptions::default(),
                )
                .unwrap()
                .format;
            let params = format.default_track().unwrap().codec_params.clone();
            assert_eq!(params.n_frames, Some(frames));
            let mut decoder = opus::make_decoder(&params, &DecoderOptions::default()).unwrap();
            let mut decoded = 0;
            while let Ok(packet) = format.next_packet() {
                decoded += decoder.decode(&packet).unwrap().frames() as u64;
            }
            assert_eq!(decoded, frames);
        }
    }

    #[test]
    fn cancelled_conversion_stops_before_publication() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/tone.wav");
        assert!(encode(&source, 64_000, &mut Vec::new(), &|| true).is_err());
    }

    fn decoded_frame_count(bytes: Vec<u8>) -> u64 {
        let stream =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("opus");
        let mut format = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .unwrap()
            .format;
        let params = format.default_track().unwrap().codec_params.clone();
        let mut decoder = opus::make_decoder(&params, &DecoderOptions::default()).unwrap();
        let mut frames = 0;
        while let Ok(packet) = format.next_packet() {
            frames += decoder.decode(&packet).unwrap().frames() as u64;
        }
        assert_eq!(params.n_frames, Some(frames));
        frames
    }

    #[test]
    fn supported_original_formats_encode_complete_audible_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for name in [
            "tone.flac",
            "tone.mp3",
            "formats/opus.opus",
            "formats/long-opus.oga",
            "formats/vorbis.ogg",
            "formats/aac.m4a",
            "formats/alac.m4a",
            "formats/tone.aac",
            "formats/tone.aiff",
        ] {
            let mut bytes = Vec::new();
            let frames = encode(&root.join(name), 128_000, &mut bytes, &|| false)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(decoded_frame_count(bytes), frames, "{name}");
            assert!(frames >= 19_200, "{name}: {frames}");
        }
    }

    fn wave(rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let data_size = frames as u32 * u32::from(channels) * 2;
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        for frame in 0..frames {
            let sample = ((frame as f64 * 0.053).sin() * 7000.0) as i16;
            for _ in 0..channels {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn resampling_44100_and_96000_preserves_exact_rounded_audible_length() {
        let path =
            std::env::temp_dir().join(format!("phonolite-rendition-{}.wav", uuid::Uuid::new_v4()));
        for (rate, channels, frames) in [(44_100, 1, 17_641), (96_000, 2, 38_401)] {
            std::fs::write(&path, wave(rate, channels, frames)).unwrap();
            let expected = (frames as u64 * 48_000 + u64::from(rate / 2)) / u64::from(rate);
            let mut bytes = Vec::new();
            assert_eq!(
                encode(&path, 96_000, &mut bytes, &|| false).unwrap(),
                expected
            );
            assert_eq!(decoded_frame_count(bytes), expected);
        }
        let mut truncated = wave(48_000, 2, 48_000);
        truncated.truncate(truncated.len() - 10_000);
        std::fs::write(&path, truncated).unwrap();
        assert!(encode(&path, 96_000, &mut Vec::new(), &|| false).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
