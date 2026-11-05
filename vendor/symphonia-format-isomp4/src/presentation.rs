// Copyright (c) 2026 Phonolite contributors.
// SPDX-License-Identifier: MPL-2.0

use std::convert::TryFrom;

use symphonia_core::codecs::{CodecParameters, CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_NULL};
use symphonia_core::errors::{unsupported_error, Result};

use crate::atoms::TrakAtom;

/// A single, unit-rate mapping from compressed media time to audible time.
#[derive(Clone, Copy)]
pub struct Presentation {
    pub delay: u32,
    pub frames: u64,
    pub aac: bool,
}

impl Presentation {
    pub fn from_track(
        trak: &TrakAtom,
        movie_scale: u32,
        params: &mut CodecParameters,
    ) -> Result<Option<Self>> {
        let edits = match trak.edts.as_ref().and_then(|edts| edts.elst.as_ref()) {
            Some(edits) if params.codec != CODEC_TYPE_NULL => &edits.entries,
            _ => return Ok(None),
        };
        if edits.len() != 1 {
            return unsupported_error("isomp4: one presentation edit is required");
        }
        let edit = &edits[0];
        if edit.media_time < 0 || edit.media_rate_int != 1 || edit.media_rate_frac != 0 {
            return unsupported_error("isomp4: empty or non-unit-rate presentation edit");
        }
        let media_scale = trak.mdia.mdhd.timescale;
        if movie_scale == 0 || media_scale == 0 {
            return unsupported_error("isomp4: zero presentation timescale");
        }
        let delay = match u32::try_from(edit.media_time) {
            Ok(delay) => delay,
            Err(_) => {
                return unsupported_error("isomp4: presentation delay exceeds supported range")
            }
        };
        // A movie-clock tick may fall between media samples. Round to the
        // nearest sample and permit that tick's rounding at the physical end.
        let scaled = u128::from(edit.segment_duration) * u128::from(media_scale);
        let frames = (scaled + u128::from(movie_scale / 2)) / u128::from(movie_scale);
        let mut frames = match u64::try_from(frames) {
            Ok(frames) => frames,
            Err(_) => return unsupported_error("isomp4: presentation duration overflow"),
        };
        let media_frames = trak.mdia.minf.stbl.stts.total_duration;
        let available = match media_frames.checked_sub(u64::from(delay)) {
            Some(available) => available,
            None => return unsupported_error("isomp4: presentation starts beyond media end"),
        };
        if frames > available {
            let tick =
                (u64::from(media_scale) + u64::from(movie_scale) - 1) / u64::from(movie_scale);
            if frames - available > tick {
                return unsupported_error("isomp4: presentation extends beyond media end");
            }
            frames = available;
        }
        if !matches!(params.codec, CODEC_TYPE_AAC | CODEC_TYPE_ALAC) {
            if delay == 0 && frames == media_frames {
                return Ok(None);
            }
            return unsupported_error("isomp4: edited audio codec is not supported");
        }
        if params.sample_rate != Some(media_scale) {
            return unsupported_error("isomp4: edited audio requires sample-rate media timing");
        }
        let aac = params.codec == CODEC_TYPE_AAC;
        let encoded_frames = if aac {
            // The supported AAC-LC decoder emits 1024 frames per access unit.
            // A final stts entry may describe only its non-padding prefix.
            let entries = &trak.mdia.minf.stbl.stts.entries;
            let mut access_units = 0u64;
            for (index, entry) in entries.iter().enumerate() {
                access_units += u64::from(entry.sample_count);
                if entry.sample_delta != 1024
                    && !(index + 1 == entries.len()
                        && entry.sample_count == 1
                        && entry.sample_delta > 0
                        && entry.sample_delta < 1024)
                {
                    return unsupported_error("isomp4: unsupported AAC access-unit timing");
                }
            }
            if access_units != u64::from(trak.mdia.minf.stbl.stsz.sample_count) {
                return unsupported_error("isomp4: inconsistent AAC sample tables");
            }
            access_units * 1024
        } else {
            media_frames
        };
        let padding = encoded_frames.saturating_sub(u64::from(delay) + frames);
        let padding = match u32::try_from(padding) {
            Ok(padding) => padding,
            Err(_) => {
                return unsupported_error("isomp4: presentation padding exceeds supported range")
            }
        };
        params
            .with_n_frames(frames)
            .with_delay(delay)
            .with_padding(padding);
        Ok(Some(Self { delay, frames, aac }))
    }
}
