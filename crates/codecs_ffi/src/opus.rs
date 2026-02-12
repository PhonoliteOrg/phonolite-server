use std::os::raw::c_int;

use crate::CodecError;

#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct OpusDecoder {
    _private: [u8; 0],
}

const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_MAX_FRAME_SIZE: usize = 5760;

extern "C" {
    fn opus_encoder_create(
        fs: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: c_int,
    ) -> c_int;
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: c_int,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

#[derive(Debug)]
pub enum OpusEncodeError {
    EncoderUnavailable,
    EncoderInitFailed,
    InvalidInput,
    EncodeFailed,
    UnsupportedRate,
}

impl std::fmt::Display for OpusEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpusEncodeError::EncoderUnavailable => write!(f, "opus encoder unavailable"),
            OpusEncodeError::EncoderInitFailed => write!(f, "opus encoder init failed"),
            OpusEncodeError::InvalidInput => write!(f, "invalid input"),
            OpusEncodeError::EncodeFailed => write!(f, "opus encode failed"),
            OpusEncodeError::UnsupportedRate => write!(f, "unsupported sample rate"),
        }
    }
}

impl std::error::Error for OpusEncodeError {}

#[derive(Debug)]
pub enum OpusDecodeError {
    DecoderUnavailable,
    DecoderInitFailed,
    InvalidInput,
    DecodeFailed,
    UnsupportedRate,
}

impl std::fmt::Display for OpusDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpusDecodeError::DecoderUnavailable => write!(f, "opus decoder unavailable"),
            OpusDecodeError::DecoderInitFailed => write!(f, "opus decoder init failed"),
            OpusDecodeError::InvalidInput => write!(f, "invalid input"),
            OpusDecodeError::DecodeFailed => write!(f, "opus decode failed"),
            OpusDecodeError::UnsupportedRate => write!(f, "unsupported sample rate"),
        }
    }
}

impl std::error::Error for OpusDecodeError {}

pub struct OpusEncoderWrapper {
    encoder: *mut OpusEncoder,
    sample_rate: u32,
    channels: u8,
}

unsafe impl Send for OpusEncoderWrapper {}
unsafe impl Sync for OpusEncoderWrapper {}

impl OpusEncoderWrapper {
    pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self, OpusEncodeError> {
        if !matches!(sample_rate, 8000 | 12000 | 16000 | 24000 | 48000) {
            return Err(OpusEncodeError::UnsupportedRate);
        }
        let mut err = 0;
        let encoder = unsafe {
            opus_encoder_create(
                sample_rate as c_int,
                channels as c_int,
                OPUS_APPLICATION_AUDIO,
                &mut err as *mut c_int,
            )
        };
        if encoder.is_null() || err != OPUS_OK {
            return Err(OpusEncodeError::EncoderInitFailed);
        }
        let ctl_err =
            unsafe { opus_encoder_ctl(encoder, OPUS_SET_BITRATE_REQUEST, bitrate_bps as c_int) };
        if ctl_err != OPUS_OK {
            unsafe { opus_encoder_destroy(encoder) };
            return Err(OpusEncodeError::EncoderInitFailed);
        }
        Ok(Self {
            encoder,
            sample_rate,
            channels,
        })
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), OpusEncodeError> {
        let ctl_err =
            unsafe { opus_encoder_ctl(self.encoder, OPUS_SET_BITRATE_REQUEST, bitrate_bps as c_int) };
        if ctl_err != OPUS_OK {
            return Err(OpusEncodeError::EncodeFailed);
        }
        Ok(())
    }

    pub fn encode(&mut self, pcm: &[i16], frame_size: usize) -> Result<Vec<u8>, OpusEncodeError> {
        if pcm.len() < frame_size * self.channels as usize {
            return Err(OpusEncodeError::InvalidInput);
        }
        let mut out = vec![0u8; 4000];
        let encoded = unsafe {
            opus_encode(
                self.encoder,
                pcm.as_ptr(),
                frame_size as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if encoded < 0 {
            return Err(OpusEncodeError::EncodeFailed);
        }
        out.truncate(encoded as usize);
        Ok(out)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }
}

impl Drop for OpusEncoderWrapper {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            unsafe { opus_encoder_destroy(self.encoder) };
        }
    }
}

pub struct OpusDecoderWrapper {
    decoder: *mut OpusDecoder,
    sample_rate: u32,
    channels: u8,
}

unsafe impl Send for OpusDecoderWrapper {}
unsafe impl Sync for OpusDecoderWrapper {}

impl OpusDecoderWrapper {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self, OpusDecodeError> {
        if !matches!(sample_rate, 8000 | 12000 | 16000 | 24000 | 48000) {
            return Err(OpusDecodeError::UnsupportedRate);
        }
        let mut err = 0;
        let decoder = unsafe {
            opus_decoder_create(
                sample_rate as c_int,
                channels as c_int,
                &mut err as *mut c_int,
            )
        };
        if decoder.is_null() || err != OPUS_OK {
            return Err(OpusDecodeError::DecoderInitFailed);
        }
        Ok(Self {
            decoder,
            sample_rate,
            channels,
        })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, OpusDecodeError> {
        if packet.is_empty() {
            return Err(OpusDecodeError::InvalidInput);
        }
        let mut out = vec![0i16; OPUS_MAX_FRAME_SIZE * self.channels as usize];
        let decoded = unsafe {
            opus_decode(
                self.decoder,
                packet.as_ptr(),
                packet.len() as c_int,
                out.as_mut_ptr(),
                OPUS_MAX_FRAME_SIZE as c_int,
                0,
            )
        };
        if decoded < 0 {
            return Err(OpusDecodeError::DecodeFailed);
        }
        let samples = decoded as usize * self.channels as usize;
        out.truncate(samples);
        Ok(out)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }
}

impl Drop for OpusDecoderWrapper {
    fn drop(&mut self) {
        if !self.decoder.is_null() {
            unsafe { opus_decoder_destroy(self.decoder) };
        }
    }
}

pub fn opus_encode_chunk(
    _pcm_i16: &[i16],
    _sample_rate: u32,
    _channels: u8,
) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::FfiUnavailable)
}
