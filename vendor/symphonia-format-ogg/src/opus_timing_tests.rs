use std::io::Cursor;

use symphonia_core::checksum::Crc32;
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, Monitor};
use symphonia_core::units::Time;

use crate::OggReader;

fn page(sequence: u32, flags: u8, granule: u64, packets: &[&[u8]]) -> Vec<u8> {
    let mut bytes = b"OggS\0".to_vec();
    bytes.push(flags);
    bytes.extend_from_slice(&granule.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.push(packets.len() as u8);
    for packet in packets {
        assert!(packet.len() < 255);
        bytes.push(packet.len() as u8);
    }
    for packet in packets {
        bytes.extend_from_slice(packet);
    }
    let mut crc = Crc32::new(0);
    crc.process_buf_bytes(&bytes);
    bytes[22..26].copy_from_slice(&crc.crc().to_le_bytes());
    bytes
}

fn reader(audio_pages: &[(u64, usize)], gapless: bool) -> OggReader {
    let mut header = b"OpusHead\x01\x01".to_vec();
    header.extend_from_slice(&312u16.to_le_bytes());
    header.extend_from_slice(&48_000u32.to_le_bytes());
    header.extend_from_slice(&[0; 3]);
    let mut bytes = page(0, 2, 0, &[&header]);
    bytes.extend(page(1, 0, 0, &[b"OpusTags\0\0\0\0\0\0\0\0"]));
    // A valid 20 ms Opus silence packet (960 decoded frames).
    let silence = &[0xf8, 0xff, 0xfe][..];
    for (index, &(granule, count)) in audio_pages.iter().enumerate() {
        let flags = if index + 1 == audio_pages.len() { 4 } else { 0 };
        bytes.extend(page(
            index as u32 + 2,
            flags,
            granule,
            &vec![silence; count],
        ));
    }
    OggReader::try_new(
        MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default()),
        &FormatOptions {
            enable_gapless: gapless,
            ..Default::default()
        },
    )
    .unwrap()
}

fn verify_timeline(mut reader: OggReader, expected_frames: u64, expected_delay: u32) {
    let params = &reader.default_track().unwrap().codec_params;
    assert_eq!(params.delay, Some(expected_delay));
    assert_eq!(params.n_frames, Some(expected_frames));
    let mut cursor = 0;
    let mut start_trim = 0;
    let mut end_trim = 0;
    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(error) => panic!("{}", error),
        };
        assert_eq!(packet.ts, cursor);
        cursor += packet.dur;
        start_trim += packet.trim_start;
        end_trim += packet.trim_end;
    }
    assert_eq!(cursor, expected_frames);
    if expected_delay != 0 {
        assert_eq!(start_trim, 312);
        assert_eq!(end_trim, 648);
    }
}

#[test]
fn single_audio_page_preserves_head_skip_and_tail_padding() {
    // 21 packets = 20160 raw frames; granule 19512 includes pre-skip 312,
    // hence 19200 presentation frames and 648 final padding frames.
    verify_timeline(reader(&[(19_512, 21)], true), 19_200, 312);
}

#[test]
fn multiple_pages_have_a_continuous_presentation_timeline() {
    verify_timeline(reader(&[(48_000, 50), (67_512, 21)], true), 67_200, 312);
}

#[test]
fn seeking_reports_presentation_timestamps_after_pre_skip() {
    let mut source = reader(&[(48_000, 50), (67_512, 21)], true);
    let seek = source
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::new(1, 0.2),
                track_id: Some(1),
            },
        )
        .unwrap();
    assert_eq!(seek.required_ts, 57_600);
    assert!(seek.actual_ts <= seek.required_ts);
    let packet = source.next_packet().unwrap();
    assert_eq!(packet.ts, seek.actual_ts);
    assert!(packet.ts + packet.dur >= seek.required_ts);
    assert_eq!(packet.trim_start, 0);
}

#[test]
fn disabling_gapless_retains_all_decoded_frames() {
    let mut source = reader(&[(19_512, 21)], false);
    assert_eq!(
        source.default_track().unwrap().codec_params.n_frames,
        Some(20_160)
    );
    let mut frames = 0;
    while let Ok(packet) = source.next_packet() {
        assert_eq!(packet.trim_start, 0);
        assert_eq!(packet.trim_end, 0);
        assert_eq!(packet.ts, frames);
        frames += packet.dur;
    }
    assert_eq!(frames, 20_160);
}
