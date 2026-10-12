use codecs_ffi::OpusEncoderWrapper;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::stream_cache::CacheWriter;

const TARGET_SAMPLE_RATE: u32 = 48_000;
const MAX_SOURCE_PREROLL_MS: u32 = 30_000;

pub struct BitrateSelector {
    pub quality: TranscodeQuality,
    pub fixed_bitrate_bps: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TranscodeMode {
    Auto,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TranscodeQuality {
    High,
    Medium,
    Low,
}

/// A range has its own encoder epoch and explicit delay. Production fills only
/// this bounded range; it never waits for a playback/PCM consumer.
pub fn transcode_raw_opus_range_cache(
    path: &Path,
    selector: BitrateSelector,
    frame_ms: u32,
    meta: RawOpusMeta,
    start_ms: u32,
    end_ms: u32,
    cache: &mut CacheWriter,
) -> Result<(), String> {
    let frame_ms = validate_frame_ms(frame_ms)?;
    if end_ms < start_ms {
        return Err("invalid transcode range".into());
    }
    let bundle = open_format(path)?;
    let mut format = bundle.format;
    let mut track_id = bundle.track_id;
    let mut codec_params = bundle.track_codec_params;
    let mut time_base = bundle.track_time_base;
    let (seek_used, seek_skip) = apply_seek(
        path,
        &mut format,
        &mut track_id,
        &mut codec_params,
        &mut time_base,
        start_ms,
        start_ms > 0,
    )?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| error.to_string())?;
    let mut encoder: Option<OpusEncoderWrapper> = None;
    let mut resampler: Option<LinearResampler> = None;
    let mut channels = 0usize;
    let mut frame_size = 0usize;
    let mut skip = if seek_used {
        seek_skip.unwrap_or(0)
    } else {
        TARGET_SAMPLE_RATE as u64 * start_ms as u64 / 1000
    };
    let mut input_remaining = TARGET_SAMPLE_RATE as u64 * (end_ms - start_ms) as u64 / 1000;
    let mut lookahead = 0usize;
    let mut pcm = Vec::new();
    let mut input_done = input_remaining == 0;
    while !input_done {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                // Allow only integer-ms duration/resampler rounding, never a
                // missing Opus frame disguised as a successful range.
                if input_remaining > TARGET_SAMPLE_RATE as u64 / 1000 {
                    return Err("source ended before requested range was complete".into());
                }
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count();
        if decoded_channels == 0 || decoded_channels > 2 {
            return Err("unsupported channel count".into());
        }
        if encoder.is_none() {
            channels = decoded_channels;
            frame_size = (TARGET_SAMPLE_RATE / 1000 * frame_ms) as usize;
            let bitrate = selector
                .fixed_bitrate_bps
                .unwrap_or_else(|| quality_bitrate(selector.quality));
            let created = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels as u8, bitrate)
                .map_err(|error| error.to_string())?;
            lookahead = created.lookahead().map_err(|error| error.to_string())? as usize;
            let pre_skip =
                u16::try_from(lookahead).map_err(|_| "encoder delay exceeds protocol limit")?;
            send_raw_header_cache(
                &meta,
                TARGET_SAMPLE_RATE,
                channels as u8,
                frame_ms,
                bitrate,
                pre_skip,
                cache,
            )?;
            encoder = Some(created);
        }
        if decoded_channels != channels {
            return Err("channel count changed within range".into());
        }
        let mut samples = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        let output = if spec.rate == TARGET_SAMPLE_RATE {
            samples.samples().to_vec()
        } else {
            resampler
                .get_or_insert_with(|| {
                    LinearResampler::new(spec.rate, TARGET_SAMPLE_RATE, channels as u8)
                })
                .process(samples.samples())
        };
        let frames = output.len() / channels;
        let discard = skip.min(frames as u64) as usize;
        skip -= discard as u64;
        let take = input_remaining.min((frames - discard) as u64) as usize;
        pcm.extend_from_slice(&output[discard * channels..(discard + take) * channels]);
        input_remaining -= take as u64;
        input_done = input_remaining == 0;
        let needed = frame_size * channels;
        while pcm.len() >= needed {
            let packet = encoder
                .as_mut()
                .unwrap()
                .encode(&pcm[..needed], frame_size)
                .map_err(|error| error.to_string())?;
            send_raw_frame_cache(&packet, cache)?;
            pcm.drain(..needed);
        }
    }
    let Some(mut encoder) = encoder else {
        return Err("range has no decodable audio".into());
    };
    // Flush the encoder delay; the client trims final padding to range_end_ms.
    pcm.resize(pcm.len() + lookahead * channels, 0);
    let needed = frame_size * channels;
    while !pcm.is_empty() {
        if pcm.len() < needed {
            pcm.resize(needed, 0);
        }
        let packet = encoder
            .encode(&pcm[..needed], frame_size)
            .map_err(|error| error.to_string())?;
        send_raw_frame_cache(&packet, cache)?;
        pcm.drain(..needed);
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct RawOpusMeta {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub codec: String,
    pub container: String,
}

struct FormatBundle {
    format: Box<dyn FormatReader>,
    track_id: u32,
    track_codec_params: CodecParameters,
    track_time_base: Option<TimeBase>,
}

fn open_format(path: &Path) -> Result<FormatBundle, String> {
    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| err.to_string())?;
    let format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default audio track".to_string())?;
    let track_id = track.id;
    let track_codec_params = track.codec_params.clone();
    let track_time_base = track.codec_params.time_base;
    Ok(FormatBundle {
        format,
        track_id,
        track_codec_params,
        track_time_base,
    })
}

fn apply_seek(
    path: &Path,
    format: &mut Box<dyn FormatReader>,
    track_id: &mut u32,
    track_codec_params: &mut CodecParameters,
    track_time_base: &mut Option<TimeBase>,
    start_ms: u32,
    force_seek: bool,
) -> Result<(bool, Option<u64>), String> {
    if !force_seek && start_ms == 0 {
        return Ok((false, None));
    }
    let time = Time::new((start_ms / 1000) as u64, (start_ms % 1000) as f64 / 1000.0);
    let seek_error = match format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time,
            track_id: Some(*track_id),
        },
    ) {
        Ok(seeked) => {
            match source_seek_skip(*track_time_base, seeked.required_ts, seeked.actual_ts) {
                Ok(skip) => {
                    tracing::info!("QUIC transcode seek start_ms={} actual_ts={} required_ts={} skip_samples_per_channel={}", start_ms, seeked.actual_ts, seeked.required_ts, skip);
                    return Ok((true, Some(skip)));
                }
                Err(error) => error,
            }
        }
        Err(error) => error.to_string(),
    };
    // Broken/missing indexes can fall back for a short initial seek. Never
    // decode an entire long track just to answer one seek request.
    bounded_source_skip(TARGET_SAMPLE_RATE as u64 * start_ms as u64 / 1000)?;
    tracing::warn!(
        "QUIC transcode seek failed (bounded fallback to skip): {}",
        seek_error
    );
    let bundle = open_format(path)?;
    *format = bundle.format;
    *track_id = bundle.track_id;
    *track_codec_params = bundle.track_codec_params;
    *track_time_base = bundle.track_time_base;
    Ok((false, None))
}

fn source_seek_skip(
    time_base: Option<TimeBase>,
    required_ts: u64,
    actual_ts: u64,
) -> Result<u64, String> {
    if actual_ts > required_ts {
        return Err("source seek landed after the requested timestamp".into());
    }
    let time_base = time_base.ok_or("source seek has no timestamp time base")?;
    let required = time_base.calc_time(required_ts);
    let actual = time_base.calc_time(actual_ts);
    let delta_secs =
        (required.seconds as f64 + required.frac) - (actual.seconds as f64 + actual.frac);
    bounded_source_skip((delta_secs.max(0.0) * TARGET_SAMPLE_RATE as f64).round() as u64)
}

fn bounded_source_skip(samples: u64) -> Result<u64, String> {
    if samples > TARGET_SAMPLE_RATE as u64 * MAX_SOURCE_PREROLL_MS as u64 / 1000 {
        return Err("source does not support efficient seeking at this position (preroll exceeds 30 seconds)".into());
    }
    Ok(samples)
}

fn send_raw_header_cache(
    meta: &RawOpusMeta,
    sample_rate: u32,
    channels: u8,
    frame_ms: u32,
    bitrate_bps: u32,
    pre_skip: u16,
    cache: &mut CacheWriter,
) -> Result<(), String> {
    let track_id = meta.track_id.as_bytes();
    let title = meta.title.as_bytes();
    let artist = meta.artist.as_bytes();
    let album = meta.album.as_bytes();
    let codec = meta.codec.as_bytes();
    let container = meta.container.as_bytes();

    let header_len = 8
        + 1
        + 1
        + 2
        + 4
        + 1
        + 1
        + 4
        + 4
        + 2
        + 2 * 6
        + track_id.len()
        + title.len()
        + artist.len()
        + album.len()
        + codec.len()
        + container.len();
    if header_len > u16::MAX as usize {
        return Err("raw opus header too large".to_string());
    }

    let mut buf = Vec::with_capacity(header_len);
    buf.extend_from_slice(b"OPUSR01\0");
    buf.push(1);
    buf.push(0);
    buf.extend_from_slice(&(header_len as u16).to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.push(channels);
    buf.push(frame_ms as u8);
    buf.extend_from_slice(&bitrate_bps.to_le_bytes());
    buf.extend_from_slice(&meta.duration_ms.to_le_bytes());
    buf.extend_from_slice(&pre_skip.to_le_bytes());
    buf.extend_from_slice(&(track_id.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(title.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(artist.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(album.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(codec.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(container.len() as u16).to_le_bytes());
    buf.extend_from_slice(track_id);
    buf.extend_from_slice(title);
    buf.extend_from_slice(artist);
    buf.extend_from_slice(album);
    buf.extend_from_slice(codec);
    buf.extend_from_slice(container);

    cache.write_header(&buf).map_err(|err| err.to_string())?;
    Ok(())
}

fn send_raw_frame_cache(data: &[u8], cache: &mut CacheWriter) -> Result<(), String> {
    let len = data.len();
    if len > u16::MAX as usize {
        return Err("opus frame too large".to_string());
    }
    let mut buf = Vec::with_capacity(2 + len);
    buf.extend_from_slice(&(len as u16).to_le_bytes());
    buf.extend_from_slice(data);
    cache.write_frame(&buf).map_err(|err| err.to_string())?;
    Ok(())
}

fn validate_frame_ms(frame_ms: u32) -> Result<u32, String> {
    match frame_ms {
        2 | 5 | 10 | 20 | 40 | 60 => Ok(frame_ms),
        _ => Err("invalid frame_ms (allowed 2,5,10,20,40,60)".to_string()),
    }
}

pub fn quality_bitrate(quality: TranscodeQuality) -> u32 {
    match quality {
        TranscodeQuality::High => 160_000,
        TranscodeQuality::Medium => 96_000,
        TranscodeQuality::Low => 48_000,
    }
}

struct LinearResampler {
    input_rate: u32,
    output_rate: u32,
    channels: u8,
    buffer: Vec<i16>,
    position: f64,
}

impl LinearResampler {
    fn new(input_rate: u32, output_rate: u32, channels: u8) -> Self {
        Self {
            input_rate,
            output_rate,
            channels,
            buffer: Vec::new(),
            position: 0.0,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        self.buffer.extend_from_slice(input);
        let channels = self.channels as usize;
        let in_frames = self.buffer.len() / channels;
        if in_frames < 2 {
            return Vec::new();
        }

        let step = self.input_rate as f64 / self.output_rate as f64;
        let mut out = Vec::new();

        while self.position + 1.0 < in_frames as f64 {
            let idx = self.position.floor() as usize;
            let frac = self.position - idx as f64;
            for ch in 0..channels {
                let s0 = self.buffer[idx * channels + ch] as f64;
                let s1 = self.buffer[(idx + 1) * channels + ch] as f64;
                let sample = s0 + (s1 - s0) * frac;
                out.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            }
            self.position += step;
        }

        let drop_frames = (self.position.floor() - 1.0).max(0.0) as usize;
        if drop_frames > 0 {
            let drop_samples = drop_frames * channels;
            self.buffer.drain(0..drop_samples);
            self.position -= drop_frames as f64;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_cache::{CacheKey, StreamCache};
    use bytes::Bytes;
    use codecs_ffi::OpusDecoderWrapper;
    use std::path::PathBuf;
    fn decode_cached(reader: crate::stream_cache::CacheReader, position_ms: u32) -> (usize, usize) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        reader.stream_to(position_ms, &tx).unwrap();
        let header = rx.try_recv().unwrap().unwrap();
        let channels = header[16] as usize;
        let skip = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
        let mut decoder = OpusDecoderWrapper::new(48000, channels as u8).unwrap();
        let mut samples = Vec::new();
        let mut eos = 0;
        while let Ok(Ok(packet)) = rx.try_recv() {
            if packet == Bytes::from_static(&[0, 0]) {
                eos += 1;
                continue;
            }
            let length = u16::from_le_bytes(packet[..2].try_into().unwrap()) as usize;
            assert_eq!(length, packet.len() - 2);
            samples.extend(decoder.decode(&packet[2..]).unwrap());
        }
        assert_eq!(eos, 1);
        assert!(samples.iter().any(|sample| sample.abs() > 100));
        (samples.len() / channels - skip, channels)
    }
    #[test]
    fn real_flac_and_mp3_ranges_flush_delay_and_support_cached_sample_exact_seek() {
        for extension in ["flac", "mp3"] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/tone.{extension}"));
            let cache = StreamCache::new(PathBuf::new(), true);
            let key = CacheKey::new(extension, 20, TranscodeMode::Fixed, TranscodeQuality::High)
                .with_source(&path)
                .unwrap();
            let mut writer = cache.writer(key.clone()).unwrap().unwrap();
            let meta = RawOpusMeta {
                track_id: extension.into(),
                title: "tone".into(),
                artist: "fixture".into(),
                album: "tests".into(),
                duration_ms: 400,
                codec: "opus".into(),
                container: "raw".into(),
            };
            transcode_raw_opus_range_cache(
                &path,
                BitrateSelector {
                    quality: TranscodeQuality::High,
                    fixed_bitrate_bps: None,
                },
                20,
                meta,
                0,
                200,
                &mut writer,
            )
            .unwrap();
            writer.finalize().unwrap();
            let (samples, _) = decode_cached(cache.reader(&key).unwrap().unwrap(), 0);
            assert!((9600..10560).contains(&samples), "{extension}: {samples}");
            let (samples, _) = decode_cached(cache.reader(&key).unwrap().unwrap(), 105);
            assert!(
                (4560..5520).contains(&samples),
                "{extension} cached seek: {samples}"
            );
        }
    }
    #[test]
    fn late_source_seek_compensates_a_source_point_more_than_250ms_early() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let cache = StreamCache::new(PathBuf::new(), true);
        let key =
            CacheKey::new("late", 20, TranscodeMode::Fixed, TranscodeQuality::High).with_range(300);
        let mut writer = cache.writer(key.clone()).unwrap().unwrap();
        let meta = RawOpusMeta {
            track_id: "late".into(),
            title: "tone".into(),
            artist: "fixture".into(),
            album: "tests".into(),
            duration_ms: 400,
            codec: "opus".into(),
            container: "raw".into(),
        };
        transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            meta,
            300,
            400,
            &mut writer,
        )
        .unwrap();
        writer.finalize().unwrap();
        let (samples, _) = decode_cached(cache.reader(&key).unwrap().unwrap(), 300);
        assert!((4800..5760).contains(&samples));
    }
    #[test]
    fn source_failure_preserves_an_error_instead_of_emitting_successful_eos() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let cache = StreamCache::new(PathBuf::new(), true);
        let key = CacheKey::new("short", 20, TranscodeMode::Fixed, TranscodeQuality::High);
        let mut writer = cache.writer(key.clone()).unwrap().unwrap();
        let meta = RawOpusMeta {
            track_id: "short".into(),
            title: "tone".into(),
            artist: "fixture".into(),
            album: "tests".into(),
            duration_ms: 5000,
            codec: "opus".into(),
            container: "raw".into(),
        };
        let error = transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            meta,
            0,
            1000,
            &mut writer,
        )
        .unwrap_err();
        assert!(error.contains("source ended"));
        writer.fail(error);
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        assert!(cache
            .reader(&key)
            .unwrap()
            .unwrap()
            .stream_to(0, &tx)
            .is_err());
        while let Ok(Ok(packet)) = rx.try_recv() {
            assert_ne!(packet, Bytes::from_static(&[0, 0]));
        }
    }
    #[test]
    fn ten_milliseconds_missing_at_source_end_is_an_error() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let cache = StreamCache::new(PathBuf::new(), true);
        let mut writer = cache
            .writer(CacheKey::new(
                "missing-tail",
                20,
                TranscodeMode::Fixed,
                TranscodeQuality::High,
            ))
            .unwrap()
            .unwrap();
        let meta = RawOpusMeta {
            track_id: "missing-tail".into(),
            title: "tone".into(),
            artist: "fixture".into(),
            album: "tests".into(),
            duration_ms: 410,
            codec: "opus".into(),
            container: "raw".into(),
        };
        let result = transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            meta,
            0,
            410,
            &mut writer,
        );
        assert!(result.unwrap_err().contains("source ended"));
    }

    #[test]
    fn source_seek_rejects_late_unknown_and_unbounded_positions() {
        let time_base = Some(TimeBase::new(1, TARGET_SAMPLE_RATE));
        assert_eq!(source_seek_skip(time_base, 14400, 0).unwrap(), 14400);
        assert!(source_seek_skip(time_base, 14400, 14401).is_err());
        assert!(source_seek_skip(None, 14400, 0).is_err());
        assert_eq!(bounded_source_skip(30 * 48000).unwrap(), 30 * 48000);
        assert!(bounded_source_skip(30 * 48000 + 1).is_err());
    }
}
