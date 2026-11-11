//! Decoding of compressed blocks into a slice, and of whole frames from a slice into a slice.
//!
//! [`BlockScratch::decode_block`] decodes a block to a position of a slice whose bytes before
//! that position are the window: sequences are executed a few at a time, as they are decoded. The
//! layout follows the reference decoder's (`zstd_decompress_block.c`): sequence tables hold the
//! base value and extra bits of each state, and the bitstream is refilled at most twice per
//! sequence. Both decoders use it:
//!
//! * [`FrameDecoder::decode_all`](super::FrameDecoder::decode_all), for frames without a
//!   dictionary, through [`DirectScratch::decode_frame`]: the output slice is its own window and
//!   blocks are read in place from the input.
//! * The streaming decoder, whose window is the
//!   [`DecodeBuffer`](super::decode_buffer::DecodeBuffer). Its matches may also reach bytes that
//!   are not in the slice a block is decoded to: an older part of the window and the content of a
//!   dictionary, the [`History`].

use super::frame;
use crate::bit_io::BackBits;
use crate::blocks::block::BlockType;
use crate::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
use crate::blocks::sequence_section::{ModeType, SequencesHeader};
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::common::cpu;
use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::dictionary::Dictionary;
use crate::decoding::errors::{
    DecodeBlockContentError, DecodeBufferError, DecodeSequenceError, DecompressBlockError,
    DecompressLiteralsError, ExecuteSequencesError, FSEDecoderError, FrameDecoderError,
};
use crate::decoding::sequence_section_decoder::{
    LITERALS_LENGTH_DEFAULT_DISTRIBUTION, LL_DEFAULT_ACC_LOG, LL_MAX_LOG,
    MATCH_LENGTH_DEFAULT_DISTRIBUTION, ML_DEFAULT_ACC_LOG, ML_MAX_LOG, OFFSET_DEFAULT_DISTRIBUTION,
    OF_DEFAULT_ACC_LOG, OF_MAX_LOG,
};
use crate::fse::FSETable;
use crate::huff0::HuffmanTable;
use crate::io::{Error, ErrorKind};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::convert::TryFrom;

/// A state of a sequence table: what the state decodes to and how to reach the next one.
#[derive(Clone, Copy, Default)]
struct SeqEntry {
    /// The value of the state's code, to which `extra_bits` bits of the stream are added.
    base: u32,
    /// The first of the states that may follow, to which `num_bits` bits are added.
    next: u16,
    extra_bits: BitCount,
    num_bits: BitCount,
}

macro_rules! bit_counts {
    ($($count:ident)*) => {
        /// A number of bits from 0 to 63.
        ///
        /// It is an enum, not a `u8`, for what it tells the compiler: what is read from a table
        /// is then known to be less than 64, so a mask of that many bits is one instruction
        /// (`bzhi`) and not two, with nothing to guard a shift by 64 or more against.
        #[derive(Clone, Copy, Default)]
        #[repr(u8)]
        enum BitCount {
            #[default]
            $($count),*
        }

        impl BitCount {
            const ALL: [BitCount; 64] = [$(BitCount::$count),*];
        }
    };
}

bit_counts!(B0 B1 B2 B3 B4 B5 B6 B7 B8 B9 B10 B11 B12 B13 B14 B15 B16 B17 B18 B19 B20 B21 B22 B23 B24 B25 B26 B27 B28 B29 B30 B31 B32 B33 B34 B35 B36 B37 B38 B39 B40 B41 B42 B43 B44 B45 B46 B47 B48 B49 B50 B51 B52 B53 B54 B55 B56 B57 B58 B59 B60 B61 B62 B63);

impl BitCount {
    /// `bits` as a count; no table has a count of 64 or more.
    fn new(bits: u8) -> Self {
        Self::ALL[usize::from(bits) % 64]
    }

    #[inline(always)]
    fn get(self) -> u32 {
        self as u32
    }
}

const SEQ_TABLE_SIZE: usize = 512;

/// The table of literal lengths, match lengths or offsets of a block.
struct SeqTable {
    entries: [SeqEntry; SEQ_TABLE_SIZE],
    /// The bits a state takes of the stream: the extra bits of its code and the bits that lead
    /// to the next state. The sum is here so that it is ready as soon as the entry is.
    spans: [u8; SEQ_TABLE_SIZE],
    accuracy_log: u8,
    /// Set once a block has defined the table: a block may only repeat a defined table.
    defined: bool,
}

impl SeqTable {
    fn new() -> Self {
        SeqTable {
            entries: [SeqEntry::default(); SEQ_TABLE_SIZE],
            spans: [0; SEQ_TABLE_SIZE],
            accuracy_log: 0,
            defined: false,
        }
    }

    /// The table of an FSE table, with the base and extra bits of each code.
    fn set_fse(&mut self, table: &FSETable, codes: &dyn Fn(u8) -> (u32, u8)) {
        let entries = self.entries.iter_mut().zip(self.spans.iter_mut());
        for ((entry, span), state) in entries.zip(&table.decode) {
            let (base, extra_bits) = codes(state.symbol);
            *entry = SeqEntry {
                base,
                next: state.base_line as u16,
                extra_bits: BitCount::new(extra_bits),
                num_bits: BitCount::new(state.num_bits),
            };
            *span = extra_bits + state.num_bits;
        }
        self.accuracy_log = table.accuracy_log;
        self.defined = true;
    }

    /// The table of a distribution of codes, given as the probabilities and the accuracy log of
    /// an FSE table, with the base and extra bits of each code: what [`SeqTable::set_fse`] makes
    /// of the FSE table of the distribution, without building that table first. `false`, and
    /// the table is not to be used, when the probabilities are not those of a table of that size.
    ///
    /// The states are laid out as the format has it (`FSETable::build_decoding_table` has the
    /// comments): the symbols of probability "less than one" from the top down, the others
    /// spread over the rest in steps; then the states of a symbol of probability `p`, in the
    /// order of the table, are numbered from `p` to `2 p - 1`, and the number gives the bits a
    /// state reads and the state it leads to.
    fn set_distribution(
        &mut self,
        probabilities: &[i32],
        accuracy_log: u8,
        codes: impl Fn(u8) -> (u32, u8),
    ) -> bool {
        // The step of the spreading visits every state for these sizes.
        if !(5..=9).contains(&accuracy_log) || probabilities.len() > 256 {
            return false;
        }
        let size = 1usize << accuracy_log;
        let mut symbols = [0u8; SEQ_TABLE_SIZE];
        let symbols = &mut symbols[..size];
        // The number of the next state of each symbol
        let mut numbers = [0u32; 256];
        let mut high = size;
        let mut spread = 0;
        for (symbol, &probability) in probabilities.iter().enumerate() {
            if probability == -1 {
                let Some(below) = high.checked_sub(1) else {
                    return false;
                };
                high = below;
                symbols[high] = symbol as u8;
            } else if probability > 0 {
                numbers[symbol] = probability as u32;
                spread += probability as usize;
            }
        }
        if spread != high {
            return false;
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mut position = 0;
        for (symbol, &count) in numbers.iter().enumerate() {
            for _ in 0..count {
                symbols[position] = symbol as u8;
                loop {
                    position = (position + step) & (size - 1);
                    if position < high {
                        break;
                    }
                }
            }
        }

        // What the states of a symbol have in common, worked out once for each symbol
        let mut of_symbol = [SeqEntry::default(); 256];
        for (symbol, entry) in of_symbol.iter_mut().enumerate().take(probabilities.len()) {
            let (base, extra_bits) = codes(symbol as u8);
            entry.base = base;
            entry.extra_bits = BitCount::new(extra_bits);
        }
        let entries = self.entries.iter_mut().zip(self.spans.iter_mut());
        for (index, ((entry, span), &symbol)) in entries.zip(symbols.iter()).enumerate() {
            let symbol = usize::from(symbol);
            let (next, num_bits) = if index < high {
                let number = numbers[symbol];
                numbers[symbol] += 1;
                let bits = u32::from(accuracy_log).saturating_sub(number.max(1).ilog2());
                ((number << bits).wrapping_sub(size as u32), bits as u8)
            } else {
                (0, accuracy_log)
            };
            *entry = SeqEntry {
                next: next as u16,
                num_bits: BitCount::new(num_bits),
                ..of_symbol[symbol]
            };
            *span = of_symbol[symbol].extra_bits.get() as u8 + num_bits;
        }
        self.accuracy_log = accuracy_log;
        self.defined = true;
        true
    }

    /// The table of a single code: one state, reached with no bits.
    fn set_rle(&mut self, (base, extra_bits): (u32, u8)) {
        self.entries[0] = SeqEntry {
            base,
            next: 0,
            extra_bits: BitCount::new(extra_bits),
            num_bits: BitCount::B0,
        };
        self.spans[0] = extra_bits;
        self.accuracy_log = 0;
        self.defined = true;
    }

    fn copy_from(&mut self, other: &SeqTable) {
        let states = 1usize << other.accuracy_log;
        self.entries[..states].copy_from_slice(&other.entries[..states]);
        self.spans[..states].copy_from_slice(&other.spans[..states]);
        self.accuracy_log = other.accuracy_log;
        self.defined = true;
    }

    /// The table a dictionary starts a frame with: that of its FSE table or of its single code,
    /// and none when the dictionary has neither (a dictionary of content only).
    fn set_from_dict(
        &mut self,
        table: &FSETable,
        rle: Option<u8>,
        codes: &dyn Fn(u8) -> (u32, u8),
    ) {
        if let Some(code) = rle {
            self.set_rle(codes(code));
        } else if table.accuracy_log != 0 && table.decode.len() == 1usize << table.accuracy_log {
            self.set_fse(table, codes);
        } else {
            self.defined = false;
        }
    }
}

/// The base and extra bits of a literal length code.
fn ll_code(code: u8) -> (u32, u8) {
    const BASE: [u32; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    const BITS: [u8; 36] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ];
    let code = usize::from(code.min(MAX_LITERAL_LENGTH_CODE));
    (BASE[code], BITS[code])
}

/// The base and extra bits of a match length code.
fn ml_code(code: u8) -> (u32, u8) {
    const BASE: [u32; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ];
    const BITS: [u8; 53] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ];
    let code = usize::from(code.min(MAX_MATCH_LENGTH_CODE));
    (BASE[code], BITS[code])
}

/// The base and extra bits of an offset code.
fn of_code(code: u8) -> (u32, u8) {
    let code = code.min(MAX_OFFSET_CODE);
    (1 << code, code)
}

/// The tables of literal lengths, offsets and match lengths: those of a block, and the predefined
/// ones (built once). They are one value so that the loop that decodes sequences reaches all
/// three through one pointer.
struct SeqTables {
    ll: SeqTable,
    of: SeqTable,
    ml: SeqTable,
}

impl SeqTables {
    fn new() -> Self {
        SeqTables {
            ll: SeqTable::new(),
            of: SeqTable::new(),
            ml: SeqTable::new(),
        }
    }
}

/// What the blocks of a frame keep from one to the next: the tables a block may repeat and the
/// last three offsets. The allocations are kept from frame to frame.
pub(crate) struct BlockScratch {
    huf: HuffmanTable,
    fse: FSETable,
    tables: SeqTables,
    predefined: Option<Box<SeqTables>>,
    literals: Vec<u8>,
    offset_hist: [u32; 3],
}

/// What [`FrameDecoder::decode_all`](super::FrameDecoder::decode_all) keeps between frames.
pub(crate) struct DirectScratch {
    block: BlockScratch,
    /// The checksum of the last frame decoded, if it has one.
    pub(crate) checksum: Option<FrameChecksum>,
}

/// The bytes before those of the slice a block is decoded to that its matches may reach. From
/// the newest to the oldest: the slice up to the position of the block, `window`, `dict`.
pub(crate) struct History<'a> {
    /// The older part of the window, when the window is not in one piece.
    pub(crate) window: &'a [u8],
    /// The content of the dictionary of the frame.
    pub(crate) dict: &'a [u8],
    /// Matches that start before this position of the slice may reach into the dictionary: a
    /// frame may refer to its dictionary until it has decoded more than its window size.
    pub(crate) dict_end: usize,
}

impl History<'_> {
    /// Nothing but the slice itself.
    pub(crate) const NONE: History<'static> = History {
        window: &[],
        dict: &[],
        dict_end: 0,
    };
}

/// Why a block was not decoded.
pub(crate) enum BlockError {
    /// The slice ends before the block does.
    TargetTooSmall,
    Failed(DecompressBlockError),
}

/// The checksum a frame ends with, and the one of the content decoded (with the `hash` feature).
#[derive(Clone, Copy)]
pub(crate) struct FrameChecksum {
    pub(crate) from_data: u32,
    #[cfg_attr(not(feature = "hash"), allow(dead_code))]
    pub(crate) calculated: Option<u32>,
}

/// Why a frame was not decoded by this path.
pub(crate) enum DirectError {
    /// The frame needs what only the streaming decoder has (a dictionary).
    Unsupported,
    Failed(FrameDecoderError),
}

impl From<FrameDecoderError> for DirectError {
    fn from(e: FrameDecoderError) -> Self {
        DirectError::Failed(e)
    }
}

impl From<BlockError> for DirectError {
    fn from(e: BlockError) -> Self {
        DirectError::Failed(match e {
            BlockError::TargetTooSmall => FrameDecoderError::TargetTooSmall,
            BlockError::Failed(e) => FrameDecoderError::FailedToReadBlockBody(
                DecodeBlockContentError::DecompressBlockError(e),
            ),
        })
    }
}

fn block_error(e: impl Into<DecompressBlockError>) -> BlockError {
    BlockError::Failed(e.into())
}

fn truncated(step: BlockType) -> DirectError {
    DirectError::Failed(FrameDecoderError::FailedToReadBlockBody(
        DecodeBlockContentError::ReadError {
            step,
            source: Error::from(ErrorKind::UnexpectedEof),
        },
    ))
}

impl DirectScratch {
    pub(crate) fn new() -> Self {
        DirectScratch {
            block: BlockScratch::new(),
            checksum: None,
        }
    }

    /// Decodes the frame at the start of `input` to the start of `output`: the bytes of `input`
    /// the frame takes and the bytes written.
    ///
    /// `input` starts after any skippable frame. Nothing of the scratch but its allocations
    /// carries over from another frame.
    pub(crate) fn decode_frame(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        max_window_size: u64,
    ) -> Result<(usize, usize), DirectError> {
        let mut source = input;
        let (header, _) = frame::read_frame_header(&mut source)
            .map_err(FrameDecoderError::ReadFrameHeaderError)?;
        if header.dictionary_id().is_some() {
            return Err(DirectError::Unsupported);
        }
        let window_size = header
            .window_size()
            .map_err(FrameDecoderError::FrameHeaderError)?;
        if window_size > max_window_size {
            return Err(FrameDecoderError::WindowSizeTooBig {
                requested: window_size,
                max: max_window_size,
            }
            .into());
        }
        let max_block_len = usize::try_from(window_size)
            .unwrap_or(usize::MAX)
            .min(MAX_BLOCK_SIZE as usize);

        self.block.reset();

        let mut pos = 0;
        loop {
            let Some((head, rest)) = source.split_first_chunk::<3>() else {
                return Err(FrameDecoderError::FailedToReadBlockHeader(
                    crate::decoding::errors::BlockHeaderReadError::ReadError(Error::from(
                        ErrorKind::UnexpectedEof,
                    )),
                )
                .into());
            };
            source = rest;
            let last_block = head[0] & 1 == 1;
            let size = (usize::from(head[0]) >> 3)
                | (usize::from(head[1]) << 5)
                | (usize::from(head[2]) << 13);
            if size > MAX_BLOCK_SIZE as usize {
                return Err(FrameDecoderError::FailedToReadBlockHeader(
                    crate::decoding::errors::BlockHeaderReadError::BlockSizeError(
                        crate::decoding::errors::BlockSizeError::BlockSizeTooLarge {
                            size: size as u32,
                        },
                    ),
                )
                .into());
            }
            match (head[0] >> 1) & 3 {
                0 => {
                    check_block_len(size, max_block_len)?;
                    let Some((raw, rest)) = source.split_at_checked(size) else {
                        return Err(truncated(BlockType::Raw));
                    };
                    source = rest;
                    output
                        .get_mut(pos..pos + size)
                        .ok_or(FrameDecoderError::TargetTooSmall)?
                        .copy_from_slice(raw);
                    pos += size;
                }
                1 => {
                    check_block_len(size, max_block_len)?;
                    let Some((&byte, rest)) = source.split_first() else {
                        return Err(truncated(BlockType::RLE));
                    };
                    source = rest;
                    output
                        .get_mut(pos..pos + size)
                        .ok_or(FrameDecoderError::TargetTooSmall)?
                        .fill(byte);
                    pos += size;
                }
                2 => {
                    let Some((block, rest)) = source.split_at_checked(size) else {
                        return Err(truncated(BlockType::Compressed));
                    };
                    source = rest;
                    pos = self.block.decode_block(
                        block,
                        output,
                        pos,
                        max_block_len,
                        &History::NONE,
                    )?;
                }
                _ => {
                    return Err(FrameDecoderError::FailedToReadBlockHeader(
                        crate::decoding::errors::BlockHeaderReadError::FoundReservedBlock,
                    )
                    .into());
                }
            }
            if last_block {
                break;
            }
        }
        let mut checksum = None;
        if header.descriptor.content_checksum_flag() {
            let Some((bytes, rest)) = source.split_first_chunk::<4>() else {
                return Err(FrameDecoderError::FailedToReadChecksum(Error::from(
                    ErrorKind::UnexpectedEof,
                ))
                .into());
            };
            source = rest;
            let expected = u32::from_le_bytes(*bytes);
            // The checksum is the low half of the XXH64 of the content
            #[cfg(feature = "hash")]
            let calculated = {
                let calculated = twox_hash::XxHash64::oneshot(0, &output[..pos]) as u32;
                if calculated != expected {
                    return Err(FrameDecoderError::ChecksumMismatch {
                        expected,
                        calculated,
                    }
                    .into());
                }
                Some(calculated)
            };
            #[cfg(not(feature = "hash"))]
            let calculated = None;
            checksum = Some(FrameChecksum {
                from_data: expected,
                calculated,
            });
        }
        self.checksum = checksum;
        Ok((input.len() - source.len(), pos))
    }
}

impl BlockScratch {
    pub(crate) fn new() -> Self {
        BlockScratch {
            huf: HuffmanTable::new(),
            fse: FSETable::new(255),
            tables: SeqTables::new(),
            predefined: None,
            literals: Vec::new(),
            offset_hist: [1, 4, 8],
        }
    }

    /// The state a frame without a dictionary starts with.
    pub(crate) fn reset(&mut self) {
        self.huf.reset();
        self.tables.ll.defined = false;
        self.tables.of.defined = false;
        self.tables.ml.defined = false;
        self.offset_hist = [1, 4, 8];
    }

    /// The state a frame with `dict` starts with: its tables, which the first block may repeat,
    /// and its offsets.
    pub(crate) fn init_from_dict(&mut self, dict: &Dictionary) {
        self.huf.reinit_from(&dict.huf.table);
        self.tables
            .ll
            .set_from_dict(&dict.fse.literal_lengths, dict.fse.ll_rle, &ll_code);
        self.tables
            .of
            .set_from_dict(&dict.fse.offsets, dict.fse.of_rle, &of_code);
        self.tables
            .ml
            .set_from_dict(&dict.fse.match_lengths, dict.fse.ml_rle, &ml_code);
        self.offset_hist = dict.offset_hist;
    }

    /// Decodes a compressed block to `output[pos..]`: the position after the block.
    ///
    /// The `pos` bytes before are the window, with `history` before them. The block decodes to
    /// no more than `max_block_len` bytes. Up to 32 bytes after those of the block may be written
    /// to, where `output` has them.
    pub(crate) fn decode_block(
        &mut self,
        block: &[u8],
        output: &mut [u8],
        pos: usize,
        max_block_len: usize,
        history: &History<'_>,
    ) -> Result<usize, BlockError> {
        let mut section = LiteralsSection::new();
        let header_len = usize::from(section.parse_from_header(block).map_err(block_error)?);
        let raw = block.get(header_len..).unwrap_or_default();
        let regenerated = section.regenerated_size as usize;
        check_block_len(regenerated, max_block_len)?;
        let literals_len = match (section.compressed_size, &section.ls_type) {
            (Some(size), _) => size as usize,
            (None, LiteralsSectionType::RLE) => 1,
            (None, _) => regenerated,
        };
        let Some((literals_source, raw)) = raw.split_at_checked(literals_len) else {
            return Err(block_error(DecompressBlockError::MalformedSectionHeader {
                expected_len: literals_len,
                remaining_bytes: raw.len(),
            }));
        };
        let literals: &[u8] = match section.ls_type {
            LiteralsSectionType::Raw => literals_source,
            LiteralsSectionType::RLE => {
                let literals = literals_buffer(&mut self.literals, regenerated);
                literals.fill(literals_source[0]);
                literals
            }
            LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
                let literals = literals_buffer(&mut self.literals, regenerated);
                decode_huffman(&section, &mut self.huf, literals_source, literals)
                    .map_err(block_error)?;
                literals
            }
        };

        let mut sequences = SequencesHeader::new();
        let header_len = usize::from(sequences.parse_from_header(raw).map_err(block_error)?);
        let raw = raw.get(header_len..).unwrap_or_default();
        if sequences.num_sequences == 0 {
            if !raw.is_empty() {
                return Err(block_error(DecodeSequenceError::ExtraBits {
                    bits_remaining: raw.len() as isize * 8,
                }));
            }
            output
                .get_mut(pos..pos + literals.len())
                .ok_or(BlockError::TargetTooSmall)?
                .copy_from_slice(literals);
            return Ok(pos + literals.len());
        }

        let tables_len = update_tables(
            &sequences,
            raw,
            &mut self.fse,
            &mut self.tables,
            &mut self.predefined,
        )
        .map_err(block_error)?;
        execute(
            raw.get(tables_len..).unwrap_or_default(),
            sequences.num_sequences as usize,
            &self.tables,
            &mut self.offset_hist,
            literals,
            output,
            pos,
            max_block_len,
            history,
        )
    }
}

/// The first `len` bytes of `buffer`, to be written before they are read: the buffer grows to
/// the longest literals of the frames it has seen and is not cleared for those of a block,
/// which are all decoded or an error.
fn literals_buffer(buffer: &mut Vec<u8>, len: usize) -> &mut [u8] {
    if buffer.len() < len {
        buffer.resize(len, 0);
    }
    &mut buffer[..len]
}

fn check_block_len(at_least: usize, max: usize) -> Result<(), BlockError> {
    if at_least > max {
        return Err(block_error(
            DecompressBlockError::DecompressedSizeTooLarge { at_least, max },
        ));
    }
    Ok(())
}

/// Decodes the Huffman-coded literals of `section` into exactly `target`.
fn decode_huffman(
    section: &LiteralsSection,
    table: &mut HuffmanTable,
    source: &[u8],
    target: &mut [u8],
) -> Result<(), DecompressLiteralsError> {
    use DecompressLiteralsError as err;
    let num_streams = section.num_streams.ok_or(err::MissingNumStreams)?;
    let mut source = source;
    match section.ls_type {
        LiteralsSectionType::Compressed => {
            let used = table.build_decoder(source)? as usize;
            source = source.get(used..).ok_or(err::MissingBytesForLiterals {
                got: source.len(),
                needed: used,
            })?;
        }
        _ if table.max_num_bits == 0 => return Err(err::UninitializedHuffmanTable),
        _ => {}
    }
    let expected = target.len();
    let mismatch = || err::DecodedLiteralCountMismatch {
        decoded: 0,
        expected,
    };
    if num_streams == 4 {
        let Some((jumps, source)) = source.split_first_chunk::<6>() else {
            return Err(err::MissingBytesForJumpHeader { got: source.len() });
        };
        let jump1 = usize::from(u16::from_le_bytes([jumps[0], jumps[1]]));
        let jump2 = jump1 + usize::from(u16::from_le_bytes([jumps[2], jumps[3]]));
        let jump3 = jump2 + usize::from(u16::from_le_bytes([jumps[4], jumps[5]]));
        if source.len() < jump3 {
            return Err(err::MissingBytesForLiterals {
                got: source.len(),
                needed: jump3,
            });
        }
        table
            .decode_four_streams(source, [jump1, jump2, jump3], target)
            .ok_or_else(mismatch)
    } else {
        table.decode_stream(source, target).ok_or_else(mismatch)
    }
}

/// Reads the tables of a block's sequences from `source`: the bytes they take.
fn update_tables(
    header: &SequencesHeader,
    source: &[u8],
    fse: &mut FSETable,
    tables: &mut SeqTables,
    predefined: &mut Option<Box<SeqTables>>,
) -> Result<usize, DecodeSequenceError> {
    let modes = header
        .modes
        .ok_or(DecodeSequenceError::MissingCompressionMode)?;
    let predefined = &**predefined.get_or_insert_with(|| {
        let mut tables = Box::new(SeqTables::new());
        let built = tables.ll.set_distribution(
            &LITERALS_LENGTH_DEFAULT_DISTRIBUTION,
            LL_DEFAULT_ACC_LOG,
            ll_code,
        ) && tables.of.set_distribution(
            &OFFSET_DEFAULT_DISTRIBUTION,
            OF_DEFAULT_ACC_LOG,
            of_code,
        ) && tables.ml.set_distribution(
            &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
            ML_DEFAULT_ACC_LOG,
            ml_code,
        );
        assert!(built, "the predefined distributions are valid");
        tables
    });

    let mut read = 0;
    update_table(
        modes.ll_mode(),
        &mut tables.ll,
        &predefined.ll,
        (MAX_LITERAL_LENGTH_CODE, LL_MAX_LOG, ll_code),
        DecodeSequenceError::MissingByteForRleLlTable,
        fse,
        source,
        &mut read,
    )?;
    update_table(
        modes.of_mode(),
        &mut tables.of,
        &predefined.of,
        (MAX_OFFSET_CODE, OF_MAX_LOG, of_code),
        DecodeSequenceError::MissingByteForRleOfTable,
        fse,
        source,
        &mut read,
    )?;
    update_table(
        modes.ml_mode(),
        &mut tables.ml,
        &predefined.ml,
        (MAX_MATCH_LENGTH_CODE, ML_MAX_LOG, ml_code),
        DecodeSequenceError::MissingByteForRleMlTable,
        fse,
        source,
        &mut read,
    )?;
    Ok(read)
}

/// Makes `table` what a block says it is, reading what the mode needs at `source[*read..]`.
/// The codes of the table go up to `max_code`, their base and extra bits are `codes`, and an
/// FSE table of them has an accuracy log of at most `max_log`.
#[allow(clippy::too_many_arguments)]
fn update_table(
    mode: ModeType,
    table: &mut SeqTable,
    default: &SeqTable,
    (max_code, max_log, codes): (u8, u8, impl Fn(u8) -> (u32, u8)),
    missing: DecodeSequenceError,
    fse: &mut FSETable,
    source: &[u8],
    read: &mut usize,
) -> Result<(), DecodeSequenceError> {
    match mode {
        ModeType::FSECompressed => {
            fse.reset();
            fse.set_max_symbol(max_code);
            *read += fse.read_distribution(source.get(*read..).unwrap_or_default(), max_log)?;
            if !table.set_distribution(&fse.symbol_probabilities, fse.accuracy_log, codes) {
                table.defined = false;
                return Err(DecodeSequenceError::FSEDecoderError(
                    FSEDecoderError::TableIsUninitialized,
                ));
            }
        }
        ModeType::RLE => {
            let code = *source.get(*read).ok_or(missing)?;
            if code > max_code {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            *read += 1;
            table.set_rle(codes(code));
        }
        ModeType::Predefined => table.copy_from(default),
        ModeType::Repeat => {
            if !table.defined {
                return Err(DecodeSequenceError::FSEDecoderError(
                    FSEDecoderError::TableIsUninitialized,
                ));
            }
        }
    }
    Ok(())
}

/// Copies the match of `match_len` bytes at `offset` to `output[match_start..]`, where it starts
/// before `output` does: in `history`, from which it may run on into `output`.
#[inline(never)]
pub(crate) fn copy_from_history(
    output: &mut [u8],
    match_start: usize,
    match_len: usize,
    offset: usize,
    history: &History<'_>,
) -> Result<(), ExecuteSequencesError> {
    let target = &mut output[..match_start + match_len];
    // The bytes of the match that are before `output`, if it is long enough
    let mut before = offset - match_start;
    let mut at = match_start;
    if before > history.window.len() {
        let need = before - history.window.len();
        if match_start >= history.dict_end {
            return Err(DecodeBufferError::OffsetTooBig {
                offset,
                buf_len: match_start + history.window.len(),
            }
            .into());
        }
        if need > history.dict.len() {
            return Err(DecodeBufferError::NotEnoughBytesInDictionary {
                got: history.dict.len(),
                need,
            }
            .into());
        }
        let n = need.min(target.len() - at);
        let from = history.dict.len() - need;
        target[at..at + n].copy_from_slice(&history.dict[from..from + n]);
        at += n;
        before -= n;
    }
    if at < target.len() && before > 0 {
        let n = before.min(target.len() - at);
        let from = history.window.len() - before;
        target[at..at + n].copy_from_slice(&history.window[from..from + n]);
        at += n;
    }
    // The rest is at the start of `output`, and may run into what it writes: then the bytes
    // repeat with period `at`, and each copy doubles what is there to copy. `at` is not 0
    // here: the match has bytes before `output`, which were copied above.
    let mut copied = 0;
    while at + copied < target.len() {
        let n = (at + copied).min(target.len() - at - copied);
        target.copy_within(..n, at + copied);
        copied += n;
    }
    Ok(())
}

/// A sequence as the loop that decodes passes it to the loop that copies.
#[derive(Clone, Copy, Default)]
struct Sequence {
    lit_len: u32,
    match_len: u32,
    /// The distance of the match, with the repeated offsets resolved
    offset: u32,
}

/// The sequences decoded before they are executed.
///
/// Decoding and executing are two loops, each over a few sequences, because each then keeps
/// what it works with in registers. One loop doing both has more to keep than x86-64 has
/// registers: the states and the bits of the stream then went through the stack, on the path
/// from one sequence to the next. Against one loop that decodes a sequence the same way, the
/// two were 2 to 6 % faster on the x86-64 processors of the benchmark runners and 3 % slower on
/// their AArch64 one, which has the registers.
const SEQUENCE_BATCH: usize = 16;

/// The last three offsets, the most recent first.
///
/// They are kept in a ring of four so that a new offset, which nearly every sequence has, is one
/// store and a step of the head instead of three values moving along.
#[derive(Clone, Copy)]
struct OffsetHistory {
    ring: [u32; 4],
    /// Where the most recent offset is
    head: usize,
}

impl OffsetHistory {
    fn new(offsets: [u32; 3]) -> Self {
        OffsetHistory {
            ring: [offsets[0], offsets[1], offsets[2], 0],
            head: 0,
        }
    }

    /// The offsets, the most recent first.
    fn offsets(&self) -> [u32; 3] {
        [self.get(0), self.get(1), self.get(2)]
    }

    #[inline(always)]
    fn get(&self, index: usize) -> u32 {
        self.ring[(self.head + index) % 4]
    }

    /// Makes `offset` the most recent: the others move back by one, the third is forgotten.
    #[inline(always)]
    fn push(&mut self, offset: u32) {
        self.head = self.head.wrapping_sub(1) % 4;
        self.ring[self.head] = offset;
    }

    /// The offset of a sequence that repeats one: `index` is 0 to 2 for the three, 3 for the
    /// most recent less one. The offset becomes the most recent.
    #[inline(always)]
    fn repeat(&mut self, index: usize) -> u32 {
        match index {
            0 => self.get(0),
            1 => {
                let (first, second) = (self.get(0), self.get(1));
                self.ring[self.head] = second;
                self.ring[(self.head + 1) % 4] = first;
                second
            }
            // The third comes first and the two before it move back: as a new offset does.
            2 => {
                let offset = self.get(2);
                self.push(offset);
                offset
            }
            _ => {
                let offset = self.get(0).saturating_sub(1);
                self.push(offset);
                offset
            }
        }
    }
}

/// What the loop that decodes sequences keeps from one batch to the next.
#[derive(Clone, Copy)]
struct SequenceDecoder<'s> {
    bits: BackBits<'s>,
    ll_state: usize,
    of_state: usize,
    ml_state: usize,
    hist: OffsetHistory,
}

/// The `n` bits of `word` below its top `skip` bits. A field has at most 32 bits and may have
/// none, and it ends within the word.
///
/// The fields of a sequence are all read from one word, each at the sum of the lengths of those
/// before it, instead of shifting the word after every field: the sums are ready as soon as the
/// table entries are, so the last state of a sequence does not wait for five shifts in a row.
#[inline(always)]
fn field(word: u64, skip: u32, n: u32, bmi2: bool) -> u64 {
    if bmi2 {
        field_above(word, 64u32.wrapping_sub(skip).wrapping_sub(n), n)
    } else {
        // Without `bzhi` a mask costs more than shifting three times: up to the field, by one,
        // and by what is left, so that no shift is by 64 when the field has no bits.
        ((word << (skip & 63)) >> 1) >> (63u32.wrapping_sub(n) & 63)
    }
}

/// The `n` bits of `word` above its low `below` bits: a shift and a mask, which is `bzhi` with
/// BMI2. A field of no bits at the top of the word has 64 below, which shifts by nothing, and
/// the mask leaves nothing.
#[inline(always)]
fn field_above(word: u64, below: u32, n: u32) -> u64 {
    (word >> (below & 63)) & ((1u64 << (n & 63)) - 1)
}

/// Decodes the next sequences of a block into `batch`. `ends_block` tells that the last of them
/// is the last of the block, after which there are no states to read.
#[inline(always)]
fn decode_sequences(
    decoder: &mut SequenceDecoder<'_>,
    tables: &SeqTables,
    batch: &mut [Sequence],
    ends_block: bool,
    bmi2: bool,
) -> Option<()> {
    let SequenceDecoder {
        mut bits,
        mut ll_state,
        mut of_state,
        mut ml_state,
        mut hist,
    } = *decoder;
    let (with_states, last) = batch.split_at_mut(batch.len() - usize::from(ends_block));
    for sequence in with_states {
        *sequence = decode_sequence(
            &mut bits,
            tables,
            (&mut ll_state, &mut of_state, &mut ml_state),
            &mut hist,
            true,
            bmi2,
        )?;
    }
    for sequence in last {
        *sequence = decode_sequence(
            &mut bits,
            tables,
            (&mut ll_state, &mut of_state, &mut ml_state),
            &mut hist,
            false,
            bmi2,
        )?;
    }
    *decoder = SequenceDecoder {
        bits,
        ll_state,
        of_state,
        ml_state,
        hist,
    };
    Some(())
}

/// Decodes a sequence, and the states of the next one if it `has_next`. `None` for a state that
/// is not in its table, which a table does not lead to.
///
/// The table entries are read where they are used, not copied first: what is read early has to
/// be kept until it is used, and there are not the registers for that.
#[inline(always)]
fn decode_sequence(
    bits: &mut BackBits<'_>,
    tables: &SeqTables,
    (ll_state, of_state, ml_state): (&mut usize, &mut usize, &mut usize),
    hist: &mut OffsetHistory,
    has_next: bool,
    bmi2: bool,
) -> Option<Sequence> {
    bits.reload();
    // The states are checked, not masked: a mask would be one more step from a state to the next.
    let ll_entry = tables.ll.entries.get(*ll_state)?;
    let of_entry = tables.of.entries.get(*of_state)?;
    let ml_entry = tables.ml.entries.get(*ml_state)?;
    let of_bits = of_entry.extra_bits.get();
    let ml_bits = ml_entry.extra_bits.get();
    let ll_bits = ll_entry.extra_bits.get();

    // At least 57 bits are ready: the offset takes at most 31, the match length 16.
    let word = bits.bits;
    let offset_value = of_entry.base + field(word, 0, of_bits, bmi2) as u32;
    let match_len = ml_entry.base + field(word, of_bits, ml_bits, bmi2) as u32;
    bits.bits = word << ((of_bits + ml_bits) & 63);
    bits.consumed += of_bits + ml_bits;
    // Then the literal length at most 16 and the states at most 26.
    if of_bits + ml_bits + ll_bits > 30 {
        bits.reload();
    }
    let word = bits.bits;
    let lit_len = ll_entry.base + field(word, 0, ll_bits, bmi2) as u32;
    let mut taken = ll_bits;
    if has_next {
        let ll_state_bits = ll_entry.num_bits.get();
        let ml_state_bits = ml_entry.num_bits.get();
        let of_state_bits = of_entry.num_bits.get();
        // The literal length and the bits of its state
        let span = u32::from(*tables.ll.spans.get(*ll_state)?);
        let (ll_next, ml_next, of_next);
        if bmi2 {
            // What is below each field: two steps from the table entries at most.
            let below = 64u32.wrapping_sub(span);
            ll_next = field_above(word, below, ll_state_bits);
            ml_next = field_above(word, below.wrapping_sub(ml_state_bits), ml_state_bits);
            of_next = field_above(
                word,
                below.wrapping_sub(ml_state_bits + of_state_bits),
                of_state_bits,
            );
        } else {
            ll_next = field(word, ll_bits, ll_state_bits, false);
            ml_next = field(word, span, ml_state_bits, false);
            of_next = field(word, span + ml_state_bits, of_state_bits, false);
        }
        *ll_state = usize::from(ll_entry.next) + ll_next as usize;
        *ml_state = usize::from(ml_entry.next) + ml_next as usize;
        *of_state = usize::from(of_entry.next) + of_next as usize;
        taken = span + ml_state_bits + of_state_bits;
    }
    bits.bits = word << (taken & 63);
    bits.consumed += taken;

    // The offset: a new one, or one of the last three.
    let offset = if offset_value > 3 {
        let offset = offset_value - 3;
        hist.push(offset);
        offset
    } else {
        // With no literals the repeated offsets are the second, the third and the first less one.
        hist.repeat(offset_value as usize - usize::from(lit_len != 0))
    };
    Some(Sequence {
        lit_len,
        match_len,
        offset,
    })
}

/// Decodes the `count` sequences of `stream` and executes them: literals from `literals` and
/// matches from the output itself (or from `history`, before it) go to `output[pos..]`, the
/// `pos` bytes before being the window. The position after the block.
#[allow(clippy::too_many_arguments)]
fn execute(
    stream: &[u8],
    count: usize,
    tables: &SeqTables,
    offset_hist: &mut [u32; 3],
    literals: &[u8],
    output: &mut [u8],
    start: usize,
    max_block_len: usize,
    history: &History<'_>,
) -> Result<usize, BlockError> {
    let mut bits = BackBits::new(stream)
        .ok_or_else(|| block_error(DecodeSequenceError::ExtraPadding { skipped_bits: 9 }))?;
    // At least 56 bits are ready: the three states take at most 26.
    let ll_state = bits.read(tables.ll.accuracy_log) as usize;
    let of_state = bits.read(tables.of.accuracy_log) as usize;
    let ml_state = bits.read(tables.ml.accuracy_log) as usize;
    let mut decoder = SequenceDecoder {
        bits,
        ll_state,
        of_state,
        ml_state,
        hist: OffsetHistory::new(*offset_hist),
    };
    // A block decodes to no more than `max_block_len` bytes, within the output: nothing is
    // written after them.
    let end = output.len().min(start + max_block_len);
    let mut copier = Copier {
        lits: literals,
        literals: literals.len(),
        output: &mut output[..end],
        pos: start,
        start,
        max_block_len,
        history,
    };
    let mut batch = [Sequence::default(); SEQUENCE_BATCH];
    let mut remaining = count;
    while remaining != 0 {
        let batch = &mut batch[..remaining.min(SEQUENCE_BATCH)];
        remaining -= batch.len();
        cpu::fastest(
            #[inline(always)]
            |bmi2| decode_sequences(&mut decoder, tables, batch, remaining == 0, bmi2),
        )
        .ok_or_else(state_outside_table)?;
        for sequence in batch.iter() {
            copier.execute(*sequence)?;
        }
    }
    let mut bits = decoder.bits;
    bits.reload();
    if !bits.finished() {
        return Err(block_error(if bits.overread() {
            DecodeSequenceError::NotEnoughBytesForNumSequences
        } else {
            DecodeSequenceError::ExtraBits { bits_remaining: 1 }
        }));
    }
    *offset_hist = decoder.hist.offsets();
    copier.finish()
}

/// The error of a state that is not in its table.
fn state_outside_table() -> BlockError {
    block_error(DecodeSequenceError::FSEDecoderError(
        FSEDecoderError::TableIsUninitialized,
    ))
}

/// What executes the sequences of a block: where the literals and the output are.
struct Copier<'a> {
    /// The literals not copied yet
    lits: &'a [u8],
    /// The number of literals of the block
    literals: usize,
    /// The output as far as the block may reach
    output: &'a mut [u8],
    /// Where the next sequence goes
    pos: usize,
    /// Where the block starts
    start: usize,
    max_block_len: usize,
    history: &'a History<'a>,
}

impl Copier<'_> {
    /// The error of a block that would end at `pos`, after the output or what a block may be.
    fn too_long(&self, pos: usize) -> BlockError {
        if pos > self.start + self.max_block_len {
            block_error(ExecuteSequencesError::TooManyBytesGenerated {
                max: self.max_block_len,
                decoded: pos - self.start,
            })
        } else {
            BlockError::TargetTooSmall
        }
    }

    /// Copies the literals and the match of `sequence` to the output.
    #[inline(always)]
    fn execute(&mut self, sequence: Sequence) -> Result<(), BlockError> {
        let output = &mut *self.output;
        let pos = self.pos;
        let lit_len = sequence.lit_len as usize;
        let match_len = sequence.match_len as usize;
        let offset = sequence.offset as usize;
        let match_start = pos + lit_len;
        let match_end = match_start + match_len;

        // The usual sequence, away from the ends of the literals and of the output: at most
        // sixteen literals and a match of at most sixteen bytes from sixteen bytes back or more.
        // Two copies of sixteen bytes do it, whatever the lengths, and what the conditions of the
        // copies leave to check is nothing: the literals are there, the match ends before the
        // block may, and the offset is not zero.
        if lit_len.max(match_len) <= 16 {
            if let (Some(from), Some(to)) = (self.lits.first_chunk::<16>(), chunk_at(output, pos)) {
                *to = *from;
                // Before the output (an offset greater than `match_start`) is not in it.
                let from = match_start.wrapping_sub(offset);
                if let Some((before, after)) = output.split_at_mut_checked(match_start) {
                    if let (Some(from), Some(to)) = (
                        before.get(from..).and_then(<[u8]>::first_chunk::<16>),
                        after.first_chunk_mut::<16>(),
                    ) {
                        *to = *from;
                        self.lits = &self.lits[lit_len..];
                        self.pos = match_end;
                        return Ok(());
                    }
                }
            }
        }

        // Up to twice as long, the same with copies of 32 bytes. The two halves of the match are
        // copied one after the other: from less than 32 bytes back the second reads what the
        // first wrote.
        if lit_len <= 32
            && match_len <= 32
            && offset >= 16
            && offset <= match_start
            && match_start + 32 <= output.len()
        {
            if let Some(from) = self.lits.first_chunk::<32>() {
                output[pos..pos + 32].copy_from_slice(from);
                let from = match_start - offset;
                output.copy_within(from..from + 16, match_start);
                output.copy_within(from + 16..from + 32, match_start + 16);
                self.lits = &self.lits[lit_len..];
                self.pos = match_end;
                return Ok(());
            }
        }

        if lit_len > self.lits.len() {
            return Err(block_error(
                ExecuteSequencesError::NotEnoughBytesForSequence {
                    wanted: self.literals - self.lits.len() + lit_len,
                    have: self.literals,
                },
            ));
        }
        if match_end > output.len() {
            return Err(self.too_long(match_end));
        }
        if offset == 0 {
            return Err(block_error(ExecuteSequencesError::ZeroOffset));
        }
        let (now, later) = self.lits.split_at(lit_len);
        output[pos..match_start].copy_from_slice(now);
        self.lits = later;
        self.pos = match_end;

        // A match that starts before the output: only with a dictionary or a window in two
        // pieces, otherwise an error.
        if offset > match_start {
            copy_from_history(output, match_start, match_len, offset, self.history)
                .map_err(block_error)
        } else {
            copy_match(output, match_start, match_len, offset);
            Ok(())
        }
    }

    /// Copies the literals after the last sequence: the position after the block.
    fn finish(self) -> Result<usize, BlockError> {
        let end = self.pos + self.lits.len();
        if end > self.output.len() {
            return Err(self.too_long(end));
        }
        self.output[self.pos..end].copy_from_slice(self.lits);
        Ok(end)
    }
}

/// Copies the match of `match_len` bytes from `offset` bytes back to `output[match_start..]`.
/// It is within `output` (`offset` is at most `match_start` and not zero) and may overlap what
/// it writes: sixteen bytes at once from sixteen bytes back or more, eight at a time from eight
/// back or more (each eight are then written before they are read again), byte by byte from
/// closer.
#[inline(always)]
fn copy_match(output: &mut [u8], match_start: usize, match_len: usize, offset: usize) {
    let match_end = match_start + match_len;
    let from = match_start - offset;
    if offset >= 16 && match_len <= 16 && match_start + 16 <= output.len() {
        output.copy_within(from..from + 16, match_start);
    } else if offset >= 8 && match_len <= 32 && match_start + 32 <= output.len() {
        let mut copied = 0;
        while copied < match_len {
            output.copy_within(from + copied..from + copied + 8, match_start + copied);
            copied += 8;
        }
    } else if offset >= match_len {
        output.copy_within(from..from + match_len, match_start);
    } else if offset == 1 {
        let byte = output[from];
        output[match_start..match_end].fill(byte);
    } else if match_len <= 32 {
        let (history, target) = output.split_at_mut(match_start);
        let target = &mut target[..match_len];
        // The bytes of the match not yet written are those written `offset` bytes before.
        let (first, rest) = target.split_at_mut(offset);
        first.copy_from_slice(&history[from..]);
        for at in 0..rest.len() {
            target[offset + at] = target[at];
        }
    } else {
        // The bytes repeat with period `offset`: each copy doubles what is there to copy.
        let mut copied = 0;
        while copied < match_len {
            let n = (offset + copied).min(match_len - copied);
            output.copy_within(from..from + n, match_start + copied);
            copied += n;
        }
    }
}

/// The sixteen bytes of `output` at `pos`, when they are there.
#[inline(always)]
fn chunk_at(output: &mut [u8], pos: usize) -> Option<&mut [u8; 16]> {
    output.get_mut(pos..)?.first_chunk_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *state >> 8
    }

    fn fields_of(entry: &SeqEntry) -> (u32, u16, u32, u32) {
        (
            entry.base,
            entry.next,
            entry.extra_bits.get(),
            entry.num_bits.get(),
        )
    }

    /// The table made of a distribution is the one made of the FSE table of the distribution:
    /// for the predefined distributions and for distributions of every accuracy, with and without
    /// symbols of probability "less than one".
    #[test]
    fn a_table_of_a_distribution_is_that_of_its_fse_table() {
        let mut distributions: Vec<(u8, Vec<i32>)> = Vec::new();
        distributions.push((
            LL_DEFAULT_ACC_LOG,
            LITERALS_LENGTH_DEFAULT_DISTRIBUTION.to_vec(),
        ));
        distributions.push((
            ML_DEFAULT_ACC_LOG,
            MATCH_LENGTH_DEFAULT_DISTRIBUTION.to_vec(),
        ));
        distributions.push((OF_DEFAULT_ACC_LOG, OFFSET_DEFAULT_DISTRIBUTION.to_vec()));
        let mut state = 7;
        for round in 0..400 {
            let accuracy_log = 5 + (round % 5) as u8;
            let size = 1i32 << accuracy_log;
            let symbols = 1 + lcg(&mut state) as usize % 36;
            let mut probabilities = alloc::vec![0i32; symbols];
            let mut left = size;
            // Some symbols of probability "less than one" (not the first, which is left to take
            // a piece), then the rest of the table in pieces.
            for probability in probabilities.iter_mut().skip(1) {
                if left > 1 && lcg(&mut state).is_multiple_of(4) {
                    *probability = -1;
                    left -= 1;
                }
            }
            while left > 0 {
                let symbol = lcg(&mut state) as usize % symbols;
                if probabilities[symbol] == -1 {
                    continue;
                }
                let piece = 1 + lcg(&mut state) as i32 % left;
                probabilities[symbol] += piece;
                left -= piece;
            }
            distributions.push((accuracy_log, probabilities));
        }

        for (accuracy_log, probabilities) in &distributions {
            let mut fse = FSETable::new(255);
            fse.build_from_probabilities(*accuracy_log, probabilities)
                .unwrap();
            let mut expected = SeqTable::new();
            expected.set_fse(&fse, &ll_code);
            let mut table = SeqTable::new();
            assert!(table.set_distribution(probabilities, *accuracy_log, ll_code));
            assert!(table.defined);
            assert_eq!(table.accuracy_log, expected.accuracy_log);
            let size = 1 << accuracy_log;
            for state in 0..size {
                assert_eq!(
                    fields_of(&table.entries[state]),
                    fields_of(&expected.entries[state]),
                    "state {state} of {probabilities:?}"
                );
                assert_eq!(table.spans[state], expected.spans[state]);
                let entry = &table.entries[state];
                assert_eq!(
                    u32::from(table.spans[state]),
                    entry.extra_bits.get() + entry.num_bits.get()
                );
            }
        }

        // Probabilities that are not those of a table of the size are refused.
        let mut table = SeqTable::new();
        assert!(!table.set_distribution(&[16, 15], 5, ll_code));
        assert!(!table.set_distribution(&[16, 17], 5, ll_code));
        assert!(!table.set_distribution(&[-1; 33], 5, ll_code));
        assert!(!table.set_distribution(&[8, 8], 4, ll_code));
        assert!(!table.set_distribution(&[512, 512], 10, ll_code));
        assert!(table.set_distribution(&[-1; 32], 5, ll_code));
    }

    /// The ring of the last offsets keeps them as the three values of the format do.
    #[test]
    fn the_offset_history_is_that_of_three_values() {
        let mut state = 99;
        let mut plain = [1u32, 4, 8];
        let mut ring = OffsetHistory::new(plain);
        for _ in 0..5000 {
            let kind = lcg(&mut state) % 8;
            let (expected, got) = if kind < 4 {
                let offset = 1 + lcg(&mut state) % 1000;
                plain = [offset, plain[0], plain[1]];
                ring.push(offset);
                (offset, offset)
            } else {
                let index = (kind - 4) as usize;
                let expected = match index {
                    0 => plain[0],
                    1 => {
                        plain = [plain[1], plain[0], plain[2]];
                        plain[0]
                    }
                    2 => {
                        plain = [plain[2], plain[0], plain[1]];
                        plain[0]
                    }
                    _ => {
                        let offset = plain[0].saturating_sub(1);
                        plain = [offset, plain[0], plain[1]];
                        offset
                    }
                };
                (expected, ring.repeat(index))
            };
            assert_eq!(got, expected);
            assert_eq!(ring.offsets(), plain);
        }
    }

    /// A field of a word is the same bits read either way, and what a reader that shifts the
    /// word after every field reads.
    #[test]
    fn fields_are_the_bits_a_reader_reads() {
        let mut state = 3;
        for _ in 0..2000 {
            let bytes: [u8; 8] = core::array::from_fn(|_| lcg(&mut state) as u8 | 1);
            let word = u64::from_le_bytes(bytes);
            let mut reader = BackBits::new(&bytes).unwrap();
            // Past the padding, as the reader is.
            let mut skip = reader.consumed;
            assert_eq!(reader.bits, word << skip);
            while skip < 57 {
                let n = lcg(&mut state) % 33;
                let n = n.min(57 - skip);
                let expected = u64::from(reader.read(n as u8));
                assert_eq!(field(word, skip, n, false), expected);
                assert_eq!(field(word, skip, n, true), expected);
                assert_eq!(field_above(word, 64 - skip - n, n), expected);
                skip += n;
                if n == 0 {
                    skip += 1;
                    reader.read(1);
                }
            }
        }
    }
}
