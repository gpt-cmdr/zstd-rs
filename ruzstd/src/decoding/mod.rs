//! Structures and utilities used for decoding zstd formatted data

pub mod errors;
mod frame_decoder;
mod streaming_decoder;

pub use dictionary::Dictionary;
pub use frame::content_size;
pub use frame_decoder::{BlockDecodingStrategy, FrameDecoder, DEFAULT_MAX_WINDOW_SIZE};
pub use streaming_decoder::StreamingDecoder;

pub(crate) mod block_decoder;
pub(crate) mod decode_buffer;
pub(crate) mod dictionary;
mod direct;
pub(crate) mod frame;
#[allow(dead_code)]
pub(crate) mod scratch;
pub(crate) mod sequence_section_decoder;
