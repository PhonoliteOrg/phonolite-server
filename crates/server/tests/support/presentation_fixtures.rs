/// Extend the existing one-chunk AAC fixture without invoking an encoder. The
/// repeated access units are valid; only the final unit retains its tail trim.
pub fn long_aac(source: &[u8], repeats: usize) -> (Vec<u8>, u64) {
    assert!(repeats > 1);
    let audible = repeats as u64 * 20_480 - 1024 - 256;
    (rewrite_atoms(source, repeats, audible as u32), audible)
}

fn rewrite_atoms(source: &[u8], repeats: usize, audible: u32) -> Vec<u8> {
    let mut output = Vec::new();
    let mut position = 0;
    while position < source.len() {
        let size = u32::from_be_bytes(source[position..position + 4].try_into().unwrap()) as usize;
        assert!(size >= 8 && position + size <= source.len());
        let kind = &source[position + 4..position + 8];
        let mut data = source[position + 8..position + size].to_vec();
        match kind {
            b"moov" | b"trak" | b"edts" | b"mdia" | b"minf" | b"stbl" => {
                data = rewrite_atoms(&data, repeats, audible);
            }
            b"mdat" => data = data.repeat(repeats),
            b"mvhd" => {
                data[12..16].copy_from_slice(&48_000u32.to_be_bytes());
                data[16..20].copy_from_slice(&audible.to_be_bytes());
            }
            b"tkhd" => data[20..24].copy_from_slice(&audible.to_be_bytes()),
            b"mdhd" => data[16..20].copy_from_slice(&(audible + 1024).to_be_bytes()),
            b"elst" => data[8..12].copy_from_slice(&audible.to_be_bytes()),
            b"stsz" => {
                assert_eq!(u32::from_be_bytes(data[8..12].try_into().unwrap()), 20);
                let sizes = data[12..].repeat(repeats);
                data.truncate(12);
                data[8..12].copy_from_slice(&(20 * repeats as u32).to_be_bytes());
                data.extend(sizes);
            }
            b"stsc" => {
                assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 1);
                data[12..16].copy_from_slice(&(20 * repeats as u32).to_be_bytes());
            }
            b"stts" => {
                assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 2);
                data[8..12].copy_from_slice(&(20 * repeats as u32 - 1).to_be_bytes());
            }
            _ => (),
        }
        output.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend(data);
        position += size;
    }
    output
}

/// Repacketize existing valid Vorbis audio into a nine-second multi-page
/// stream. Repeating only long-window packets preserves legal window order.
pub fn long_vorbis(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut packets = Vec::new();
    let mut serial = [0; 4];
    let mut position = 0;
    while position < source.len() {
        let count = source[position + 26] as usize;
        let sizes = &source[position + 27..position + 27 + count];
        let end = position + 27 + count + sizes.iter().map(|size| *size as usize).sum::<usize>();
        if source[position + 5] & 4 == 0 {
            output.extend_from_slice(&source[position..end]);
        } else {
            serial.copy_from_slice(&source[position + 14..position + 18]);
            let mut cursor = position + 27 + count;
            let mut packet = Vec::new();
            for &size in sizes {
                packet.extend_from_slice(&source[cursor..cursor + size as usize]);
                cursor += size as usize;
                if size < 255 {
                    packets.push(std::mem::take(&mut packet));
                }
            }
        }
        position = end;
    }
    assert_eq!(packets.len(), 21);
    let mut long = packets[..2].to_vec();
    for _ in 0..25 {
        long.extend_from_slice(&packets[2..19]);
    }
    long.extend_from_slice(&packets[19..]);
    let mut frames = 0u64;
    let mut previous = None;
    for (index, group) in long.chunks(13).enumerate() {
        for packet in group {
            let block: u64 = if packet[0] & 2 == 0 { 256 } else { 2048 };
            if let Some(previous) = previous {
                frames += (previous + block) / 4;
            }
            previous = Some(block);
        }
        let last = (index + 1) * 13 >= long.len();
        let mut page = b"OggS\0".to_vec();
        page.push(if last { 4 } else { 0 });
        page.extend_from_slice(&(frames - if last { 832 } else { 0 }).to_le_bytes());
        page.extend_from_slice(&serial);
        page.extend_from_slice(&(index as u32 + 2).to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        page.push(group.len() as u8);
        page.extend(
            group
                .iter()
                .map(|packet| u8::try_from(packet.len()).unwrap()),
        );
        for packet in group {
            page.extend_from_slice(packet);
        }
        let mut crc = 0u32;
        for &byte in &page {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 == 0 {
                    crc << 1
                } else {
                    (crc << 1) ^ 0x04c1_1db7
                };
            }
        }
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        output.extend(page);
    }
    output
}
