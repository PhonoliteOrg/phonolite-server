// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{decode_error, Result};
use symphonia_core::io::ReadBytes;
use symphonia_core::util::bits;

use crate::atoms::{Atom, AtomHeader};

/// Edit list entry.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ElstEntry {
    pub segment_duration: u64,
    pub media_time: i64,
    pub media_rate_int: i16,
    pub media_rate_frac: i16,
}

/// Edit list atom.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ElstAtom {
    header: AtomHeader,
    pub entries: Vec<ElstEntry>,
}

impl Atom for ElstAtom {
    fn header(&self) -> AtomHeader {
        self.header
    }

    fn read<B: ReadBytes>(reader: &mut B, header: AtomHeader) -> Result<Self> {
        let (version, _) = AtomHeader::read_extra(reader)?;

        let entry_count = reader.read_be_u32()?;
        let entry_size = match version {
            0 => 12,
            1 => 20,
            _ => return decode_error("isomp4: invalid presentation edit-list version"),
        };
        if entry_count > 64 || 8 + u64::from(entry_count) * entry_size > header.data_len {
            return decode_error("isomp4: invalid presentation edit list");
        }

        let mut entries = Vec::new();

        for _ in 0..entry_count {
            let (segment_duration, media_time) = match version {
                0 => (
                    u64::from(reader.read_be_u32()?),
                    i64::from(bits::sign_extend_leq32_to_i32(reader.read_be_u32()?, 32)),
                ),
                1 => (
                    reader.read_be_u64()?,
                    bits::sign_extend_leq64_to_i64(reader.read_be_u64()?, 64),
                ),
                _ => return decode_error("isomp4: invalid tkhd version"),
            };

            let media_rate_int = bits::sign_extend_leq16_to_i16(reader.read_be_u16()?, 16);
            let media_rate_frac = bits::sign_extend_leq16_to_i16(reader.read_be_u16()?, 16);

            entries.push(ElstEntry {
                segment_duration,
                media_time,
                media_rate_int,
                media_rate_frac,
            });
        }

        Ok(ElstAtom { header, entries })
    }
}
