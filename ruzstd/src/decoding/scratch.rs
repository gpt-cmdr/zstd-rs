//! Structures that wrap around various decoders to make decoding easier.

use super::decode_buffer::DecodeBuffer;
use super::direct::BlockScratch;
use crate::decoding::dictionary::Dictionary;
use crate::fse::FSETable;
use crate::huff0::HuffmanTable;
use alloc::vec::Vec;

use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};

/// What the streaming decoder keeps while it decodes a frame.
pub struct DecoderScratch {
    /// The tables and offsets the blocks of the frame keep from one to the next.
    pub(crate) block: BlockScratch,
    /// The window, into which the blocks are decoded.
    pub buffer: DecodeBuffer,
    /// The compressed block being decoded, as read from the source.
    pub block_content_buffer: Vec<u8>,
}

impl DecoderScratch {
    pub fn new(window_size: usize, content_size: Option<u64>) -> DecoderScratch {
        let mut buffer = DecodeBuffer::new(window_size);
        buffer.reset(window_size, content_size);
        DecoderScratch {
            block: BlockScratch::new(),
            buffer,
            block_content_buffer: Vec::new(),
        }
    }

    /// The state a frame without a dictionary starts with, keeping the allocations.
    pub fn reset(&mut self, window_size: usize, content_size: Option<u64>) {
        self.block_content_buffer.clear();
        self.buffer.reset(window_size, content_size);
        self.block.reset();
    }

    pub fn init_from_dict(&mut self, dict: &Dictionary) {
        self.block.init_from_dict(dict);
        self.buffer.dict_content.clear();
        self.buffer
            .dict_content
            .extend_from_slice(&dict.dict_content);
    }
}

pub struct HuffmanScratch {
    pub table: HuffmanTable,
}

impl HuffmanScratch {
    pub fn new() -> HuffmanScratch {
        HuffmanScratch {
            table: HuffmanTable::new(),
        }
    }
}

impl Default for HuffmanScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub struct FSEScratch {
    pub offsets: FSETable,
    pub of_rle: Option<u8>,
    pub literal_lengths: FSETable,
    pub ll_rle: Option<u8>,
    pub match_lengths: FSETable,
    pub ml_rle: Option<u8>,
}

impl FSEScratch {
    pub fn new() -> FSEScratch {
        FSEScratch {
            offsets: FSETable::new(MAX_OFFSET_CODE),
            of_rle: None,
            literal_lengths: FSETable::new(MAX_LITERAL_LENGTH_CODE),
            ll_rle: None,
            match_lengths: FSETable::new(MAX_MATCH_LENGTH_CODE),
            ml_rle: None,
        }
    }

    pub fn reinit_from(&mut self, other: &Self) {
        self.offsets.reinit_from(&other.offsets);
        self.literal_lengths.reinit_from(&other.literal_lengths);
        self.match_lengths.reinit_from(&other.match_lengths);
        self.of_rle = other.of_rle;
        self.ll_rle = other.ll_rle;
        self.ml_rle = other.ml_rle;
    }
}

impl Default for FSEScratch {
    fn default() -> Self {
        Self::new()
    }
}
