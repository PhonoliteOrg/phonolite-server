#[derive(Debug)]
pub enum CodecError {
    FfiUnavailable,
    InvalidPath,
    DecodeFailed,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::FfiUnavailable => write!(f, "ffi unavailable"),
            CodecError::InvalidPath => write!(f, "invalid path"),
            CodecError::DecodeFailed => write!(f, "decode failed"),
        }
    }
}

impl std::error::Error for CodecError {}

#[cfg(feature = "ffi-opus")]
mod opus;

#[cfg(feature = "ffi-opus")]
pub use opus::{opus_encode_chunk, OpusEncodeError, OpusEncoderWrapper};
#[cfg(feature = "ffi-opus")]
pub use opus::{OpusDecodeError, OpusDecoderWrapper};
