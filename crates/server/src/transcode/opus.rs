//! Decode Ogg Opus packets with the same bundled libopus used for stream encoding.

use codecs_ffi::OpusDecoderWrapper;
use symphonia::core::audio::{
    AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec,
};
use symphonia::core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_OPUS,
};
use symphonia::core::errors::{decode_error, unsupported_error, Error, Result};
use symphonia::core::formats::Packet;

pub(super) fn make_decoder(
    params: &CodecParameters,
    options: &DecoderOptions,
) -> Result<Box<dyn Decoder>> {
    if params.codec == CODEC_TYPE_OPUS {
        Ok(Box::new(OpusDecoder::try_new(params, options)?))
    } else {
        symphonia::default::get_codecs().make(params, options)
    }
}

struct OpusDecoder {
    params: CodecParameters,
    decoder: OpusDecoderWrapper,
    buffer: AudioBuffer<i16>,
    channels: usize,
    reset_failed: bool,
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> Result<Self> {
        if params.codec != CODEC_TYPE_OPUS {
            return unsupported_error("Opus decoder received a different codec");
        }
        let header = params
            .extra_data
            .as_deref()
            .ok_or(Error::DecodeError("Opus identification header is missing"))?;
        if header.len() < 19 || &header[..8] != b"OpusHead" || header[8] > 15 {
            return decode_error("invalid Opus identification header");
        }
        let channels = usize::from(header[9]);
        if !matches!(channels, 1 | 2) {
            return unsupported_error("Opus streaming supports mono and stereo audio");
        }
        // A basic libopus decoder handles one stream, either mono or coupled stereo.
        // Mapping family 1 is also valid when it describes that same stream layout.
        match header[18] {
            0 => {}
            1 if header.len() >= 21 + channels
                && header[19] == 1
                && usize::from(header[20]) == channels - 1
                && header[21..21 + channels]
                    .iter()
                    .copied()
                    .eq(0..channels as u8) => {}
            _ => return unsupported_error("unsupported Opus channel mapping"),
        }
        let channel_mask = if channels == 1 {
            Channels::FRONT_LEFT
        } else {
            Channels::FRONT_LEFT | Channels::FRONT_RIGHT
        };
        if params.sample_rate.is_some_and(|rate| rate != 48_000)
            || params.channels.is_some_and(|mask| mask != channel_mask)
        {
            return decode_error("Opus codec parameters disagree with the identification header");
        }
        let gain = i16::from_le_bytes([header[16], header[17]]);
        let mut decoder = OpusDecoderWrapper::new(48_000, channels as u8)
            .map_err(|_| Error::Unsupported("libopus decoder initialization failed"))?;
        decoder
            .set_gain(gain)
            .map_err(|_| Error::DecodeError("invalid Opus output gain"))?;
        Ok(Self {
            params: params.clone(),
            decoder,
            buffer: AudioBuffer::new(5760, SignalSpec::new(48_000, channel_mask)),
            channels,
            reset_failed: false,
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[CodecDescriptor {
            codec: CODEC_TYPE_OPUS,
            short_name: "opus",
            long_name: "Opus (bundled libopus)",
            inst_func: make_decoder,
        }]
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.reset_failed = self.decoder.reset().is_err();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        self.buffer.clear();
        if self.reset_failed {
            return decode_error("Opus decoder reset failed");
        }
        let interleaved = self
            .decoder
            .decode(&packet.data)
            .map_err(|_| Error::DecodeError("invalid Opus packet"))?;
        let frames = interleaved.len() / self.channels;
        let start = packet.trim_start() as usize;
        let end = packet.trim_end() as usize;
        if start.saturating_add(end) > frames {
            return decode_error("Opus packet trimming exceeds decoded frames");
        }
        self.buffer.render_reserved(Some(frames - start - end));
        for channel in 0..self.channels {
            for (output, input) in self.buffer.chan_mut(channel).iter_mut().zip(
                interleaved[start * self.channels..(frames - end) * self.channels]
                    .chunks_exact(self.channels),
            ) {
                *output = input[channel];
            }
        }
        Ok(self.buffer.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.buffer.as_audio_buffer_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecs_ffi::OpusEncoderWrapper;

    fn params(channels: u8, gain: i16) -> CodecParameters {
        let mut header = b"OpusHead\x01".to_vec();
        header.push(channels);
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&48_000u32.to_le_bytes());
        header.extend_from_slice(&gain.to_le_bytes());
        header.push(0);
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_OPUS)
            .with_extra_data(header.into_boxed_slice());
        params
    }

    fn encoded_packet(channels: u8) -> Vec<u8> {
        let mut encoder = OpusEncoderWrapper::new(48_000, channels, 96_000).unwrap();
        let pcm: Vec<i16> = (0..960)
            .flat_map(|frame| {
                let sample = ((frame as f32 * 0.0576).sin() * 3000.0) as i16;
                std::iter::repeat(sample).take(usize::from(channels))
            })
            .collect();
        encoder.encode(&pcm, 960).unwrap()
    }

    #[test]
    fn decodes_mono_and_stereo_with_gapless_packet_trimming() {
        for channels in [1, 2] {
            let data = encoded_packet(channels);
            let packet = Packet::new_trimmed_from_slice(0, 0, 600, 120, 240, &data);
            let mut decoder =
                make_decoder(&params(channels, 0), &DecoderOptions::default()).unwrap();
            let buffer = decoder.decode(&packet).unwrap();
            assert_eq!(buffer.frames(), 600);
            assert_eq!(buffer.spec().channels.count(), usize::from(channels));
            let AudioBufferRef::S16(buffer) = buffer else {
                panic!("expected i16 PCM")
            };
            assert!(buffer.chan(0).iter().any(|sample| sample.abs() > 100));
        }
    }

    #[test]
    fn output_gain_is_applied_and_reset_retains_gain() {
        let data = encoded_packet(1);
        let packet = Packet::new_from_slice(0, 0, 960, &data);
        let mut normal = make_decoder(&params(1, 0), &DecoderOptions::default()).unwrap();
        let mut quiet = make_decoder(&params(1, -6 * 256), &DecoderOptions::default()).unwrap();
        let energy = |buffer: AudioBufferRef<'_>| {
            let AudioBufferRef::S16(buffer) = buffer else {
                panic!("expected i16 PCM")
            };
            buffer
                .chan(0)
                .iter()
                .map(|&sample| f64::from(sample).powi(2))
                .sum::<f64>()
        };
        let full = energy(normal.decode(&packet).unwrap());
        let reduced = energy(quiet.decode(&packet).unwrap());
        assert!((0.24..0.27).contains(&(reduced / full)));
        quiet.reset();
        assert_eq!(quiet.last_decoded().frames(), 0);
        assert_eq!(energy(quiet.decode(&packet).unwrap()), reduced);
    }

    #[test]
    fn rejects_invalid_headers_layouts_and_packets_and_clears_output() {
        let options = DecoderOptions::default();
        assert!(make_decoder(&params(0, 0), &options).is_err());
        assert!(make_decoder(&params(3, 0), &options).is_err());
        let mut bad = params(2, 0);
        bad.extra_data.as_mut().unwrap()[18] = 255;
        assert!(make_decoder(&bad, &options).is_err());
        let mut decoder = make_decoder(&params(1, 0), &options).unwrap();
        let data = encoded_packet(1);
        assert_eq!(
            decoder
                .decode(&Packet::new_from_slice(0, 0, 960, &data))
                .unwrap()
                .frames(),
            960
        );
        assert!(decoder
            .decode(&Packet::new_from_slice(0, 0, 0, &[]))
            .is_err());
        assert_eq!(decoder.last_decoded().frames(), 0);
        assert!(decoder
            .decode(&Packet::new_trimmed_from_slice(0, 0, 0, 961, 0, &data))
            .is_err());
        assert_eq!(decoder.last_decoded().frames(), 0);
    }

    #[test]
    fn accepts_identity_vorbis_mapping_for_mono_and_stereo() {
        for channels in [1, 2] {
            let mut params = params(channels, 0);
            let mut header = params.extra_data.take().unwrap().into_vec();
            header[18] = 1;
            header.extend_from_slice(&[1, channels - 1]);
            header.extend(0..channels);
            params.extra_data = Some(header.into_boxed_slice());
            assert!(make_decoder(&params, &DecoderOptions::default()).is_ok());
        }
    }
}
