//! Decoding of compressed blocks into a slice, and of whole frames from a slice into a slice.
//!
//! [`BlockScratch::decode_block`] decodes a block to a position of a slice whose bytes before
//! that position are the window: sequences are executed as they are decoded. The layout follows
//! the reference decoder's (`zstd_decompress_block.c`): sequence tables hold the base value and
//! extra bits of each state, and the bitstream is refilled at most twice per sequence. Both
//! decoders use it:
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
use core::convert::{TryFrom, TryInto};

/// A state of a sequence table: what the state decodes to and how to reach the next one.
#[derive(Clone, Copy, Default)]
struct SeqEntry {
    /// The value of the state's code, to which `extra_bits` bits of the stream are added.
    base: u32,
    /// The first of the states that may follow, to which `num_bits` bits are added.
    next: u16,
    extra_bits: u8,
    num_bits: u8,
}

const SEQ_TABLE_SIZE: usize = 512;

/// The table of literal lengths, match lengths or offsets of a block.
struct SeqTable {
    entries: [SeqEntry; SEQ_TABLE_SIZE],
    accuracy_log: u8,
    /// Set once a block has defined the table: a block may only repeat a defined table.
    defined: bool,
}

impl SeqTable {
    fn new() -> Self {
        SeqTable {
            entries: [SeqEntry::default(); SEQ_TABLE_SIZE],
            accuracy_log: 0,
            defined: false,
        }
    }

    /// The table of an FSE table, with the base and extra bits of each code.
    fn set_fse(&mut self, table: &FSETable, codes: &dyn Fn(u8) -> (u32, u8)) {
        for (entry, state) in self.entries.iter_mut().zip(&table.decode) {
            let (base, extra_bits) = codes(state.symbol);
            *entry = SeqEntry {
                base,
                next: state.base_line as u16,
                extra_bits,
                num_bits: state.num_bits,
            };
        }
        self.accuracy_log = table.accuracy_log;
        self.defined = true;
    }

    /// The table of a single code: one state, reached with no bits.
    fn set_rle(&mut self, (base, extra_bits): (u32, u8)) {
        self.entries[0] = SeqEntry {
            base,
            next: 0,
            extra_bits,
            num_bits: 0,
        };
        self.accuracy_log = 0;
        self.defined = true;
    }

    fn copy_from(&mut self, other: &SeqTable) {
        let states = 1usize << other.accuracy_log;
        self.entries[..states].copy_from_slice(&other.entries[..states]);
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

/// The predefined tables, built once.
struct Predefined {
    ll: SeqTable,
    of: SeqTable,
    ml: SeqTable,
}

/// What the blocks of a frame keep from one to the next: the tables a block may repeat and the
/// last three offsets. The allocations are kept from frame to frame.
pub(crate) struct BlockScratch {
    huf: HuffmanTable,
    fse: FSETable,
    ll: SeqTable,
    of: SeqTable,
    ml: SeqTable,
    predefined: Option<Box<Predefined>>,
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
            ll: SeqTable::new(),
            of: SeqTable::new(),
            ml: SeqTable::new(),
            predefined: None,
            literals: Vec::new(),
            offset_hist: [1, 4, 8],
        }
    }

    /// The state a frame without a dictionary starts with.
    pub(crate) fn reset(&mut self) {
        self.huf.reset();
        self.ll.defined = false;
        self.of.defined = false;
        self.ml.defined = false;
        self.offset_hist = [1, 4, 8];
    }

    /// The state a frame with `dict` starts with: its tables, which the first block may repeat,
    /// and its offsets.
    pub(crate) fn init_from_dict(&mut self, dict: &Dictionary) {
        self.huf.reinit_from(&dict.huf.table);
        self.ll
            .set_from_dict(&dict.fse.literal_lengths, dict.fse.ll_rle, &ll_code);
        self.of
            .set_from_dict(&dict.fse.offsets, dict.fse.of_rle, &of_code);
        self.ml
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
                self.literals.clear();
                self.literals.resize(regenerated, literals_source[0]);
                &self.literals
            }
            LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
                self.literals.clear();
                self.literals.resize(regenerated, 0);
                decode_huffman(&section, &mut self.huf, literals_source, &mut self.literals)
                    .map_err(block_error)?;
                &self.literals
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
            &mut self.ll,
            &mut self.of,
            &mut self.ml,
            &mut self.predefined,
        )
        .map_err(block_error)?;
        execute(
            raw.get(tables_len..).unwrap_or_default(),
            sequences.num_sequences as usize,
            (&self.ll, &self.of, &self.ml),
            &mut self.offset_hist,
            literals,
            output,
            pos,
            max_block_len,
            history,
        )
    }
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
        let streams = [
            &source[..jump1],
            &source[jump1..jump2],
            &source[jump2..jump3],
            &source[jump3..],
        ];
        table
            .decode_four_streams(streams, target)
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
    ll: &mut SeqTable,
    of: &mut SeqTable,
    ml: &mut SeqTable,
    predefined: &mut Option<Box<Predefined>>,
) -> Result<usize, DecodeSequenceError> {
    let modes = header
        .modes
        .ok_or(DecodeSequenceError::MissingCompressionMode)?;
    let predefined = &**predefined.get_or_insert_with(|| {
        let mut tables = Box::new(Predefined {
            ll: SeqTable::new(),
            of: SeqTable::new(),
            ml: SeqTable::new(),
        });
        let build = |table: &mut SeqTable,
                     max: u8,
                     log: u8,
                     probabilities: &[i32],
                     codes: &dyn Fn(u8) -> (u32, u8)| {
            let mut fse = FSETable::new(max);
            fse.build_from_probabilities(log, probabilities)
                .expect("the predefined distributions are valid");
            table.set_fse(&fse, codes);
        };
        build(
            &mut tables.ll,
            MAX_LITERAL_LENGTH_CODE,
            LL_DEFAULT_ACC_LOG,
            &LITERALS_LENGTH_DEFAULT_DISTRIBUTION,
            &ll_code,
        );
        build(
            &mut tables.of,
            MAX_OFFSET_CODE,
            OF_DEFAULT_ACC_LOG,
            &OFFSET_DEFAULT_DISTRIBUTION,
            &of_code,
        );
        build(
            &mut tables.ml,
            MAX_MATCH_LENGTH_CODE,
            ML_DEFAULT_ACC_LOG,
            &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
            &ml_code,
        );
        tables
    });

    let mut read = 0;
    let mut update = |mode: ModeType,
                      table: &mut SeqTable,
                      default: &SeqTable,
                      max_code: u8,
                      max_log: u8,
                      codes: &dyn Fn(u8) -> (u32, u8),
                      missing: DecodeSequenceError|
     -> Result<(), DecodeSequenceError> {
        match mode {
            ModeType::FSECompressed => {
                fse.reset();
                fse.set_max_symbol(max_code);
                read += fse.build_decoder(source.get(read..).unwrap_or_default(), max_log)?;
                table.set_fse(fse, codes);
            }
            ModeType::RLE => {
                let code = *source.get(read).ok_or(missing)?;
                if code > max_code {
                    return Err(DecodeSequenceError::MissingByteForRleMlTable);
                }
                read += 1;
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
    };
    update(
        modes.ll_mode(),
        ll,
        &predefined.ll,
        MAX_LITERAL_LENGTH_CODE,
        LL_MAX_LOG,
        &ll_code,
        DecodeSequenceError::MissingByteForRleLlTable,
    )?;
    update(
        modes.of_mode(),
        of,
        &predefined.of,
        MAX_OFFSET_CODE,
        OF_MAX_LOG,
        &of_code,
        DecodeSequenceError::MissingByteForRleOfTable,
    )?;
    update(
        modes.ml_mode(),
        ml,
        &predefined.ml,
        MAX_MATCH_LENGTH_CODE,
        ML_MAX_LOG,
        &ml_code,
        DecodeSequenceError::MissingByteForRleMlTable,
    )?;
    Ok(read)
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

/// Decodes the `count` sequences of `stream` and executes them: literals from `literals` and
/// matches from the output itself (or from `history`, before it) go to `output[pos..]`, the
/// `pos` bytes before being the window. The position after the block.
#[allow(clippy::too_many_arguments)]
fn execute(
    stream: &[u8],
    count: usize,
    (ll, of, ml): (&SeqTable, &SeqTable, &SeqTable),
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
    let mut ll_state = bits.read(ll.accuracy_log) as usize;
    let mut of_state = bits.read(of.accuracy_log) as usize;
    let mut ml_state = bits.read(ml.accuracy_log) as usize;

    // A block decodes to no more than `max_block_len` bytes, within the output.
    let end = output.len().min(start + max_block_len);
    let too_long = |pos: usize| -> BlockError {
        if pos > start + max_block_len {
            block_error(ExecuteSequencesError::TooManyBytesGenerated {
                max: max_block_len,
                decoded: pos - start,
            })
        } else {
            BlockError::TargetTooSmall
        }
    };
    let mut hist = *offset_hist;
    let mut pos = start;
    let mut literals_pos = 0;
    for remaining in (0..count).rev() {
        bits.reload();
        let ll_entry = ll.entries[ll_state % SEQ_TABLE_SIZE];
        let of_entry = of.entries[of_state % SEQ_TABLE_SIZE];
        let ml_entry = ml.entries[ml_state % SEQ_TABLE_SIZE];

        // At least 57 bits are ready: the offset takes at most 31, the match length 16.
        let offset_value = of_entry.base + bits.read(of_entry.extra_bits);
        let match_len = (ml_entry.base + bits.read(ml_entry.extra_bits)) as usize;
        // Then the literal length at most 16 and the states at most 26.
        if of_entry.extra_bits + ml_entry.extra_bits + ll_entry.extra_bits > 30 {
            bits.reload();
        }
        let lit_len = (ll_entry.base + bits.read(ll_entry.extra_bits)) as usize;
        if remaining != 0 {
            ll_state = usize::from(ll_entry.next) + bits.read(ll_entry.num_bits) as usize;
            ml_state = usize::from(ml_entry.next) + bits.read(ml_entry.num_bits) as usize;
            of_state = usize::from(of_entry.next) + bits.read(of_entry.num_bits) as usize;
        }

        // The offset: a new one, or one of the last three.
        let offset = if offset_value > 3 {
            let offset = offset_value - 3;
            hist = [offset, hist[0], hist[1]];
            offset
        } else {
            // With no literals the repeated offsets are the second, the third and the first less one.
            let index = offset_value as usize - usize::from(lit_len != 0);
            match index {
                0 => hist[0],
                1 => {
                    hist = [hist[1], hist[0], hist[2]];
                    hist[0]
                }
                2 => {
                    hist = [hist[2], hist[0], hist[1]];
                    hist[0]
                }
                _ => {
                    let offset = hist[0].saturating_sub(1);
                    hist = [offset, hist[0], hist[1]];
                    offset
                }
            }
        } as usize;

        let literals_end = literals_pos + lit_len;
        let match_start = pos + lit_len;
        let match_end = match_start + match_len;
        if literals_end > literals.len() {
            return Err(block_error(
                ExecuteSequencesError::NotEnoughBytesForSequence {
                    wanted: literals_end,
                    have: literals.len(),
                },
            ));
        }
        if match_end > end {
            return Err(too_long(match_end));
        }
        if offset == 0 {
            return Err(block_error(ExecuteSequencesError::ZeroOffset));
        }

        // Literals: sixteen bytes at once when they fit (most do), whatever the length.
        if lit_len <= 16 && literals_pos + 16 <= literals.len() && pos + 16 <= output.len() {
            let chunk: [u8; 16] = literals[literals_pos..literals_pos + 16]
                .try_into()
                .unwrap();
            output[pos..pos + 16].copy_from_slice(&chunk);
        } else {
            output[pos..match_start].copy_from_slice(&literals[literals_pos..literals_end]);
        }
        literals_pos = literals_end;

        // A match that starts before the output: only with a dictionary or a window in two
        // pieces, otherwise an error.
        if offset > match_start {
            copy_from_history(output, match_start, match_len, offset, history)
                .map_err(block_error)?;
            pos = match_end;
            continue;
        }

        // The match, which may overlap what it writes. Most matches are short: sixteen bytes at
        // once from sixteen bytes back or more, eight at a time from eight back or more (each
        // eight are then written before they are read again), byte by byte from closer.
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
        pos = match_end;
    }
    bits.reload();
    if !bits.finished() {
        return Err(block_error(if bits.overread() {
            DecodeSequenceError::NotEnoughBytesForNumSequences
        } else {
            DecodeSequenceError::ExtraBits { bits_remaining: 1 }
        }));
    }
    *offset_hist = hist;

    // The literals after the last sequence.
    let rest = &literals[literals_pos..];
    if pos + rest.len() > end {
        return Err(too_long(pos + rest.len()));
    }
    output[pos..pos + rest.len()].copy_from_slice(rest);
    Ok(pos + rest.len())
}
