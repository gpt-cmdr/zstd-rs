use crate::{
    common::MAX_BLOCK_SIZE,
    encoding::{
        block_header::BlockHeader, blocks::compress_block, frame_compressor::CompressState, Matcher,
    },
};
use alloc::vec::Vec;

/// Compresses a single block using the matcher in `state`.
///
/// This is shared by all compression levels that actually compress data. The level only influences
/// the behaviour of the [`Matcher`] (which has been set up by [`Matcher::reset`]).
///
/// If the whole block consists of the same byte it is encoded as an RLE block. Otherwise it is encoded as a
/// compressed block, unless that does not shrink the block, in which case a raw block is emitted.
///
/// # Parameters
/// - `state`: [`CompressState`] so the compressor can refer to data before
///   the start of this block
/// - `last_block`: Whether or not this block is going to be the last block in the frame
///   (needed because this info is written into the block header)
/// - `uncompressed_data`: A block's worth of uncompressed data, taken from the
///   larger input
/// - `output`: As `uncompressed_data` is compressed, it's appended to `output`.
#[inline]
pub fn compress_matched_block<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    uncompressed_data: Vec<u8>,
    output: &mut Vec<u8>,
) {
    state.matcher.commit_space(uncompressed_data);
    compress_committed_block(state, last_block, output)
}

/// As [`compress_matched_block`], for a block that was committed to the matcher already: its last space.
pub(crate) fn compress_committed_block<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    output: &mut Vec<u8>,
) {
    let block = state.matcher.get_last_space();
    let block_size = block.len() as u32;
    // First check to see if run length encoding can be used for the entire block
    if block.iter().all(|x| block[0].eq(x)) {
        let rle_byte = block[0];
        state.matcher.skip_matching();
        let header = BlockHeader {
            last_block,
            block_type: crate::blocks::block::BlockType::RLE,
            block_size,
        };
        // Write the header, then the block
        header.serialize(output);
        output.push(rle_byte);
    } else {
        // Compress as a standard compressed block, right behind a placeholder for its header
        let header_at = output.len();
        output.extend_from_slice(&[0; 3]);
        let entropy = compress_block(state, output);
        let compressed_size = output.len() - header_at - 3;
        // If compression does not shrink the block, store it raw instead.
        // Also preserve the format guard that compressed blocks must not
        // exceed the maximum block size.
        if compressed_size >= block_size as usize || compressed_size > MAX_BLOCK_SIZE as usize {
            output.truncate(header_at);
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend_from_slice(state.matcher.get_last_space());
        } else {
            // Only now do the sequences and tables of this block reach the decoder
            entropy.commit(state);
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Compressed,
                block_size: compressed_size as u32,
            };
            let mut encoded = Vec::with_capacity(3);
            header.serialize(&mut encoded);
            output[header_at..header_at + 3].copy_from_slice(&encoded);
        }
    }
}
