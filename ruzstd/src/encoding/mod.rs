//! Structures and utilities used for compressing/encoding data into the Zstd format.

pub(crate) mod block_header;
pub(crate) mod blocks;
mod dictionary;
pub(crate) mod frame_header;
pub(crate) mod match_generator;
pub(crate) mod util;
pub(crate) mod window_matcher;

mod compressor;
mod frame_compressor;
mod levels;
pub use compressor::Compressor;
pub use dictionary::Dictionary;
pub use frame_compressor::FrameCompressor;
pub use match_generator::MatchGeneratorDriver;

use crate::io::{Read, Write};
use alloc::vec::Vec;

/// Convenience function to compress some source into a target without reusing any resources of the compressor
/// ```rust
/// use zstd_rust::encoding::{compress, CompressionLevel};
/// let data: &[u8] = &[0,0,0,0,0,0,0,0,0,0,0,0];
/// let mut target = Vec::new();
/// compress(data, &mut target, CompressionLevel::Fastest);
/// ```
pub fn compress<R: Read, W: Write>(source: R, target: W, level: CompressionLevel) {
    let mut frame_enc = FrameCompressor::new(level);
    frame_enc.set_source(source);
    frame_enc.set_drain(target);
    frame_enc.compress();
}

/// Convenience function to compress some source into a Vec without reusing any resources of the compressor
/// ```rust
/// use zstd_rust::encoding::{compress_to_vec, CompressionLevel};
/// let data: &[u8] = &[0,0,0,0,0,0,0,0,0,0,0,0];
/// let compressed = compress_to_vec(data, CompressionLevel::Fastest);
/// ```
pub fn compress_to_vec<R: Read>(source: R, level: CompressionLevel) -> Vec<u8> {
    let mut vec = Vec::new();
    compress(source, &mut vec, level);
    vec
}

/// Convenience function to compress some source into a target using a dictionary, see [`Dictionary`]
///
/// The decoder needs the same dictionary to decompress the frame.
/// ```rust
/// use zstd_rust::encoding::{compress_with_dictionary, CompressionLevel, Dictionary};
/// let dict = Dictionary::from_raw_content(b"some data that is typical for the inputs".to_vec());
/// let data: &[u8] = b"some data that is typical for this input";
/// let mut target = Vec::new();
/// compress_with_dictionary(data, &mut target, CompressionLevel::Default, &dict);
/// ```
pub fn compress_with_dictionary<R: Read, W: Write>(
    source: R,
    target: W,
    level: CompressionLevel,
    dictionary: &Dictionary,
) {
    let mut frame_enc = FrameCompressor::new(level);
    frame_enc.set_dictionary(dictionary.clone());
    frame_enc.set_source(source);
    frame_enc.set_drain(target);
    frame_enc.compress();
}

/// Convenience function to compress some source into a Vec using a dictionary, see [`compress_with_dictionary`]
pub fn compress_to_vec_with_dictionary<R: Read>(
    source: R,
    level: CompressionLevel,
    dictionary: &Dictionary,
) -> Vec<u8> {
    let mut vec = Vec::new();
    compress_with_dictionary(source, &mut vec, level, dictionary);
    vec
}

/// The compression mode used impacts the speed of compression,
/// and resulting compression ratios. Faster compression will result
/// in worse compression ratios, and vice versa.
///
/// If the whole input fits into one block (128 kB), the frame header states its size and the window and tables
/// of the matcher are shrunk to fit it, so the memory usage given for each level is an upper bound.
#[derive(Copy, Clone, Debug)]
pub enum CompressionLevel {
    /// This level does not compress the data at all, and simply wraps
    /// it in a Zstandard frame.
    Uncompressed,
    /// This level is roughly equivalent to Zstd compression level 1.
    ///
    /// Uses one hash table of 7-byte prefixes (6-byte ones for data of up to 256 KiB, 5-byte ones up to 16 KiB)
    /// and greedy matching (the reference implementation's `fast` strategy) over a 512 KiB window. Memory usage
    /// of the matcher is about 1.3 MiB.
    Fastest,
    /// This level is roughly equivalent to Zstd level 3,
    /// or the one used by the official compressor when no level
    /// is specified.
    ///
    /// Uses a hash table of 8-byte and one of 5-byte prefixes and greedy matching (the reference
    /// implementation's `dfast` strategy) over a 2 MiB window. Memory usage of the matcher is about 3.3 MiB.
    Default,
    /// This level is roughly equivalent to Zstd level 9.
    ///
    /// Uses rows of 16 tagged positions (the reference implementation's row match finder; hash chains of
    /// depth 16 in frames with a dictionary) with a two-byte lazy match evaluation over a 4 MiB window. Memory
    /// usage of the matcher is about 7.5 MiB.
    Better,
    /// Uses rows of 64 tagged positions (hash chains of depth 128 in frames with a dictionary) with a two-byte
    /// lazy match evaluation over a 8 MiB window. Memory usage of the matcher is about 14.5 MiB.
    Best,
}

/// Trait used by the encoder that users can use to extend the matching facilities with their own algorithm
/// making their own tradeoffs between runtime, memory usage and compression ratio
///
/// This trait operates on buffers that represent the chunks of data the matching algorithm wants to work on.
/// Each one of these buffers is referred to as a *space*. One or more of these buffers represent the window
/// the decoder will need to decode the data again.
///
/// This library asks the Matcher for a new buffer using `get_next_space` to allow reusing of allocated buffers when they are no longer part of the
/// window of data that is being used for matching.
///
/// The library fills the buffer with data that is to be compressed and commits them back to the matcher using `commit_space`.
///
/// Then it will either call `start_matching` or, if the space is deemed not worth compressing, `skip_matching` is called.
///
/// This is repeated until no more data is left to be compressed.
pub trait Matcher {
    /// Get a space where we can put data to be matched on. Will be encoded as one block. The maximum allowed size is 128 kB.
    ///
    /// The compressor fills the space up to its length. An empty space is filled up to the maximum block size, the
    /// compressor resizes it as needed.
    fn get_next_space(&mut self) -> alloc::vec::Vec<u8>;
    /// Get a reference to the last commited space
    fn get_last_space(&mut self) -> &[u8];
    /// Commit a space to the matcher so it can be matched against
    fn commit_space(&mut self, space: alloc::vec::Vec<u8>);
    /// Just process the data in the last commited space for future matching
    fn skip_matching(&mut self);
    /// Process the data in the last commited space for future matching AND generate matches for the data
    fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>));
    /// Reset this matcher so it can be used for the next new frame
    fn reset(&mut self, level: CompressionLevel);
    /// The total size of the data of the next frame, if it is known. Called after `reset` and before
    /// `load_dictionary`, `window_size` and the first `commit_space`. Matchers can use it to size their window
    /// and tables to the input; the default implementation ignores it.
    fn source_size_hint(&mut self, _size: u64) {}
    /// The size of the window the decoder will need to execute all sequences produced by this matcher
    ///
    /// May change after a call to reset with a different compression level or to `source_size_hint`
    fn window_size(&self) -> u64;
    /// Load the content of a dictionary that is used for the next frame. Called right after `reset`,
    /// before the first space is committed.
    ///
    /// The content acts as data that precedes the first block. Sequences may reference it like data of the
    /// frame, but only as long as the total amount of data in the frame does not exceed [`Matcher::window_size`]
    /// (this is the limit the Zstd specification places on dictionary references). Matchers that do not
    /// support dictionaries can ignore this, the default implementation does nothing.
    fn load_dictionary(&mut self, _content: &[u8]) {}
}

#[derive(PartialEq, Eq, Debug)]
/// Sequences that a [`Matcher`] can produce
pub enum Sequence<'data> {
    /// Is encoded as a sequence for the decoder sequence execution.
    ///
    /// First the literals will be copied to the decoded data,
    /// then `match_len` bytes are copied from `offset` bytes back in the decoded data
    Triple {
        literals: &'data [u8],
        offset: usize,
        match_len: usize,
    },
    /// This is returned as the last sequence in a block
    ///
    /// These literals will just be copied at the end of the sequence execution by the decoder
    Literals { literals: &'data [u8] },
}
