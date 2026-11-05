//! Probe MP4 audio properties without requiring a fixed sample-description size.
use std::fs::File;
use std::io;
use std::path::Path;

use common::Codec;
use symphonia::core::codecs::{CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::probe::Hint;

use crate::TagInfo;

pub(super) fn read_properties(path: &Path, info: &mut TagInfo) -> io::Result<()> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp4");
    let probed = symphonia::default::get_probe()
        .format(&hint, source, &Default::default(), &Default::default())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    for track in probed.format.tracks() {
        let parameters = &track.codec_params;
        let codec = match parameters.codec {
            CODEC_TYPE_AAC => Codec::Aac,
            CODEC_TYPE_ALAC => Codec::Alac,
            CODEC_TYPE_FLAC => Codec::Flac,
            CODEC_TYPE_MP3 => Codec::Mp3,
            _ => continue,
        };
        info.codec = Some(codec);
        info.sample_rate = parameters
            .sample_rate
            .filter(|rate| *rate > 0)
            .or(info.sample_rate.filter(|rate| *rate > 0));
        info.channels = parameters
            .channels
            .map(|channels| channels.count() as u8)
            .or(info.channels.filter(|channels| *channels > 0));
        if parameters.codec == CODEC_TYPE_ALAC {
            // The demuxer preserves ALACSpecificConfig instead of expanding its
            // channel count and rate into the generic codec parameters.
            if let Some(config) = parameters
                .extra_data
                .as_deref()
                .filter(|data| matches!(data.len(), 24 | 48))
            {
                info.channels = Some(config[9]).filter(|channels| (1..=8).contains(channels));
                info.sample_rate = Some(u32::from_be_bytes(config[20..24].try_into().unwrap()))
                    .filter(|rate| *rate > 0);
                info.bitrate = Some(u32::from_be_bytes(config[16..20].try_into().unwrap()) / 1000)
                    .filter(|bitrate| *bitrate > 0);
            }
        }
        if let (Some(time_base), Some(frames)) = (parameters.time_base, parameters.n_frames) {
            let time = time_base.calc_time(frames);
            let millis = time.seconds.saturating_mul(1000) + (time.frac * 1000.0) as u64;
            info.duration_ms = Some(millis.min(u64::from(u32::MAX)) as u32);
            if millis > 0 {
                // Overall bitrate in kbps, matching the existing metadata unit.
                info.bitrate = info.bitrate.filter(|bitrate| *bitrate > 0).or_else(|| {
                    Some((length.saturating_mul(8) / millis).min(u64::from(u32::MAX)) as u32)
                });
            }
        }
        return Ok(());
    }
    Ok(())
}
