use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use bytes::Bytes;
use codecs_ffi::OpusEncoderWrapper;
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
const DEFAULT_FRAME_MS: u32 = 20;
const MAX_SEEK_SKIP_MS: u32 = 250;

pub struct BitrateSelector {
    pub mode: TranscodeMode,
    pub quality: TranscodeQuality,
    pub fixed_bitrate_bps: Option<u32>,
    pub adaptive_bitrate_bps: Option<Arc<AtomicU32>>,
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

pub fn transcode_to_ogg_opus(
    path: &Path,
    selector: BitrateSelector,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    transcode_to_opus(
        path,
        selector,
        DEFAULT_FRAME_MS,
        OpusOutput::Ogg(tx),
        0,
        None,
    )
}

pub fn transcode_to_raw_opus(
    path: &Path,
    selector: BitrateSelector,
    frame_ms: u32,
    meta: RawOpusMeta,
    start_ms: u32,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    cache: Option<CacheWriter>,
) -> Result<(), String> {
    transcode_to_opus(
        path,
        selector,
        frame_ms,
        OpusOutput::Raw(tx, meta),
        start_ms,
        cache,
    )
}

pub fn transcode_to_raw_opus_cache(
    path: &Path,
    selector: BitrateSelector,
    frame_ms: u32,
    meta: RawOpusMeta,
    mut cache: CacheWriter,
) -> Result<(), String> {
    let frame_ms = validate_frame_ms(frame_ms)?;
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
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default audio track".to_string())?;
    let track_codec_params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track_codec_params, &DecoderOptions::default())
        .map_err(|err| err.to_string())?;

    let mut channels: Option<u8> = None;
    let mut resampler: Option<LinearResampler> = None;
    let mut pcm_buffer: Vec<i16> = Vec::new();
    let mut pcm_offset: usize = 0;
    let mut encoder: Option<OpusEncoderWrapper> = None;
    let mut current_bitrate = 0u32;
    let mut frame_size = 0usize;

    loop {
        if cache.is_cancelled() {
            return Ok(());
        }
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                return Err("decoder reset required".to_string());
            }
            Err(SymphoniaError::IoError(_)) => break,
            Err(err) => return Err(err.to_string()),
        };
        let decoded = decoder.decode(&packet).map_err(|err| err.to_string())?;

        if cache.is_cancelled() {
            return Ok(());
        }

        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count() as u8;
        if decoded_channels == 0 || decoded_channels > 2 {
            return Err("unsupported channel count".to_string());
        }
        if channels.is_none() {
            channels = Some(decoded_channels);
        }
        let channels = channels.unwrap();

        if encoder.is_none() {
            let bitrate = match selector.mode {
                TranscodeMode::Fixed => selector
                    .fixed_bitrate_bps
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
                TranscodeMode::Auto => selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
            };
            let created = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels, bitrate)
                .map_err(|err| err.to_string())?;
            send_raw_header_cache(
                &meta,
                created.sample_rate(),
                created.channels(),
                frame_ms,
                bitrate,
                &mut cache,
            )?;
            frame_size = (created.sample_rate() / 1000 * frame_ms) as usize;
            current_bitrate = bitrate;
            encoder = Some(created);
        }

        let mut sample_buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();

        let input_rate = spec.rate;
        let output_samples = if input_rate == TARGET_SAMPLE_RATE {
            samples.to_vec()
        } else {
            if resampler.is_none() {
                resampler = Some(LinearResampler::new(
                    input_rate,
                    TARGET_SAMPLE_RATE,
                    channels,
                ));
            }
            resampler
                .as_mut()
                .ok_or_else(|| "resampler missing".to_string())?
                .process(samples)
        };

        pcm_buffer.extend_from_slice(&output_samples);

        while pcm_buffer.len().saturating_sub(pcm_offset) >= frame_size * channels as usize {
            if selector.mode == TranscodeMode::Auto {
                let desired = selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality));
                if desired != current_bitrate {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(desired).map_err(|err| err.to_string())?;
                    }
                    current_bitrate = desired;
                }
            }

            let end = pcm_offset + frame_size * channels as usize;
            let frame = &pcm_buffer[pcm_offset..end];
            let encoded = encoder
                .as_mut()
                .ok_or_else(|| "encoder not initialized".to_string())?
                .encode(frame, frame_size)
                .map_err(|err| err.to_string())?;
            send_raw_frame_cache(&encoded, &mut cache)?;
            pcm_offset = end;
        }

        if pcm_offset > 0 {
            pcm_buffer.drain(..pcm_offset);
            pcm_offset = 0;
        }
    }

    cache.finalize().map_err(|err| err.to_string())?;
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

pub enum TranscodeCommand {
    Seek {
        start_ms: u32,
        tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    },
    Stop,
}

pub fn spawn_raw_opus_worker(
    path: PathBuf,
    selector: BitrateSelector,
    frame_ms: u32,
    meta: RawOpusMeta,
    start_ms: u32,
    cache: Option<CacheWriter>,
) -> Result<
    (
        tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
        std_mpsc::Sender<TranscodeCommand>,
    ),
    String,
> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let (cmd_tx, cmd_rx) = std_mpsc::channel::<TranscodeCommand>();
    let tx_err = tx.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(err) =
            transcode_raw_worker_loop(path, selector, frame_ms, meta, start_ms, tx, cmd_rx, cache)
        {
            if err == "stream closed" {
                tracing::debug!("QUIC transcode worker stopped: {}", err);
            } else {
                send_worker_error(&tx_err, &err);
                tracing::warn!("QUIC transcode worker failed: {}", err);
            }
        }
    });
    Ok((rx, cmd_tx))
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
    let mut format = probed.format;
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

enum OpusOutput<'a> {
    Ogg(&'a tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>),
    Raw(
        &'a tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
        RawOpusMeta,
    ),
}

fn transcode_to_opus(
    path: &Path,
    selector: BitrateSelector,
    frame_ms: u32,
    output: OpusOutput<'_>,
    start_ms: u32,
    mut cache: Option<CacheWriter>,
) -> Result<(), String> {
    let frame_ms = validate_frame_ms(frame_ms)?;
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
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default audio track".to_string())?;
    let track_id = track.id;
    let track_codec_params = track.codec_params.clone();
    let track_time_base = track.codec_params.time_base;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track_codec_params, &DecoderOptions::default())
        .map_err(|err| err.to_string())?;

    let mut channels: Option<u8> = None;
    let mut resampler: Option<LinearResampler> = None;
    let mut pcm_buffer: Vec<i16> = Vec::new();
    let mut pcm_offset: usize = 0;
    let mut total_samples = 0u64;
    let mut ogg = OggWriter::new(serial_from_path(path));
    let mut encoder: Option<OpusEncoderWrapper> = None;
    let mut current_bitrate = 0u32;
    let mut frame_size = 0usize;
    let mut packet_counter = 0u32;
    let mut skip_samples: u64 = 0;
    let mut skip_initialized = false;
    let mut seek_used = false;
    let mut seek_skip_per_channel: Option<u64> = None;

    if start_ms > 0 {
        let seconds = (start_ms / 1000) as u64;
        let frac = (start_ms % 1000) as f64 / 1000.0;
        let time = Time::new(seconds, frac);
        let seek_mode = SeekMode::Coarse;
        match format.seek(
            seek_mode,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        ) {
            Ok(seeked) => {
                seek_used = true;
                if let Some(time_base) = track_time_base {
                    let required = time_base.calc_time(seeked.required_ts);
                    let actual = time_base.calc_time(seeked.actual_ts);
                    let required_secs = required.seconds as f64 + required.frac;
                    let actual_secs = actual.seconds as f64 + actual.frac;
                    let delta_secs = required_secs - actual_secs;
                    if delta_secs > 0.0 {
                        let skip = (delta_secs * TARGET_SAMPLE_RATE as f64).round() as u64;
                        let max_skip = (TARGET_SAMPLE_RATE as u64)
                            .saturating_mul(MAX_SEEK_SKIP_MS as u64)
                            / 1000;
                        if skip > max_skip {
                            tracing::info!(
                                "QUIC transcode seek skip capped: requested_skip={} max_skip={} (ms={})",
                                skip,
                                max_skip,
                                MAX_SEEK_SKIP_MS
                            );
                            seek_skip_per_channel = None;
                        } else {
                            seek_skip_per_channel = Some(skip);
                        }
                    }
                }
                decoder = symphonia::default::get_codecs()
                    .make(&track_codec_params, &DecoderOptions::default())
                    .map_err(|err| err.to_string())?;
                resampler = None;
                pcm_buffer.clear();
                tracing::info!(
                    "QUIC transcode seek start_ms={} actual_ts={} required_ts={} skip_samples_per_channel={}",
                    start_ms,
                    seeked.actual_ts,
                    seeked.required_ts,
                    seek_skip_per_channel.unwrap_or(0)
                );
            }
            Err(err) => {
                tracing::warn!("QUIC transcode seek failed (fallback to skip): {}", err);
            }
        }
    }

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                return Err("decoder reset required".to_string());
            }
            Err(SymphoniaError::IoError(_)) => break,
            Err(err) => return Err(err.to_string()),
        };
        let decoded = decoder.decode(&packet).map_err(|err| err.to_string())?;

        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count() as u8;
        if decoded_channels == 0 || decoded_channels > 2 {
            return Err("unsupported channel count".to_string());
        }
        if channels.is_none() {
            channels = Some(decoded_channels);
        }
        let channels = channels.unwrap();
        if !skip_initialized && start_ms > 0 {
            let per_channel = if seek_used {
                seek_skip_per_channel.unwrap_or(0)
            } else {
                (TARGET_SAMPLE_RATE as u64).saturating_mul(start_ms as u64) / 1000
            };
            if per_channel > 0 {
                skip_samples = per_channel.saturating_mul(channels as u64);
            }
            skip_initialized = true;
        }

        if encoder.is_none() {
            let bitrate = match selector.mode {
                TranscodeMode::Fixed => selector
                    .fixed_bitrate_bps
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
                TranscodeMode::Auto => selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
            };
            let created = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels, bitrate)
                .map_err(|err| err.to_string())?;
            match &output {
                OpusOutput::Ogg(tx) => {
                    send_headers(&mut ogg, created.channels(), created.sample_rate(), tx)?;
                }
                OpusOutput::Raw(tx, meta) => {
                    send_raw_header(
                        meta,
                        created.sample_rate(),
                        created.channels(),
                        frame_ms,
                        bitrate,
                        tx,
                        cache.as_mut(),
                    )?;
                }
            }
            frame_size = (created.sample_rate() / 1000 * frame_ms) as usize;
            current_bitrate = bitrate;
            encoder = Some(created);
        }

        let mut sample_buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();

        let input_rate = spec.rate;
        let output_samples = if input_rate == TARGET_SAMPLE_RATE {
            samples.to_vec()
        } else {
            if resampler.is_none() {
                resampler = Some(LinearResampler::new(
                    input_rate,
                    TARGET_SAMPLE_RATE,
                    channels,
                ));
            }
            resampler
                .as_mut()
                .ok_or_else(|| "resampler missing".to_string())?
                .process(samples)
        };

        pcm_buffer.extend_from_slice(&output_samples);
        if skip_samples > 0 {
            let available = pcm_buffer.len().saturating_sub(pcm_offset);
            let drop = std::cmp::min(skip_samples as usize, available);
            if drop > 0 {
                pcm_offset = pcm_offset.saturating_add(drop);
                skip_samples = skip_samples.saturating_sub(drop as u64);
            }
            if skip_samples > 0 {
                if pcm_offset > 0 {
                    pcm_buffer.drain(..pcm_offset);
                    pcm_offset = 0;
                }
                continue;
            }
        }

        while pcm_buffer.len().saturating_sub(pcm_offset) >= frame_size * channels as usize {
            if selector.mode == TranscodeMode::Auto {
                let desired = selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality));
                if desired != current_bitrate {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(desired).map_err(|err| err.to_string())?;
                    }
                    current_bitrate = desired;
                }
            }

            let end = pcm_offset + frame_size * channels as usize;
            let frame = &pcm_buffer[pcm_offset..end];
            let encoded = encoder
                .as_mut()
                .ok_or_else(|| "encoder not initialized".to_string())?
                .encode(frame, frame_size)
                .map_err(|err| err.to_string())?;
            total_samples += frame_size as u64;
            match &output {
                OpusOutput::Ogg(tx) => {
                    let pages = ogg.write_packet(&encoded, total_samples, false, false);
                    send_pages(&pages, tx)?;
                }
                OpusOutput::Raw(tx, _) => {
                    send_raw_frame(&encoded, tx, cache.as_mut())?;
                }
            }
            pcm_offset = end;
            packet_counter = packet_counter.wrapping_add(1);
        }

        if pcm_offset > 0 {
            pcm_buffer.drain(..pcm_offset);
            pcm_offset = 0;
        }
    }

    if encoder.is_none() {
        return Err("encoder not initialized".to_string());
    }

    if skip_samples > 0 && !pcm_buffer.is_empty() {
        let drop = std::cmp::min(skip_samples as usize, pcm_buffer.len());
        if drop > 0 {
            pcm_buffer.drain(..drop);
        }
    }

    if !pcm_buffer.is_empty() {
        let channels = encoder.as_ref().unwrap().channels() as usize;
        let needed = frame_size * channels;
        if pcm_buffer.len() < needed {
            pcm_buffer.resize(needed, 0);
        }
        let encoded = encoder
            .as_mut()
            .unwrap()
            .encode(&pcm_buffer, frame_size)
            .map_err(|err| err.to_string())?;
        total_samples += frame_size as u64;
        match &output {
            OpusOutput::Ogg(tx) => {
                let pages = ogg.write_packet(&encoded, total_samples, false, true);
                send_pages(&pages, tx)?;
            }
            OpusOutput::Raw(tx, _) => {
                send_raw_frame(&encoded, tx, cache.as_mut())?;
                send_raw_eos(tx, cache.as_mut())?;
            }
        }
    } else {
        match &output {
            OpusOutput::Ogg(tx) => {
                let pages = ogg.write_packet(&[], total_samples, false, true);
                send_pages(&pages, tx)?;
            }
            OpusOutput::Raw(tx, _) => {
                send_raw_eos(tx, cache.as_mut())?;
            }
        }
    }

    if let Some(cache) = cache {
        if let Err(err) = cache.finalize() {
            tracing::warn!("Stream cache finalize failed: {}", err);
        }
    }

    tracing::info!(
        "QUIC transcode done start_ms={} frames={} total_samples={}",
        start_ms,
        packet_counter,
        total_samples
    );

    Ok(())
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
    let mut seek_used = false;
    let mut seek_skip_per_channel = None;
    let mut seek_error: Option<String> = None;
    if force_seek || start_ms > 0 {
        let seconds = (start_ms / 1000) as u64;
        let frac = (start_ms % 1000) as f64 / 1000.0;
        let time = Time::new(seconds, frac);
        let seek_mode = SeekMode::Coarse;
        match format.seek(
            seek_mode,
            SeekTo::Time {
                time,
                track_id: Some(*track_id),
            },
        ) {
            Ok(seeked) => {
                seek_used = true;
                if let Some(time_base) = *track_time_base {
                    let required = time_base.calc_time(seeked.required_ts);
                    let actual = time_base.calc_time(seeked.actual_ts);
                    let required_secs = required.seconds as f64 + required.frac;
                    let actual_secs = actual.seconds as f64 + actual.frac;
                    let delta_secs = required_secs - actual_secs;
                    if delta_secs > 0.0 {
                        let skip = (delta_secs * TARGET_SAMPLE_RATE as f64).round() as u64;
                        let max_skip = (TARGET_SAMPLE_RATE as u64)
                            .saturating_mul(MAX_SEEK_SKIP_MS as u64)
                            / 1000;
                        if skip > max_skip {
                            tracing::info!(
                                "QUIC transcode seek skip capped: requested_skip={} max_skip={} (ms={})",
                                skip,
                                max_skip,
                                MAX_SEEK_SKIP_MS
                            );
                            seek_skip_per_channel = None;
                        } else {
                            seek_skip_per_channel = Some(skip);
                        }
                    }
                }
                tracing::info!(
                    "QUIC transcode seek start_ms={} actual_ts={} required_ts={} skip_samples_per_channel={}",
                    start_ms,
                    seeked.actual_ts,
                    seeked.required_ts,
                    seek_skip_per_channel.unwrap_or(0)
                );
            }
            Err(err) => {
                seek_error = Some(err.to_string());
            }
        }
    }

    if let Some(err) = seek_error {
        tracing::warn!("QUIC transcode seek failed (fallback to skip): {}", err);
        let bundle = open_format(path)?;
        *format = bundle.format;
        *track_id = bundle.track_id;
        *track_codec_params = bundle.track_codec_params;
        *track_time_base = bundle.track_time_base;
        seek_used = false;
        seek_skip_per_channel = None;
    }

    Ok((seek_used, seek_skip_per_channel))
}

fn send_worker_error(tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>, message: &str) {
    let _ = tx.blocking_send(Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        message.to_string(),
    )));
}

fn transcode_raw_worker_loop(
    path: PathBuf,
    selector: BitrateSelector,
    frame_ms: u32,
    meta: RawOpusMeta,
    mut start_ms: u32,
    mut tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    cmd_rx: std_mpsc::Receiver<TranscodeCommand>,
    mut cache: Option<CacheWriter>,
) -> Result<(), String> {
    let frame_ms = validate_frame_ms(frame_ms)?;
    start_ms = start_ms.min(meta.duration_ms);
    let mut bundle = open_format(&path)?;
    let mut format = bundle.format;
    let mut track_id = bundle.track_id;
    let mut track_codec_params = bundle.track_codec_params;
    let mut track_time_base = bundle.track_time_base;
    let mut channels: Option<u8> = None;
    let mut resampler: Option<LinearResampler> = None;
    let mut pcm_buffer: Vec<i16> = Vec::new();
    let mut pcm_offset: usize = 0;
    let mut encoder: Option<OpusEncoderWrapper> = None;
    let mut current_bitrate = 0u32;
    let mut frame_size = 0usize;
    let mut packet_counter = 0u32;
    let mut skip_samples: u64 = 0;
    let mut skip_initialized = false;
    let mut seek_used = false;
    let mut seek_skip_per_channel: Option<u64> = None;
    let mut total_samples = 0u64;
    let mut at_eof = false;

    let (used, skip) = apply_seek(
        &path,
        &mut format,
        &mut track_id,
        &mut track_codec_params,
        &mut track_time_base,
        start_ms,
        start_ms > 0,
    )?;
    seek_used = used;
    seek_skip_per_channel = skip;
    let mut decoder: Box<dyn symphonia::core::codecs::Decoder> = symphonia::default::get_codecs()
        .make(&track_codec_params, &DecoderOptions::default())
        .map_err(|err| err.to_string())?;

    loop {
        if at_eof {
            match cmd_rx.recv() {
                Ok(TranscodeCommand::Seek {
                    start_ms: next_ms,
                    tx: next_tx,
                }) => {
                    tx = next_tx;
                    start_ms = next_ms.min(meta.duration_ms);
                    cache = None;
                    let (used, skip) = apply_seek(
                        &path,
                        &mut format,
                        &mut track_id,
                        &mut track_codec_params,
                        &mut track_time_base,
                        start_ms,
                        true,
                    )?;
                    seek_used = used;
                    seek_skip_per_channel = skip;
                    decoder = symphonia::default::get_codecs()
                        .make(&track_codec_params, &DecoderOptions::default())
                        .map_err(|err| err.to_string())?;
                    resampler = None;
                    pcm_buffer.clear();
                    pcm_offset = 0;
                    encoder = None;
                    current_bitrate = 0;
                    frame_size = 0;
                    packet_counter = 0;
                    total_samples = 0;
                    channels = None;
                    skip_samples = 0;
                    skip_initialized = false;
                    at_eof = false;
                    continue;
                }
                Ok(TranscodeCommand::Stop) | Err(_) => return Ok(()),
            }
        }

        let mut pending_seek: Option<(
            u32,
            tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
        )> = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TranscodeCommand::Seek {
                    start_ms: next_ms,
                    tx: next_tx,
                } => {
                    pending_seek = Some((next_ms, next_tx));
                }
                TranscodeCommand::Stop => return Ok(()),
            }
        }

        if let Some((next_ms, next_tx)) = pending_seek.take() {
            tx = next_tx;
            start_ms = next_ms.min(meta.duration_ms);
            cache = None;
            let (used, skip) = apply_seek(
                &path,
                &mut format,
                &mut track_id,
                &mut track_codec_params,
                &mut track_time_base,
                start_ms,
                true,
            )?;
            seek_used = used;
            seek_skip_per_channel = skip;
            decoder = symphonia::default::get_codecs()
                .make(&track_codec_params, &DecoderOptions::default())
                .map_err(|err| err.to_string())?;
            resampler = None;
            pcm_buffer.clear();
            pcm_offset = 0;
            encoder = None;
            current_bitrate = 0;
            frame_size = 0;
            packet_counter = 0;
            total_samples = 0;
            channels = None;
            skip_samples = 0;
            skip_initialized = false;
            continue;
        }

        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                let err = "decoder reset required".to_string();
                send_worker_error(&tx, &err);
                return Err(err);
            }
            Err(SymphoniaError::IoError(_)) => {
                if encoder.is_none() {
                    let err = "encoder not initialized".to_string();
                    send_worker_error(&tx, &err);
                    return Err(err);
                }

                if skip_samples > 0 && !pcm_buffer.is_empty() {
                    let drop = std::cmp::min(skip_samples as usize, pcm_buffer.len());
                    if drop > 0 {
                        pcm_buffer.drain(..drop);
                    }
                }

                if !pcm_buffer.is_empty() {
                    let channels = encoder.as_ref().unwrap().channels() as usize;
                    let needed = frame_size * channels;
                    if pcm_buffer.len() < needed {
                        pcm_buffer.resize(needed, 0);
                    }
                    let encoded = encoder
                        .as_mut()
                        .unwrap()
                        .encode(&pcm_buffer, frame_size)
                        .map_err(|err| err.to_string())?;
                    total_samples += frame_size as u64;
                    if let Err(err) = send_raw_frame(&encoded, &tx, cache.as_mut()) {
                        send_worker_error(&tx, &err);
                        return Err(err);
                    }
                    let _ = send_raw_eos(&tx, cache.as_mut());
                } else {
                    let _ = send_raw_eos(&tx, cache.as_mut());
                }

                if let Some(cache) = cache.take() {
                    if let Err(err) = cache.finalize() {
                        tracing::warn!("Stream cache finalize failed: {}", err);
                    }
                }
                tracing::info!(
                    "QUIC transcode done start_ms={} frames={} total_samples={}",
                    start_ms,
                    packet_counter,
                    total_samples
                );
                at_eof = true;
                continue;
            }
            Err(err) => {
                let err_text = err.to_string();
                send_worker_error(&tx, &err_text);
                return Err(err_text);
            }
        };
        let decoded = match decoder.decode(&packet) {
            Ok(value) => value,
            Err(err) => {
                let err_text = err.to_string();
                send_worker_error(&tx, &err_text);
                return Err(err_text);
            }
        };

        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count() as u8;
        if decoded_channels == 0 || decoded_channels > 2 {
            let err = "unsupported channel count".to_string();
            send_worker_error(&tx, &err);
            return Err(err);
        }
        if channels.is_none() {
            channels = Some(decoded_channels);
        }
        let channels = channels.unwrap();
        if !skip_initialized && start_ms > 0 {
            let per_channel = if seek_used {
                seek_skip_per_channel.unwrap_or(0)
            } else {
                (TARGET_SAMPLE_RATE as u64).saturating_mul(start_ms as u64) / 1000
            };
            if per_channel > 0 {
                skip_samples = per_channel.saturating_mul(channels as u64);
            }
            skip_initialized = true;
        }

        if encoder.is_none() {
            let bitrate = match selector.mode {
                TranscodeMode::Fixed => selector
                    .fixed_bitrate_bps
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
                TranscodeMode::Auto => selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality)),
            };
            let created = OpusEncoderWrapper::new(TARGET_SAMPLE_RATE, channels, bitrate)
                .map_err(|err| err.to_string())?;
            if let Err(err) = send_raw_header(
                &meta,
                created.sample_rate(),
                created.channels(),
                frame_ms,
                bitrate,
                &tx,
                cache.as_mut(),
            ) {
                send_worker_error(&tx, &err);
                return Err(err);
            }
            frame_size = (created.sample_rate() / 1000 * frame_ms) as usize;
            current_bitrate = bitrate;
            encoder = Some(created);
        }

        let mut sample_buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();

        let input_rate = spec.rate;
        let output_samples = if input_rate == TARGET_SAMPLE_RATE {
            samples.to_vec()
        } else {
            if resampler.is_none() {
                resampler = Some(LinearResampler::new(
                    input_rate,
                    TARGET_SAMPLE_RATE,
                    channels,
                ));
            }
            resampler
                .as_mut()
                .ok_or_else(|| "resampler missing".to_string())?
                .process(samples)
        };

        pcm_buffer.extend_from_slice(&output_samples);
        if skip_samples > 0 {
            let available = pcm_buffer.len().saturating_sub(pcm_offset);
            let drop = std::cmp::min(skip_samples as usize, available);
            if drop > 0 {
                pcm_offset = pcm_offset.saturating_add(drop);
                skip_samples = skip_samples.saturating_sub(drop as u64);
            }
            if skip_samples > 0 {
                if pcm_offset > 0 {
                    pcm_buffer.drain(..pcm_offset);
                    pcm_offset = 0;
                }
                continue;
            }
        }

        while pcm_buffer.len().saturating_sub(pcm_offset) >= frame_size * channels as usize {
            if selector.mode == TranscodeMode::Auto {
                let desired = selector
                    .adaptive_bitrate_bps
                    .as_ref()
                    .map(|value| value.load(Ordering::Relaxed))
                    .unwrap_or_else(|| quality_bitrate(selector.quality));
                if desired != current_bitrate {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(desired).map_err(|err| err.to_string())?;
                    }
                    current_bitrate = desired;
                }
            }

            let end = pcm_offset + frame_size * channels as usize;
            let frame = &pcm_buffer[pcm_offset..end];
            let encoded = encoder
                .as_mut()
                .ok_or_else(|| "encoder not initialized".to_string())?
                .encode(frame, frame_size)
                .map_err(|err| err.to_string())?;
            total_samples += frame_size as u64;
            if let Err(err) = send_raw_frame(&encoded, &tx, cache.as_mut()) {
                send_worker_error(&tx, &err);
                return Err(err);
            }
            pcm_offset = end;
            packet_counter = packet_counter.wrapping_add(1);
        }

        if pcm_offset > 0 {
            pcm_buffer.drain(..pcm_offset);
            pcm_offset = 0;
        }
    }
}

fn send_headers(
    ogg: &mut OggWriter,
    channels: u8,
    sample_rate: u32,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    let head = opus_head_packet(channels, sample_rate);
    let tags = opus_tags_packet("phonolite");
    let head_pages = ogg.write_packet(&head, 0, true, false);
    send_pages(&head_pages, tx)?;
    let tag_pages = ogg.write_packet(&tags, 0, false, false);
    send_pages(&tag_pages, tx)?;
    Ok(())
}

fn send_pages(
    pages: &[Bytes],
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    for page in pages {
        tx.blocking_send(Ok(page.clone()))
            .map_err(|_| "stream closed".to_string())?;
    }
    Ok(())
}

fn send_raw_header(
    meta: &RawOpusMeta,
    sample_rate: u32,
    channels: u8,
    frame_ms: u32,
    bitrate_bps: u32,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    cache: Option<&mut CacheWriter>,
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
    buf.extend_from_slice(&0u16.to_le_bytes()); // pre_skip
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

    if let Some(cache) = cache {
        if let Err(err) = cache.write_header(&buf) {
            tracing::warn!("Stream cache write header failed: {}", err);
        }
    }
    tx.blocking_send(Ok(Bytes::from(buf)))
        .map_err(|_| "stream closed".to_string())?;
    Ok(())
}

fn send_raw_header_cache(
    meta: &RawOpusMeta,
    sample_rate: u32,
    channels: u8,
    frame_ms: u32,
    bitrate_bps: u32,
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
    buf.extend_from_slice(&0u16.to_le_bytes()); // pre_skip
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

fn send_raw_frame(
    data: &[u8],
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    cache: Option<&mut CacheWriter>,
) -> Result<(), String> {
    let len = data.len();
    if len > u16::MAX as usize {
        return Err("opus frame too large".to_string());
    }
    let mut buf = Vec::with_capacity(2 + len);
    buf.extend_from_slice(&(len as u16).to_le_bytes());
    buf.extend_from_slice(data);
    if let Some(cache) = cache {
        if let Err(err) = cache.write_frame(&buf) {
            tracing::warn!("Stream cache write frame failed: {}", err);
        }
    }
    tx.blocking_send(Ok(Bytes::from(buf)))
        .map_err(|_| "stream closed".to_string())?;
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

fn send_raw_eos(
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    cache: Option<&mut CacheWriter>,
) -> Result<(), String> {
    let buf = (0u16).to_le_bytes().to_vec();
    if let Some(cache) = cache {
        if let Err(err) = cache.write_frame(&buf) {
            tracing::warn!("Stream cache write eos failed: {}", err);
        }
    }
    tx.blocking_send(Ok(Bytes::from(buf)))
        .map_err(|_| "stream closed".to_string())?;
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

fn serial_from_path(path: &Path) -> u32 {
    let hash = blake3::hash(path.to_string_lossy().as_bytes());
    let bytes = hash.as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn opus_head_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(19);
    packet.extend_from_slice(b"OpusHead");
    packet.push(1);
    packet.push(channels);
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.extend_from_slice(&sample_rate.to_le_bytes());
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.push(0);
    packet
}

fn opus_tags_packet(vendor: &str) -> Vec<u8> {
    let vendor_bytes = vendor.as_bytes();
    let mut packet = Vec::new();
    packet.extend_from_slice(b"OpusTags");
    packet.extend_from_slice(&(vendor_bytes.len() as u32).to_le_bytes());
    packet.extend_from_slice(vendor_bytes);
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet
}

struct OggWriter {
    serial: u32,
    sequence: u32,
}

impl OggWriter {
    fn new(serial: u32) -> Self {
        Self {
            serial,
            sequence: 0,
        }
    }

    fn write_packet(
        &mut self,
        packet: &[u8],
        granule_pos: u64,
        bos: bool,
        eos: bool,
    ) -> Vec<Bytes> {
        let mut segments = Vec::new();
        let mut remaining = packet.len();
        while remaining >= 255 {
            segments.push(255u8);
            remaining -= 255;
        }
        segments.push(remaining as u8);

        let mut pages = Vec::new();
        let mut seg_index = 0usize;
        let mut data_offset = 0usize;
        let mut first = true;

        while seg_index < segments.len() {
            let max_segments = 255usize;
            let end = (seg_index + max_segments).min(segments.len());
            let page_segments = &segments[seg_index..end];
            let data_len: usize = page_segments.iter().map(|v| *v as usize).sum();
            let data = &packet[data_offset..data_offset + data_len];

            let is_last = end == segments.len();
            let header_type = (if !first { 0x01 } else { 0 })
                | (if bos && first { 0x02 } else { 0 })
                | (if eos && is_last { 0x04 } else { 0 });
            let page_granule = if is_last { granule_pos } else { u64::MAX };
            let page = build_ogg_page(
                header_type,
                page_granule,
                self.serial,
                self.sequence,
                page_segments,
                data,
            );
            pages.push(page);
            self.sequence = self.sequence.wrapping_add(1);
            seg_index = end;
            data_offset += data_len;
            first = false;
        }

        pages
    }
}

fn build_ogg_page(
    header_type: u8,
    granule_pos: u64,
    serial: u32,
    sequence: u32,
    segments: &[u8],
    data: &[u8],
) -> Bytes {
    let mut page = Vec::with_capacity(27 + segments.len() + data.len());
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(header_type);
    page.extend_from_slice(&granule_pos.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(segments.len() as u8);
    page.extend_from_slice(segments);
    page.extend_from_slice(data);

    let crc = ogg_crc32(&page);
    let crc_bytes = crc.to_le_bytes();
    page[22] = crc_bytes[0];
    page[23] = crc_bytes[1];
    page[24] = crc_bytes[2];
    page[25] = crc_bytes[3];
    Bytes::from(page)
}

fn ogg_crc32(data: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = (i as u32) << 24;
            for _ in 0..8 {
                if (crc & 0x8000_0000) != 0 {
                    crc = (crc << 1) ^ 0x04C11DB7;
                } else {
                    crc <<= 1;
                }
            }
            *entry = crc;
        }
        table
    });

    let mut crc = 0u32;
    for &b in data {
        let idx = ((crc >> 24) as u8) ^ b;
        crc = (crc << 8) ^ table[idx as usize];
    }
    crc
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
