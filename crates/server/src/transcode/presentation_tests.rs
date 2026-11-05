use super::*;

#[path = "../../tests/support/presentation_fixtures.rs"]
mod fixtures;

struct PacketSink(Vec<Vec<u8>>);

impl TranscodeSink for PacketSink {
    fn check_cancelled(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn write_header(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.push(bytes.to_vec());
        Ok(())
    }
    fn write_frame(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.push(bytes.to_vec());
        Ok(())
    }
}

struct ReferenceWave(std::path::PathBuf);

impl ReferenceWave {
    fn new(samples: &[i16]) -> Self {
        let size = samples.len() as u32 * 2;
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(36 + size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&192_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&size.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Self::bytes("wav", &bytes)
    }

    fn bytes(extension: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "phonolite-presentation-{}.{extension}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
}

impl Drop for ReferenceWave {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/formats")
        .join(name)
}

fn raw_aac() -> Vec<i16> {
    decoded_source(&fixture("tone.aac"))
}

fn decoded_source(path: &Path) -> Vec<i16> {
    let mut source = open_format(path).unwrap();
    let mut decoder =
        opus::make_decoder(&source.track_codec_params, &DecoderOptions::default()).unwrap();
    let mut samples = None;
    let mut output = Vec::new();
    loop {
        match source.format.next_packet() {
            Ok(packet) => {
                let decoded = decoder.decode(&packet).unwrap();
                output.extend_from_slice(interleaved_samples(&mut samples, decoded));
            }
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(error) => panic!("{error}"),
        }
    }
    output
}

fn decode_packets(packets: Vec<Vec<u8>>) -> Vec<i16> {
    let header = &packets[0];
    let channels = header[16] as usize;
    let skip = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
    let frames = u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap()) as usize;
    let mut decoder = codecs_ffi::OpusDecoderWrapper::new(48_000, channels as u8).unwrap();
    let mut decoded = Vec::new();
    for packet in &packets[1..] {
        decoded.extend(decoder.decode(&packet[2..]).unwrap());
    }
    decoded[skip * channels..(skip + frames) * channels].to_vec()
}

fn packets(
    path: &Path,
    quality: TranscodeQuality,
    start: u32,
    end: u32,
    indexed_duration: u32,
) -> Vec<Vec<u8>> {
    let mut sink = PacketSink(Vec::new());
    transcode_raw_opus_range_cache(
        path,
        BitrateSelector {
            quality,
            fixed_bitrate_bps: None,
        },
        20,
        RawOpusMeta {
            track_id: "presentation".into(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration_ms: indexed_duration,
            codec: "aac".into(),
            container: "mp4".into(),
        },
        start,
        end,
        &mut sink,
    )
    .unwrap();
    sink.0
}

#[test]
fn aac_edit_presentation_matches_reference_pcm_at_every_stream_quality_and_seek() {
    let raw = raw_aac();
    assert_eq!(raw.len(), 20_480 * 2);
    // Independent ADTS packaging exposes raw AAC samples. The MP4 edit list
    // declares media start 1024 and 400 ms of 48 kHz presentation audio.
    let reference = ReferenceWave::new(&raw[1024 * 2..(1024 + 19_200) * 2]);
    let source = fixture("aac.m4a");
    assert_eq!(metadata::read_tags(&source).unwrap().duration_ms, Some(400));
    for quality in [
        TranscodeQuality::High,
        TranscodeQuality::Medium,
        TranscodeQuality::Low,
    ] {
        for start in [0, 105, 350] {
            assert_eq!(
                packets(&source, quality, start, 400, 400),
                packets(&reference.0, quality, start, 400, 400),
                "{quality:?} at{start}ms"
            );
        }
    }
}

#[test]
fn old_indexed_aac_duration_cannot_request_padding_or_fail_empty_tail_ranges() {
    let source = fixture("aac.m4a");
    for start in [0, 350, 410] {
        let output = packets(&source, TranscodeQuality::High, start, 421, 421);
        let header = &output[0];
        let frames = u64::from_le_bytes(header[header.len() - 8..].try_into().unwrap());
        assert_eq!(frames, 19_200u64.saturating_sub(u64::from(start) * 48));
    }
}

#[test]
fn durable_aac_opus_files_use_the_same_trimmed_presentation() {
    let raw = raw_aac();
    let reference = ReferenceWave::new(&raw[1024 * 2..(1024 + 19_200) * 2]);
    for bitrate in [64_000, 96_000, 128_000, 192_000] {
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        assert_eq!(
            file::encode(&fixture("aac.m4a"), bitrate, &mut actual, &|| false).unwrap(),
            19_200
        );
        assert_eq!(
            file::encode(&reference.0, bitrate, &mut expected, &|| false).unwrap(),
            19_200
        );
        assert_eq!(actual, expected, "{bitrate}bps");
    }
}

#[test]
fn multi_page_vorbis_ranges_keep_presentation_alignment_at_all_qualities() {
    let source = ReferenceWave::bytes(
        "ogg",
        &fixtures::long_vorbis(&std::fs::read(fixture("vorbis.ogg")).unwrap()),
    );
    let pcm = decoded_source(&source.0);
    assert_eq!(pcm.len(), 436_992 * 2);
    let reference = ReferenceWave::new(&pcm);
    for quality in [
        TranscodeQuality::High,
        TranscodeQuality::Medium,
        TranscodeQuality::Low,
    ] {
        for start in [0, 3100, 8000, 9050] {
            assert_eq!(
                packets(&source.0, quality, start, 9104, 9104),
                packets(&reference.0, quality, start, 9104, 9104),
                "{quality:?} at{start}ms"
            );
        }
    }
}

#[test]
fn long_aac_ranges_preserve_fractional_tail_and_bounded_seek_alignment() {
    let (m4a, frames) = fixtures::long_aac(&std::fs::read(fixture("aac.m4a")).unwrap(), 30);
    let source = ReferenceWave::bytes("m4a", &m4a);
    let adts = ReferenceWave::bytes(
        "aac",
        &std::fs::read(fixture("tone.aac")).unwrap().repeat(30),
    );
    let raw = decoded_source(&adts.0);
    let reference = ReferenceWave::new(&raw[1024 * 2..(1024 + frames as usize) * 2]);
    for quality in [
        TranscodeQuality::High,
        TranscodeQuality::Medium,
        TranscodeQuality::Low,
    ] {
        let full = packets(&source.0, quality, 0, 12_773, 12_773);
        assert_eq!(
            u64::from_le_bytes(full[0][full[0].len() - 8..].try_into().unwrap()),
            frames
        );
        assert_eq!(full, packets(&reference.0, quality, 0, 12_773, 12_773));
        for start in [3100, 8000, 12_770] {
            let actual = decode_packets(packets(&source.0, quality, start, 12_773, 12_773));
            let expected = decode_packets(packets(&reference.0, quality, start, 12_773, 12_773));
            assert_eq!(actual.len(), expected.len());
            let error: f64 = actual
                .iter()
                .zip(&expected)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum();
            let energy: f64 = expected
                .iter()
                .map(|&sample| f64::from(sample).powi(2))
                .sum();
            // Tiny AAC perceptual-noise differences can change Opus's later
            // quantization, especially over the final 3 ms. Frame counts stay
            // exact; this signal bound still rejects a one-packet time shift.
            assert!(
                error / energy < 1e-2,
                "{quality:?} at{start}ms: {}",
                error / energy
            );
        }
    }
}
