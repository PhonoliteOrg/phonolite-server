use codecs_ffi::OpusEncoderWrapper;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{
    CodecParameters, DecoderOptions, CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_OPUS,
    CODEC_TYPE_VORBIS,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::stream_cache::CacheWriter;

#[path = "transcode/file.rs"]
pub(crate) mod file;
#[path = "transcode/opus.rs"]
mod opus;
#[path = "transcode/resample.rs"]
mod resample;
use resample::SincResampler;

const TARGET_SAMPLE_RATE: u32 = 48_000;
const MAX_SOURCE_PREROLL_MS: u32 = 30_000;
const ENCODER_PREROLL_MS: u32 = 80;

#[cfg(test)]
#[path = "transcode/presentation_tests.rs"]
mod presentation_tests;

pub struct BitrateSelector {
    pub quality: TranscodeQuality,
    pub fixed_bitrate_bps: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TranscodeMode {
    Auto,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
    cache: &mut impl TranscodeSink,
) -> Result<(), String> {
    check_cancelled(cache)?;
    let frame_ms = validate_frame_ms(frame_ms)?;
    if end_ms < start_ms {
        return Err("invalid transcode range".into());
    }
    let bundle = open_format(path)?;
    check_cancelled(cache)?;
    let mut format = bundle.format;
    let mut track_id = bundle.track_id;
    let mut codec_params = bundle.track_codec_params;
    let mut time_base = bundle.track_time_base;
    let start_frame = u64::from(start_ms) * 48;
    let nominal_end = u64::from(end_ms) * 48;
    let source_frames = source_output_frames(&codec_params);
    let final_range = end_ms == meta.duration_ms;
    let legacy_mp3_duration = legacy_mp3_duration_frames(&codec_params)
        .is_some_and(|frames| frames.abs_diff(nominal_end) <= 48);
    let edited_mp4 = matches!(codec_params.codec, CODEC_TYPE_AAC | CODEC_TYPE_ALAC)
        && codec_params.delay.is_some()
        && codec_params.padding.is_some();
    // Correct fractional-ms indexing and legacy MP3 durations that explicitly
    // included trusted codec delay/padding. Other truncation remains an error.
    let exact_end = if edited_mp4 {
        let frames = source_frames.ok_or("MP4 presentation frame count is not representable")?;
        if final_range && frames.abs_diff(nominal_end) <= 48 {
            frames
        } else {
            nominal_end.min(frames)
        }
    } else {
        source_frames
            .filter(|&frames| {
                final_range && (frames.abs_diff(nominal_end) <= 48 || legacy_mp3_duration)
            })
            .unwrap_or(nominal_end)
    };
    let playable_frames = if edited_mp4
        || !final_range
        || source_frames.is_some_and(|n| n.abs_diff(nominal_end) <= 48 || legacy_mp3_duration)
    {
        Some(exact_end.saturating_sub(start_frame))
    } else {
        None
    };
    let encode_start_ms = start_ms.saturating_sub(ENCODER_PREROLL_MS);
    let encode_start_frame = u64::from(encode_start_ms) * 48;
    let encoder_preroll = start_frame - encode_start_frame;
    if playable_frames == Some(0) {
        // A legacy index may include known MP3 padding. Seeking inside that
        // discarded tail is a valid empty final range, not missing source data.
        let channels = if let Some(channels) = codec_params.channels {
            channels.count()
        } else {
            // MP4 AAC may carry its layout only in AudioSpecificConfig.
            let decoder = opus::make_decoder(&codec_params, &DecoderOptions::default())
                .map_err(|error| error.to_string())?;
            decoder.last_decoded().spec().channels.count()
        };
        if !matches!(channels, 1 | 2) {
            return Err("unsupported channel count".into());
        }
        let bitrate = selector
            .fixed_bitrate_bps
            .unwrap_or_else(|| quality_bitrate(selector.quality));
        let mut encoder = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels as u8, bitrate)
            .map_err(|e| e.to_string())?;
        let lookahead = encoder.lookahead().map_err(|e| e.to_string())?;
        send_raw_header_cache(
            &meta,
            TARGET_SAMPLE_RATE,
            channels as u8,
            frame_ms,
            bitrate,
            u16::try_from(lookahead).map_err(|_| "encoder delay exceeds protocol limit")?,
            Some(0),
            cache,
        )?;
        let frame_size = (TARGET_SAMPLE_RATE / 1000 * frame_ms) as usize;
        let silence = vec![0; frame_size * channels];
        for _ in 0..(lookahead as usize).div_ceil(frame_size).max(1) {
            check_cancelled(cache)?;
            let packet = encoder
                .encode(&silence, frame_size)
                .map_err(|e| e.to_string())?;
            send_raw_frame_cache(&packet, cache)?;
        }
        return Ok(());
    }
    let (seek_used, seek_skip) = apply_seek(
        path,
        &mut format,
        &mut track_id,
        &mut codec_params,
        &mut time_base,
        encode_start_ms,
        encode_start_ms > 0,
    )?;
    check_cancelled(cache)?;
    let mut decoder = opus::make_decoder(&codec_params, &DecoderOptions::default())
        .map_err(|error| error.to_string())?;
    let mut encoder: Option<OpusEncoderWrapper> = None;
    let mut resampler: Option<SincResampler> = None;
    let mut source_rate = None;
    let mut channels = 0usize;
    let mut frame_size = 0usize;
    let mut skip = if seek_used {
        seek_skip.unwrap_or(0)
    } else {
        encode_start_frame
    };
    let mut input_remaining = exact_end.saturating_sub(encode_start_frame);
    let mut lookahead = 0usize;
    let mut samples = None;
    let mut pcm = Vec::new();
    let mut input_done = input_remaining == 0;
    while !input_done {
        // Filesystem reads can be synchronous, so stop at each read/decode/
        // encode boundary rather than waiting for the whole range to finish.
        check_cancelled(cache)?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                if let Some(resampler) = resampler.as_mut() {
                    let tail = resampler.finish();
                    append_range_pcm(&tail, channels, &mut skip, &mut input_remaining, &mut pcm);
                    encode_ready(
                        encoder.as_mut().unwrap(),
                        &mut pcm,
                        frame_size,
                        channels,
                        cache,
                    )?;
                }
                // Known exact lengths must be fulfilled completely. A format
                // without a reliable length keeps the legacy sub-ms tolerance.
                if input_remaining > if playable_frames.is_some() { 0 } else { 48 } {
                    return Err(format!("source ended before requested range was complete (missing {input_remaining} frames)"));
                }
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        check_cancelled(cache)?;
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(error)) => {
                let packet_end = timestamp_frames(
                    packet.ts().saturating_add(packet.dur()),
                    time_base,
                    TARGET_SAMPLE_RATE,
                );
                if source_rate.is_none() && packet_end.is_some_and(|end| end <= encode_start_frame)
                {
                    // A seek may start before the compressed decoder has enough
                    // history. Only discard errors wholly before wanted input.
                    continue;
                }
                return Err(format!("source audio decode failed: {error}"));
            }
            Err(error) => return Err(error.to_string()),
        };
        // Symphonia 0.5's AAC/ALAC decoders leave MP4 edit-list trim to
        // their caller. Other supported decoders already trim internally.
        let (trim_start, trim_end) = packet_trim(codec_params.codec, &packet, decoded.frames())?;
        if decoded.frames() == trim_start + trim_end && (trim_start > 0 || trim_end > 0) {
            continue;
        }
        if decoded.frames() == 0 && codec_params.codec == CODEC_TYPE_VORBIS {
            // After a seek, Vorbis consumes one packet to prime its overlap
            // window. Its duration has elapsed even though it emits no PCM.
            if let Some(time_base) = time_base {
                let elapsed = time_base.calc_time(packet.dur());
                let frames = elapsed.seconds * TARGET_SAMPLE_RATE as u64
                    + (elapsed.frac * TARGET_SAMPLE_RATE as f64).round() as u64;
                skip = skip.saturating_sub(frames);
            }
            continue;
        }
        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count();
        if decoded_channels == 0 || decoded_channels > 2 {
            return Err("unsupported channel count".into());
        }
        if let Some(rate) = source_rate {
            if rate != spec.rate {
                return Err("sample rate changed within range".into());
            }
        } else {
            source_rate = Some(spec.rate);
            let origin = timestamp_frames(
                packet.ts().saturating_sub(codec_params.start_ts),
                time_base,
                spec.rate,
            )
            .unwrap_or(0);
            let output_origin = if spec.rate == TARGET_SAMPLE_RATE {
                origin
            } else {
                let sampler = SincResampler::new(
                    spec.rate,
                    TARGET_SAMPLE_RATE,
                    decoded_channels as u8,
                    origin,
                );
                let first = sampler.first_output_frame();
                resampler = Some(sampler);
                first
            };
            if output_origin > encode_start_frame {
                return Err("source seek landed after requested audio".into());
            }
            skip = encode_start_frame - output_origin;
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
            let pre_skip = u16::try_from(lookahead as u64 + encoder_preroll)
                .map_err(|_| "encoder delay exceeds protocol limit")?;
            send_raw_header_cache(
                &meta,
                TARGET_SAMPLE_RATE,
                channels as u8,
                frame_ms,
                bitrate,
                pre_skip,
                playable_frames,
                cache,
            )?;
            encoder = Some(created);
        }
        if decoded_channels != channels {
            return Err("channel count changed within range".into());
        }
        let samples = interleaved_samples(&mut samples, decoded);
        let samples = &samples[trim_start * channels..samples.len() - trim_end * channels];
        if spec.rate == TARGET_SAMPLE_RATE {
            append_range_pcm(samples, channels, &mut skip, &mut input_remaining, &mut pcm);
        } else {
            let output = resampler.as_mut().unwrap().process(samples);
            append_range_pcm(&output, channels, &mut skip, &mut input_remaining, &mut pcm);
        }
        input_done = input_remaining == 0;
        encode_ready(
            encoder.as_mut().unwrap(),
            &mut pcm,
            frame_size,
            channels,
            cache,
        )?;
    }
    let Some(mut encoder) = encoder else {
        return Err("range has no decodable audio".into());
    };
    // Flush encoder delay. New clients use the exact playable frame count;
    // legacy clients retain millisecond range trimming.
    pcm.resize(pcm.len() + lookahead * channels, 0);
    let needed = frame_size * channels;
    pcm.resize(pcm.len().div_ceil(needed) * needed, 0);
    encode_ready(&mut encoder, &mut pcm, frame_size, channels, cache)?;
    Ok(())
}

fn interleaved_samples<'a>(
    buffer: &'a mut Option<SampleBuffer<i16>>,
    decoded: AudioBufferRef<'_>,
) -> &'a [i16] {
    let spec = *decoded.spec();
    let required = decoded.capacity() * spec.channels.count();
    if buffer
        .as_ref()
        .is_none_or(|buffer| buffer.capacity() < required)
    {
        *buffer = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
    }
    // SampleBuffer stores only sample capacity, not a signal specification.
    // Conversion uses each decoded packet's actual layout and written frames.
    let buffer = buffer.as_mut().unwrap();
    buffer.copy_interleaved_ref(decoded);
    buffer.samples()
}

fn packet_trim(
    codec: symphonia::core::codecs::CodecType,
    packet: &symphonia::core::formats::Packet,
    frames: usize,
) -> Result<(usize, usize), String> {
    let trim = if matches!(codec, CODEC_TYPE_AAC | CODEC_TYPE_ALAC) {
        (packet.trim_start() as usize, packet.trim_end() as usize)
    } else {
        (0, 0)
    };
    if trim.0 + trim.1 > frames {
        return Err("source packet trim exceeds decoded audio".into());
    }
    Ok(trim)
}

fn append_range_pcm(
    output: &[i16],
    channels: usize,
    skip: &mut u64,
    remaining: &mut u64,
    pcm: &mut Vec<i16>,
) {
    let frames = output.len() / channels;
    let discard = (*skip).min(frames as u64) as usize;
    *skip -= discard as u64;
    let take = (*remaining).min((frames - discard) as u64) as usize;
    pcm.extend_from_slice(&output[discard * channels..(discard + take) * channels]);
    *remaining -= take as u64;
}

fn encode_ready(
    encoder: &mut OpusEncoderWrapper,
    pcm: &mut Vec<i16>,
    frame_size: usize,
    channels: usize,
    cache: &mut impl TranscodeSink,
) -> Result<(), String> {
    let needed = frame_size * channels;
    let mut consumed = 0;
    while pcm.len() - consumed >= needed {
        check_cancelled(cache)?;
        let packet = encoder
            .encode(&pcm[consumed..consumed + needed], frame_size)
            .map_err(|error| error.to_string())?;
        send_raw_frame_cache(&packet, cache)?;
        consumed += needed;
    }
    if consumed != 0 {
        // Move the partial frame once per decoded batch, not after every packet.
        pcm.copy_within(consumed.., 0);
        pcm.truncate(pcm.len() - consumed);
    }
    Ok(())
}

fn timestamp_frames(timestamp: u64, time_base: Option<TimeBase>, rate: u32) -> Option<u64> {
    let base = time_base?;
    let numerator = u128::from(timestamp)
        .checked_mul(u128::from(base.numer))?
        .checked_mul(u128::from(rate))?;
    u64::try_from((numerator + u128::from(base.denom / 2)) / u128::from(base.denom)).ok()
}

fn source_output_frames(params: &CodecParameters) -> Option<u64> {
    // A bare MP3/VBR duration estimate is not a sample-exact boundary.
    if params.codec == symphonia::core::codecs::CODEC_TYPE_MP3
        && (params.delay.is_none() || params.padding.is_none())
    {
        return None;
    }
    timestamp_frames(
        params.n_frames?,
        params
            .time_base
            .or_else(|| params.sample_rate.map(|rate| TimeBase::new(1, rate))),
        TARGET_SAMPLE_RATE,
    )
}

fn legacy_mp3_duration_frames(params: &CodecParameters) -> Option<u64> {
    if params.codec != symphonia::core::codecs::CODEC_TYPE_MP3 {
        return None;
    }
    let frames = params
        .n_frames?
        .checked_add(u64::from(params.delay?))?
        .checked_add(u64::from(params.padding?))?;
    timestamp_frames(
        frames,
        params
            .time_base
            .or_else(|| params.sample_rate.map(|rate| TimeBase::new(1, rate))),
        TARGET_SAMPLE_RATE,
    )
}

pub(crate) trait TranscodeSink {
    fn check_cancelled(&self) -> std::io::Result<()>;
    fn write_header(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn write_frame(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

impl TranscodeSink for CacheWriter {
    fn check_cancelled(&self) -> std::io::Result<()> {
        CacheWriter::check_cancelled(self)
    }
    fn write_header(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        CacheWriter::write_header(self, bytes)
    }
    fn write_frame(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        CacheWriter::write_frame(self, bytes)
    }
}

fn check_cancelled(cache: &impl TranscodeSink) -> Result<(), String> {
    cache.check_cancelled().map_err(|error| error.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
            &FormatOptions {
                enable_gapless: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|err| err.to_string())?;
    let format = probed.format;
    // MP4 may list video before its audio track. Select a supported audio codec.
    let track = format
        .tracks()
        .iter()
        .find(|track| {
            track.codec_params.codec == CODEC_TYPE_OPUS
                || symphonia::default::get_codecs()
                    .get_codec(track.codec_params.codec)
                    .is_some()
        })
        .ok_or_else(|| "no supported audio track".to_string())?;
    let track_id = track.id;
    let mut track_codec_params = track.codec_params.clone();
    if let Some(frames) = aiff_frame_count(path)? {
        // Symphonia 0.5's AIFF data-length fallback includes the eight SSND
        // control bytes. COMM carries the actual frame count.
        track_codec_params.n_frames = Some(frames);
    }
    let track_time_base = track.codec_params.time_base;
    Ok(FormatBundle {
        format,
        track_id,
        track_codec_params,
        track_time_base,
    })
}

fn aiff_frame_count(path: &Path) -> Result<Option<u64>, String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let mut form = [0; 12];
    if file.read_exact(&mut form).is_err()
        || &form[..4] != b"FORM"
        || !matches!(&form[8..], b"AIFF" | b"AIFC")
    {
        return Ok(None);
    }
    let mut position = 12u64;
    // Read fixed-size chunk headers only. Tags/artwork cannot force allocations
    // or an unbounded scan before playback begins.
    for _ in 0..256 {
        if position + 8 > length {
            break;
        }
        file.seek(SeekFrom::Start(position))
            .map_err(|error| error.to_string())?;
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)
            .map_err(|error| error.to_string())?;
        let size = u64::from(u32::from_be_bytes(chunk[4..8].try_into().unwrap()));
        if size > length - position - 8 {
            return Err("truncated AIFF chunk".into());
        }
        if &chunk[..4] == b"COMM" && size >= 18 {
            let mut common = [0; 6];
            file.read_exact(&mut common)
                .map_err(|error| error.to_string())?;
            return Ok(Some(u64::from(u32::from_be_bytes(
                common[2..6].try_into().unwrap(),
            ))));
        }
        position += 8 + size + (size & 1);
    }
    Ok(None)
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
    let seek_ms = start_ms.saturating_sub(source_preroll_ms(track_codec_params));
    let time = Time::new((seek_ms / 1000) as u64, (seek_ms % 1000) as f64 / 1000.0);
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
                    let skip =
                        skip + TARGET_SAMPLE_RATE as u64 * (start_ms - seek_ms) as u64 / 1000;
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

fn source_preroll_ms(params: &CodecParameters) -> u32 {
    if matches!(
        params.codec,
        symphonia::core::codecs::CODEC_TYPE_MP3 | CODEC_TYPE_AAC
    ) {
        // Restore compressed history and AAC overlap before the encoder's
        // own priming interval. Seeking straight to a frame can decode silence.
        1000
    } else if params.codec == CODEC_TYPE_OPUS {
        // A fresh Opus decoder needs 80 ms of history.
        80
    } else if params.codec == CODEC_TYPE_VORBIS {
        // Prime at least one full Vorbis window. The identification header
        // stores the long-window exponent in the upper half of byte 28.
        let exponent = params
            .extra_data
            .as_deref()
            .filter(|header| header.starts_with(b"\x01vorbis"))
            .and_then(|header| header.get(28))
            .map(|sizes| sizes >> 4)
            .filter(|exponent| (6..=13).contains(exponent))
            .unwrap_or(13);
        let rate = params.sample_rate.unwrap_or(TARGET_SAMPLE_RATE).max(1);
        ((1u64 << exponent) * 1000).div_ceil(rate as u64) as u32
    } else {
        0
    }
}

fn source_seek_skip(
    time_base: Option<TimeBase>,
    required_ts: u64,
    actual_ts: u64,
) -> Result<u64, String> {
    if actual_ts > required_ts {
        return Err("source seek landed after the requested timestamp".into());
    }
    let frames = timestamp_frames(required_ts - actual_ts, time_base, TARGET_SAMPLE_RATE)
        .ok_or("source seek has no timestamp time base")?;
    bounded_source_skip(frames)
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
    playable_frames: Option<u64>,
    cache: &mut impl TranscodeSink,
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
        + container.len()
        + if playable_frames.is_some() { 8 } else { 0 };
    if header_len > u16::MAX as usize {
        return Err("raw opus header too large".to_string());
    }

    let mut buf = Vec::with_capacity(header_len);
    buf.extend_from_slice(b"OPUSR01\0");
    buf.push(1);
    // Optional trailing extension, backwards compatible with length-framed R01.
    buf.push(u8::from(playable_frames.is_some()));
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
    if let Some(frames) = playable_frames {
        buf.extend_from_slice(&frames.to_le_bytes());
    }

    cache.write_header(&buf).map_err(|err| err.to_string())?;
    Ok(())
}

fn send_raw_frame_cache(data: &[u8], cache: &mut impl TranscodeSink) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_cache::{CacheKey, StreamCache};
    use bytes::Bytes;
    use codecs_ffi::OpusDecoderWrapper;
    use std::path::PathBuf;
    use symphonia::core::audio::{AsAudioBufferRef, AudioBuffer, Channels, Signal, SignalSpec};

    #[test]
    fn interleaved_conversion_reuses_capacity_and_keeps_only_current_packet_samples() {
        let mut reused = None;
        for (capacity, frames, channels, rate) in [
            (128, 100, 2, 48000),
            (64, 17, 2, 48000),
            (32, 20, 1, 44100),
            (256, 219, 2, 48000),
            (128, 0, 1, 48000),
            (256, 256, 2, 48000),
        ] {
            let layout = if channels == 1 {
                Channels::FRONT_LEFT
            } else {
                Channels::FRONT_LEFT | Channels::FRONT_RIGHT
            };
            let spec = SignalSpec::new(rate, layout);
            let mut decoded = AudioBuffer::<f32>::new(capacity, spec);
            decoded.render_reserved(Some(frames));
            for channel in 0..channels {
                for (index, sample) in decoded.chan_mut(channel).iter_mut().enumerate() {
                    *sample = ((index * 7 + channel * 13) as f32 * 0.13).sin() * 0.8;
                }
            }
            let mut fresh = SampleBuffer::<i16>::new(capacity, spec);
            fresh.copy_interleaved_ref(decoded.as_audio_buffer_ref());
            let previous = reused
                .as_ref()
                .map(|buffer: &SampleBuffer<i16>| (buffer.capacity(), buffer.samples().as_ptr()));
            let actual = interleaved_samples(&mut reused, decoded.as_audio_buffer_ref());
            assert_eq!(actual, fresh.samples());
            assert_eq!(actual.len(), frames * channels);
            if let Some((previous_capacity, previous_address)) = previous {
                if capacity as usize * channels <= previous_capacity {
                    assert_eq!(actual.as_ptr(), previous_address);
                }
            }
        }
    }

    #[derive(Default)]
    struct PacketSink(Vec<Vec<u8>>);

    impl TranscodeSink for PacketSink {
        fn check_cancelled(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn write_header(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
            unreachable!("frame-only encoding does not write a header")
        }

        fn write_frame(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn batched_pcm_consumption_preserves_packets_and_partial_frames() {
        for channels in [1, 2] {
            for frame_ms in [5, 10, 20, 40, 60] {
                let frame_size = 48 * frame_ms;
                let needed = frame_size * channels;
                let mut encoder = OpusEncoderWrapper::new(48000, channels as u8, 160000).unwrap();
                let mut reference_encoder =
                    OpusEncoderWrapper::new(48000, channels as u8, 160000).unwrap();
                let mut pcm = Vec::new();
                let mut reference_pcm = Vec::new();
                let mut actual = PacketSink::default();
                let mut expected = PacketSink::default();
                let mut position = 0usize;
                for chunk_frames in [1, 17, 2893, 127, 4320, 73, 6201] {
                    let samples: Vec<i16> = (position..position + chunk_frames * channels)
                        .map(|index| ((index as f64 * 0.073).sin() * 20000.0) as i16)
                        .collect();
                    position += samples.len();
                    pcm.extend_from_slice(&samples);
                    reference_pcm.extend_from_slice(&samples);
                    while reference_pcm.len() >= needed {
                        let packet = reference_encoder
                            .encode(&reference_pcm[..needed], frame_size)
                            .unwrap();
                        send_raw_frame_cache(&packet, &mut expected).unwrap();
                        reference_pcm.drain(..needed);
                    }
                    let allocation = pcm.as_ptr();
                    let capacity = pcm.capacity();
                    encode_ready(&mut encoder, &mut pcm, frame_size, channels, &mut actual)
                        .unwrap();
                    assert_eq!(pcm, reference_pcm);
                    assert_eq!(actual.0, expected.0);
                    assert_eq!(pcm.as_ptr(), allocation);
                    assert_eq!(pcm.capacity(), capacity);
                }
                let lookahead = encoder.lookahead().unwrap() as usize;
                pcm.resize(pcm.len() + lookahead * channels, 0);
                pcm.resize(pcm.len().div_ceil(needed) * needed, 0);
                encode_ready(&mut encoder, &mut pcm, frame_size, channels, &mut actual).unwrap();
                reference_pcm.resize(reference_pcm.len() + lookahead * channels, 0);
                while !reference_pcm.is_empty() {
                    if reference_pcm.len() < needed {
                        reference_pcm.resize(needed, 0);
                    }
                    let packet = reference_encoder
                        .encode(&reference_pcm[..needed], frame_size)
                        .unwrap();
                    send_raw_frame_cache(&packet, &mut expected).unwrap();
                    reference_pcm.drain(..needed);
                }
                assert!(pcm.is_empty());
                assert_eq!(actual.0, expected.0);
            }
        }
    }

    #[test]
    fn exact_header_preserves_fractional_millisecond_tail_and_cached_seek_length() {
        let frames = 19_207u32;
        let path =
            std::env::temp_dir().join(format!("phonolite-gapless-{}.wav", uuid::Uuid::new_v4()));
        let mut wave = Vec::new();
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16u32.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&48000u32.to_le_bytes());
        wave.extend_from_slice(&96000u32.to_le_bytes());
        wave.extend_from_slice(&2u16.to_le_bytes());
        wave.extend_from_slice(&16u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&(frames * 2).to_le_bytes());
        for frame in 0..frames {
            let sample = ((f64::from(frame) * 0.0576).sin() * 10000.0) as i16;
            wave.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(&path, wave).unwrap();
        let cache = StreamCache::new(PathBuf::new(), true);
        let key = CacheKey::new(
            "exact-tail",
            20,
            TranscodeMode::Fixed,
            TranscodeQuality::High,
        );
        let mut writer = cache.writer(key.clone()).unwrap().unwrap();
        let result = transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            RawOpusMeta {
                track_id: "exact-tail".into(),
                title: "fractional".into(),
                artist: String::new(),
                album: String::new(),
                duration_ms: 400,
                codec: "pcm".into(),
                container: "wav".into(),
            },
            0,
            400,
            &mut writer,
        );
        std::fs::remove_file(&path).unwrap();
        result.unwrap();
        writer.finalize().unwrap();
        for (position, expected) in [(0, u64::from(frames)), (105, u64::from(frames) - 5040)] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(128);
            cache
                .reader(&key)
                .unwrap()
                .unwrap()
                .stream_to(position, &tx)
                .unwrap();
            let header = rx.try_recv().unwrap().unwrap();
            assert_eq!(header[8], 1, "legacy R01 version is retained");
            assert_eq!(header[9] & 1, 1);
            assert_eq!(
                u16::from_le_bytes(header[10..12].try_into().unwrap()) as usize,
                header.len()
            );
            let audible = u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap());
            assert_eq!(audible, expected);
            let skip = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
            let mut decoder = OpusDecoderWrapper::new(48000, 1).unwrap();
            let mut decoded = Vec::new();
            let mut eos = 0;
            while let Ok(Ok(packet)) = rx.try_recv() {
                if packet.as_ref() == [0, 0] {
                    eos += 1;
                    continue;
                }
                decoded.extend(decoder.decode(&packet[2..]).unwrap());
            }
            assert_eq!(eos, 1);
            assert!(decoded.len() >= skip + audible as usize);
            assert!(decoded[skip..skip + audible as usize]
                .iter()
                .any(|n| n.abs() > 100));
        }
    }

    #[test]
    fn source_frame_arithmetic_handles_long_streams_and_unknown_mp3_trimming() {
        assert_eq!(
            timestamp_frames(
                u64::from(192000u32) * 86400,
                Some(TimeBase::new(1, 192000)),
                48000
            ),
            Some(48000u64 * 86400)
        );
        let mut params = CodecParameters::new();
        params
            .for_codec(symphonia::core::codecs::CODEC_TYPE_MP3)
            .with_sample_rate(44100)
            .with_n_frames(17640)
            .with_time_base(TimeBase::new(1, 44100));
        assert_eq!(source_output_frames(&params), None);
        params.with_delay(576).with_padding(1000);
        assert_eq!(source_output_frames(&params), Some(19200));
    }

    #[test]
    fn legacy_mp3_index_duration_removes_known_padding_and_tail_seeks_are_empty() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.mp3");
        let params = open_format(&path).unwrap().track_codec_params;
        let actual_frames = source_output_frames(&params).unwrap();
        let raw_frames = legacy_mp3_duration_frames(&params).unwrap();
        assert!(raw_frames > actual_frames + 48);
        let raw_duration_ms = (raw_frames * 1000 / 48000) as u32;
        let padding_seek_ms = (actual_frames * 1000 / 48000) as u32 + 1;
        let cache = StreamCache::new(PathBuf::new(), true);
        let key = CacheKey::new(
            "legacy-mp3",
            20,
            TranscodeMode::Fixed,
            TranscodeQuality::High,
        );
        let mut writer = cache.writer(key.clone()).unwrap().unwrap();
        let meta = RawOpusMeta {
            track_id: "legacy-mp3".into(),
            title: "legacy".into(),
            artist: String::new(),
            album: String::new(),
            duration_ms: raw_duration_ms,
            codec: "mp3".into(),
            container: "mp3".into(),
        };
        transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            meta.clone(),
            0,
            raw_duration_ms,
            &mut writer,
        )
        .unwrap();
        writer.finalize().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        cache
            .reader(&key)
            .unwrap()
            .unwrap()
            .stream_to(0, &tx)
            .unwrap();
        let header = rx.try_recv().unwrap().unwrap();
        assert_eq!(header[9] & 1, 1);
        assert_eq!(
            u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap()),
            actual_frames
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        cache
            .reader(&key)
            .unwrap()
            .unwrap()
            .stream_to(padding_seek_ms, &tx)
            .unwrap();
        let header = rx.try_recv().unwrap().unwrap();
        assert_eq!(
            u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap()),
            0
        );
        assert_eq!(u16::from_le_bytes(header[26..28].try_into().unwrap()), 0);
        assert_eq!(rx.try_recv().unwrap().unwrap().as_ref(), [0, 0]);
        assert!(rx.try_recv().is_err());
        let tail_key = key.with_range(padding_seek_ms);
        let mut writer = cache.writer(tail_key.clone()).unwrap().unwrap();
        transcode_raw_opus_range_cache(
            &path,
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            meta,
            padding_seek_ms,
            raw_duration_ms,
            &mut writer,
        )
        .unwrap();
        writer.finalize().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        cache
            .reader(&tail_key)
            .unwrap()
            .unwrap()
            .stream_to(padding_seek_ms, &tx)
            .unwrap();
        let header = rx.try_recv().unwrap().unwrap();
        assert_eq!(
            u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap()),
            0
        );
        assert_eq!(rx.try_recv().unwrap().unwrap().as_ref(), [0, 0]);
    }

    #[test]
    fn shutdown_stops_transcoding_before_opening_the_source() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let cache =
            StreamCache::new(PathBuf::new(), true).with_shutdown_flag(Arc::clone(&shutdown));
        let mut writer = cache
            .writer(CacheKey::new(
                "cancelled",
                20,
                TranscodeMode::Fixed,
                TranscodeQuality::High,
            ))
            .unwrap()
            .unwrap();
        shutdown.store(true, Ordering::Release);
        let error = transcode_raw_opus_range_cache(
            Path::new("missing-cancelled-source.flac"),
            BitrateSelector {
                quality: TranscodeQuality::High,
                fixed_bitrate_bps: None,
            },
            20,
            RawOpusMeta {
                track_id: "cancelled".into(),
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                duration_ms: 5000,
                codec: "flac".into(),
                container: "flac".into(),
            },
            0,
            5000,
            &mut writer,
        )
        .unwrap_err();
        assert_eq!(error, "server shutting down");
    }

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
            let key = key.with_range(300);
            let mut writer = cache.writer(key.clone()).unwrap().unwrap();
            transcode_raw_opus_range_cache(
                &path,
                BitrateSelector {
                    quality: TranscodeQuality::High,
                    fixed_bitrate_bps: None,
                },
                20,
                RawOpusMeta {
                    track_id: extension.into(),
                    title: "late".into(),
                    artist: String::new(),
                    album: String::new(),
                    duration_ms: 400,
                    codec: "opus".into(),
                    container: "raw".into(),
                },
                300,
                400,
                &mut writer,
            )
            .unwrap();
            writer.finalize().unwrap();
            let (samples, _) = decode_cached(cache.reader(&key).unwrap().unwrap(), 300);
            assert!(
                (4800..5760).contains(&samples),
                "{extension} late range: {samples}"
            );
        }
    }

    #[test]
    fn requested_audio_formats_decode_complete_tracks_and_seek_ranges() {
        for fixture in [
            "aac.m4a",
            "aac.mp4",
            "tone.aac",
            "alac.m4a",
            "alac.mp4",
            "opus.opus",
            "opus.ogg",
            "opus.oga",
            "vorbis.ogg",
            "vorbis.oga",
            "tone.wav",
            "tone.aif",
            "tone.aiff",
            "long-opus.oga",
            "long-vorbis.ogg",
        ] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/formats")
                .join(fixture);
            let duration_ms = if fixture.starts_with("long-") {
                1400
            } else {
                400
            };
            for start_ms in [0, duration_ms - 100] {
                let cache = StreamCache::new(PathBuf::new(), true);
                let key = CacheKey::new(fixture, 20, TranscodeMode::Fixed, TranscodeQuality::High)
                    .with_source(&path)
                    .unwrap()
                    .with_range(start_ms);
                let mut writer = cache.writer(key.clone()).unwrap().unwrap();
                let meta = RawOpusMeta {
                    track_id: fixture.into(),
                    title: "tone".into(),
                    artist: "fixture".into(),
                    album: "tests".into(),
                    duration_ms,
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
                    start_ms,
                    duration_ms,
                    &mut writer,
                )
                .unwrap_or_else(|error| panic!("{fixture} at {start_ms}ms: {error}"));
                writer.finalize().unwrap();
                let (samples, _) = decode_cached(cache.reader(&key).unwrap().unwrap(), start_ms);
                let expected = ((duration_ms - start_ms) * 48) as usize;
                assert!(
                    (expected..expected + 960).contains(&samples),
                    "{fixture} at {start_ms}ms: {samples} samples, expected {expected}"
                );
            }
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
