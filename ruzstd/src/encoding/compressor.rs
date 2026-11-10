//! Compression of whole buffers with a compressor that is kept between calls.

use alloc::vec::Vec;

use super::block_header::BlockHeader;
use super::frame_compressor::{CompressState, FseTables, INITIAL_OFFSET_HISTORY};
use super::frame_header::FrameHeader;
use super::levels::compress_committed_block;
use super::{CompressionLevel, MatchGeneratorDriver, Matcher};
use crate::blocks::block::BlockType;
use crate::common::MAX_BLOCK_SIZE;

/// A compressor of whole buffers, one frame per buffer, that keeps its tables and buffers between calls.
///
/// Where [`compress_to_vec`](super::compress_to_vec) allocates the tables of the matcher for every call (several
/// megabytes at the higher levels, which the operating system hands out page by page), a `Compressor` allocates
/// them once: use one per thread to compress many buffers, such as the tiles of an image.
///
/// The size of the data is known up front, so every frame states its content size, as the frames of the
/// reference implementation do when it is given a whole buffer (`ZSTD_compress`), and the window and tables are
/// sized to the data. The frames carry no checksum.
///
/// ```rust
/// use zstd_rust::encoding::{CompressionLevel, Compressor};
/// let mut compressor = Compressor::new(CompressionLevel::Default);
/// let first = compressor.compress(b"some data, some data, some data");
/// let second = compressor.compress(b"other data, other data, other data");
/// # assert!(first.len() < 31 && second.len() < 34);
/// ```
pub struct Compressor {
    level: CompressionLevel,
    state: CompressState<MatchGeneratorDriver>,
}

impl Compressor {
    /// A compressor for `level`. Nothing is allocated before the first buffer is compressed.
    pub fn new(level: CompressionLevel) -> Self {
        Self {
            level,
            state: CompressState {
                matcher: MatchGeneratorDriver::new(level),
                last_huff_table: None,
                fse_tables: FseTables::new(),
                offset_history: INITIAL_OFFSET_HISTORY,
                literals_buf: Vec::new(),
                sequences_buf: Vec::new(),
            },
        }
    }

    /// The level of the next frames.
    pub fn compression_level(&self) -> CompressionLevel {
        self.level
    }

    /// Sets the level of the next frames. The tables are allocated anew if the level needs other ones.
    pub fn set_compression_level(&mut self, level: CompressionLevel) {
        self.level = level;
    }

    /// Compresses `data` into a frame.
    ///
    /// The frame is returned in a buffer with room for incompressible data (the size of `data` and a few bytes
    /// per block), so that it never has to be moved while it grows: memory the operating system only hands
    /// out as it is written to. Call `shrink_to_fit` on frames that are kept, or compress with
    /// [`compress_into`](Self::compress_into) into a buffer that is used again, which also saves the
    /// allocation.
    pub fn compress(&mut self, data: &[u8]) -> Vec<u8> {
        let blocks = data.len() / MAX_BLOCK_SIZE as usize + 1;
        let mut output = Vec::with_capacity(data.len() + 3 * blocks + 32);
        self.compress_into(data, &mut output);
        output
    }

    /// Compresses `data` into a frame appended to `output`.
    pub fn compress_into(&mut self, data: &[u8], output: &mut Vec<u8>) {
        let state = &mut self.state;
        state.matcher.reset(self.level);
        state.last_huff_table = None;
        state.fse_tables.reset();
        state.offset_history = INITIAL_OFFSET_HISTORY;
        state.matcher.source_size_hint(data.len() as u64);

        // As the reference implementation: data within the window is a single segment, whose window is its size
        let window_size = state.matcher.window_size();
        FrameHeader {
            frame_content_size: Some(data.len() as u64),
            single_segment: data.len() as u64 <= window_size,
            content_checksum: false,
            dictionary_id: None,
            window_size: Some(window_size),
        }
        .serialize(output);

        if data.is_empty() {
            BlockHeader {
                last_block: true,
                block_type: BlockType::Raw,
                block_size: 0,
            }
            .serialize(output);
            return;
        }
        let mut blocks = data.chunks(MAX_BLOCK_SIZE as usize).peekable();
        while let Some(block) = blocks.next() {
            let last_block = blocks.peek().is_none();
            if let CompressionLevel::Uncompressed = self.level {
                BlockHeader {
                    last_block,
                    block_type: BlockType::Raw,
                    block_size: block.len() as u32,
                }
                .serialize(output);
                output.extend_from_slice(block);
            } else {
                state.matcher.commit_slice(block);
                compress_committed_block(state, last_block, output);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Compressor;
    use crate::decoding::FrameDecoder;
    use crate::encoding::CompressionLevel;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Compressible data: few symbols, and runs repeated from further back.
    fn data(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        let mut out: Vec<u8> = Vec::with_capacity(len);
        while out.len() < len {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if out.len() > 100 && (state >> 20) & 15 == 0 {
                let from = (state as usize >> 8) % (out.len() - 40);
                for i in 0..40 {
                    out.push(out[from + i]);
                }
            } else {
                out.push((state >> 28) as u8);
            }
        }
        out.truncate(len);
        out
    }

    /// A compressor compresses buffer after buffer, of every size and at every level, to frames that state
    /// their size and decode to the buffer, whatever it compressed before.
    #[test]
    fn frames_of_a_reused_compressor_decode() {
        let levels = [
            CompressionLevel::Uncompressed,
            CompressionLevel::Fastest,
            CompressionLevel::Default,
            CompressionLevel::Better,
            CompressionLevel::Best,
        ];
        let mut decoder = FrameDecoder::new();
        let mut compressor = Compressor::new(CompressionLevel::Fastest);
        for round in 0..3u32 {
            for (l, &level) in levels.iter().enumerate() {
                compressor.set_compression_level(level);
                for &len in &[
                    0usize,
                    1,
                    5,
                    300,
                    70_000,
                    131_072,
                    131_073,
                    1 << 20,
                    3_000_000,
                ] {
                    let input = data(len, round * 31 + l as u32 * 7 + len as u32);
                    let frame = compressor.compress(&input);
                    let header = crate::decoding::frame::read_frame_header(&frame[..])
                        .unwrap()
                        .0;
                    assert_eq!(header.frame_content_size(), len as u64);
                    assert!(!header.descriptor.content_checksum_flag());
                    let mut output = vec![0; len];
                    assert_eq!(decoder.decode_all(&frame, &mut output).unwrap(), len);
                    assert!(output == input, "level {:?} length {}", level, len);
                    #[cfg(feature = "std")]
                    assert!(zstd::bulk::decompress(&frame, len).unwrap() == input);
                    if len >= 70_000 && l > 0 {
                        assert!(frame.len() < len, "level {:?} length {}", level, len);
                    }
                }
            }
        }
    }

    #[test]
    fn appends_to_the_output() {
        let input = data(200_000, 9);
        let mut compressor = Compressor::new(CompressionLevel::Default);
        let mut output = vec![1, 2, 3];
        compressor.compress_into(&input, &mut output);
        assert_eq!(output[..3], [1, 2, 3]);
        assert_eq!(output[3..], compressor.compress(&input)[..]);
    }
}
