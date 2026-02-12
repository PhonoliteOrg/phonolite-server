use library::Library;
use common::Track;

use crate::transcode::{RawOpusMeta, TranscodeMode, TranscodeQuality};

pub fn parse_transcode_mode(value: Option<&str>) -> Result<TranscodeMode, String> {
    let value = value.unwrap_or("auto").trim().to_ascii_lowercase();
    match value.as_str() {
        "auto" => Ok(TranscodeMode::Auto),
        "fixed" | "manual" => Ok(TranscodeMode::Fixed),
        other => Err(format!("invalid mode: {}", other)),
    }
}

pub fn parse_transcode_quality(value: Option<&str>) -> Result<TranscodeQuality, String> {
    let value = value.unwrap_or("high").trim().to_ascii_lowercase();
    match value.as_str() {
        "high" => Ok(TranscodeQuality::High),
        "medium" | "med" => Ok(TranscodeQuality::Medium),
        "low" => Ok(TranscodeQuality::Low),
        other => Err(format!("invalid quality: {}", other)),
    }
}

pub fn parse_frame_ms(value: Option<u32>) -> Result<u32, String> {
    let frame_ms = value.unwrap_or(20);
    match frame_ms {
        2 | 5 | 10 | 20 | 40 | 60 => Ok(frame_ms),
        _ => Err("invalid frame_ms (allowed 2,5,10,20,40,60)".to_string()),
    }
}

pub fn transcode_mode_label(mode: TranscodeMode) -> &'static str {
    match mode {
        TranscodeMode::Auto => "auto",
        TranscodeMode::Fixed => "fixed",
    }
}

pub fn transcode_quality_label(quality: TranscodeQuality) -> &'static str {
    match quality {
        TranscodeQuality::High => "high",
        TranscodeQuality::Medium => "medium",
        TranscodeQuality::Low => "low",
    }
}

pub fn target_bitrate_kbps(
    mode: TranscodeMode,
    quality: TranscodeQuality,
    fixed_kbps: Option<u32>,
) -> Option<u32> {
    let bitrate_bps = match mode {
        TranscodeMode::Fixed => fixed_kbps
            .map(|v| v.saturating_mul(1000))
            .unwrap_or_else(|| quality_bitrate_bps(quality)),
        TranscodeMode::Auto => quality_bitrate_bps(quality),
    };
    Some(bitrate_bps / 1000)
}

fn quality_bitrate_bps(quality: TranscodeQuality) -> u32 {
    match quality {
        TranscodeQuality::High => 160_000,
        TranscodeQuality::Medium => 96_000,
        TranscodeQuality::Low => 48_000,
    }
}

pub fn build_raw_opus_meta(library: &Library, track: &Track) -> RawOpusMeta {
    let artist_name = library
        .get_artist(&track.artist_id)
        .ok()
        .flatten()
        .map(|artist| artist.name)
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let album_title = library
        .get_album(&track.album_id)
        .ok()
        .flatten()
        .map(|album| album.title)
        .unwrap_or_else(|| "Unknown Album".to_string());

    RawOpusMeta {
        track_id: track.id.clone(),
        title: track.title.clone(),
        artist: artist_name,
        album: album_title,
        duration_ms: track.duration_ms,
        codec: "opus".to_string(),
        container: "raw".to_string(),
    }
}
