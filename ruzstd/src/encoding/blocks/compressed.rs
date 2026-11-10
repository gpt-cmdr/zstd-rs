use alloc::vec::Vec;

use crate::{
    bit_io::BitWriter,
    encoding::frame_compressor::{CompressState, PreviousTable},
    encoding::{Matcher, Sequence},
    fse::fse_encoder::{self, FSETable, State},
    huff0::huff0_encoder,
};

/// What the decoder will know after a compressed block: the offset history and the entropy tables later blocks
/// may repeat. The caller applies it only if the block is actually emitted as a compressed block, a raw block
/// does not show any of it to the decoder.
#[must_use]
pub struct BlockEntropy {
    offset_history: [u32; 3],
    huff_table: Option<huff0_encoder::HuffmanTable>,
    ll: Option<PreviousTable>,
    ml: Option<PreviousTable>,
    of: Option<PreviousTable>,
}

impl BlockEntropy {
    /// Make this block's state the one later blocks build on
    pub(crate) fn commit<M: Matcher>(self, state: &mut CompressState<M>) {
        state.offset_history = self.offset_history;
        if let Some(table) = self.huff_table {
            state.last_huff_table = Some(table);
        }
        if let Some(t) = self.ll {
            state.fse_tables.ll_previous = t;
        }
        if let Some(t) = self.ml {
            state.fse_tables.ml_previous = t;
        }
        if let Some(t) = self.of {
            state.fse_tables.of_previous = t;
        }
    }
}

/// A sequence with its symbols already computed
#[derive(Clone, Copy)]
pub(crate) struct CodedSequence {
    ll: u32,
    ml: u32,
    of: u32,
    ll_code: u8,
    ml_code: u8,
    of_code: u8,
}

/// A block of [`crate::common::BlockType::Compressed`]
///
/// Returns the state the decoder has after this block, see [`BlockEntropy`].
pub fn compress_block<M: Matcher>(
    state: &mut CompressState<M>,
    output: &mut Vec<u8>,
) -> BlockEntropy {
    let CompressState {
        matcher,
        literals_buf,
        sequences_buf,
        offset_history,
        ..
    } = state;
    let mut offset_history = *offset_history;
    sequences_buf.clear();
    // The literals are gathered at the start of a buffer that is never shorter than the block and the reach
    // of a short copy, so appending a run does not have to make room first
    let block_len = matcher.get_last_space().len();
    if literals_buf.len() < block_len + SHORT_RUN {
        literals_buf.resize(block_len + SHORT_RUN, 0);
    }
    // Enough for most blocks, so the buffer does not grow in small steps while matching
    sequences_buf.reserve(block_len / 16);
    let mut literals_len = 0;
    // Kept small, so that it becomes part of the loops of the matchers: the codes are worked out afterwards
    matcher.start_matching(
        #[inline(always)]
        |seq| match seq {
            Sequence::Literals { literals } => {
                literals_buf[literals_len..literals_len + literals.len()].copy_from_slice(literals);
                literals_len += literals.len();
            }
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                push_literals(literals_buf, literals_len, literals);
                literals_len += literals.len();
                sequences_buf.push(CodedSequence {
                    ll: literals.len() as u32,
                    ml: match_len as u32,
                    of: offset as u32,
                    ll_code: 0,
                    ml_code: 0,
                    of_code: 0,
                });
            }
        },
    );
    let mut counts = SymbolCounts::new();
    for seq in sequences_buf.iter_mut() {
        seq.of = offset_value(seq.of, seq.ll, &mut offset_history);
        seq.ll_code = ll_code(seq.ll);
        seq.ml_code = ml_code(seq.ml);
        seq.of_code = seq.of.ilog2() as u8;
        counts.ll[seq.ll_code as usize] += 1;
        counts.ml[seq.ml_code as usize] += 1;
        counts.of[seq.of_code as usize] += 1;
    }
    let literals_vec = &state.literals_buf[..literals_len];
    let sequences = &state.sequences_buf;

    let mut entropy = BlockEntropy {
        offset_history,
        huff_table: None,
        ll: None,
        ml: None,
        of: None,
    };

    // literals section

    let mut writer = BitWriter::from(output);
    if literals_vec.len() > 1024 {
        entropy.huff_table =
            compress_literals(literals_vec, state.last_huff_table.as_ref(), &mut writer);
    } else {
        raw_literals(literals_vec, &mut writer);
    }

    // sequences section

    if sequences.is_empty() {
        writer.write_bits(0u8, 8);
    } else {
        encode_seqnum(sequences.len(), &mut writer);

        let tables = &mut state.fse_tables;
        let ll_mode = choose_table(
            &tables.ll_previous,
            &counts.ll,
            LL_MAX_LOG,
            fse_encoder::LL_DIST,
            6,
        );
        let ml_mode = choose_table(
            &tables.ml_previous,
            &counts.ml,
            ML_MAX_LOG,
            fse_encoder::ML_DIST,
            6,
        );
        let of_mode = choose_table(
            &tables.of_previous,
            &counts.of,
            OF_MAX_LOG,
            fse_encoder::OF_DIST,
            5,
        );

        writer.write_bits(encode_fse_table_modes(&ll_mode, &ml_mode, &of_mode), 8);

        encode_table(&ll_mode, &mut writer);
        encode_table(&of_mode, &mut writer);
        encode_table(&ml_mode, &mut writer);

        let ll_table = resolve(
            &ll_mode,
            &tables.ll_previous,
            &mut tables.ll_default,
            fse_encoder::default_ll_table,
        );
        let ml_table = resolve(
            &ml_mode,
            &tables.ml_previous,
            &mut tables.ml_default,
            fse_encoder::default_ml_table,
        );
        let of_table = resolve(
            &of_mode,
            &tables.of_previous,
            &mut tables.of_default,
            fse_encoder::default_of_table,
        );
        encode_sequences(sequences, &mut writer, ll_table, ml_table, of_table);

        entropy.ll = ll_mode.into_previous();
        entropy.ml = ml_mode.into_previous();
        entropy.of = of_mode.into_previous();
    }
    writer.flush();
    entropy
}

/// The longest run of literals that is copied with a few fixed size moves instead of a call to `memcpy`, which
/// costs more than the copy for the few bytes most runs have
const SHORT_RUN: usize = 16;

/// Copy the literals of a sequence to `buf[at..]`. `buf` is at least [`SHORT_RUN`] bytes longer than all
/// literals of the block.
#[inline(always)]
fn push_literals(buf: &mut [u8], at: usize, literals: &[u8]) {
    let len = literals.len();
    if len > SHORT_RUN {
        buf[at..at + len].copy_from_slice(literals);
        return;
    }
    // The two ends of the run, which overlap unless the run has just the length of both
    let dst = &mut buf[at..at + SHORT_RUN];
    if len >= 8 {
        dst[..8].copy_from_slice(&literals[..8]);
        dst[len - 8..len].copy_from_slice(&literals[len - 8..]);
    } else if len >= 4 {
        dst[..4].copy_from_slice(&literals[..4]);
        dst[len - 4..len].copy_from_slice(&literals[len - 4..]);
    } else if len > 0 {
        dst[0] = literals[0];
        dst[len / 2] = literals[len / 2];
        dst[len - 1] = literals[len - 1];
    }
}

/// Largest accuracy logs the format allows for the three fields (RFC 8878, section 3.1.1.3.2.2)
const LL_MAX_LOG: u8 = 9;
const ML_MAX_LOG: u8 = 9;
const OF_MAX_LOG: u8 = 8;

struct SymbolCounts {
    ll: [u32; 36],
    ml: [u32; 53],
    of: [u32; 32],
}

impl SymbolCounts {
    fn new() -> Self {
        Self {
            ll: [0; 36],
            ml: [0; 53],
            of: [0; 32],
        }
    }
}

/// Turns an actual match offset into the offset value written to the stream, using a repeat code when the offset is
/// one of the three most recent ones, and updates `history` the way the decoder will (RFC 8878, section 3.1.2.5).
///
/// When the literal length is zero the repeat codes are shifted by one: 1 and 2 stand for the second and third
/// recent offset and 3 for the most recent offset minus one.
pub(crate) fn encode_offset_value(offset: u32, literal_len: u32, history: &mut [u32; 3]) -> u32 {
    debug_assert!(offset > 0);
    let [rep1, rep2, rep3] = *history;
    if literal_len > 0 {
        if offset == rep1 {
            return 1;
        }
        if offset == rep2 {
            *history = [rep2, rep1, rep3];
            return 2;
        }
        if offset == rep3 {
            *history = [rep3, rep1, rep2];
            return 3;
        }
    } else {
        if offset == rep2 {
            *history = [rep2, rep1, rep3];
            return 1;
        }
        if offset == rep3 {
            *history = [rep3, rep1, rep2];
            return 2;
        }
        if rep1 > 1 && offset == rep1 - 1 {
            *history = [offset, rep1, rep2];
            return 3;
        }
    }
    *history = [offset, rep1, rep2];
    offset + 3
}

/// [`encode_offset_value`] as selections instead of branches: whether a sequence repeats an offset is hard to
/// predict.
#[inline(always)]
fn offset_value(offset: u32, literal_len: u32, history: &mut [u32; 3]) -> u32 {
    debug_assert!(offset > 0);
    let [rep1, rep2, rep3] = *history;
    // The offsets that the codes 1, 2 and 3 stand for. Without literals the third one is the most recent
    // offset minus one, which no offset equals if that is zero.
    let (first, second, third) = if literal_len > 0 {
        (rep1, rep2, rep3)
    } else {
        (rep2, rep3, rep1 - 1)
    };
    let mut value = offset + 3;
    value = if offset == third { 3 } else { value };
    value = if offset == second { 2 } else { value };
    value = if offset == first { 1 } else { value };
    // The history stays as it is for the most recent offset after literals. Every other offset moves to the
    // front, from its place in the history if it has one.
    let unchanged = literal_len > 0 && offset == rep1;
    *history = [
        if unchanged { rep1 } else { offset },
        if unchanged { rep2 } else { rep1 },
        if unchanged || offset == rep2 {
            rep3
        } else {
            rep2
        },
    ];
    value
}

/// How the symbols of one field of the sequences are encoded in a block
#[allow(clippy::large_enum_variant)]
enum FseTableMode {
    Predefined,
    /// Every sequence has this symbol
    Rle(u8),
    Encoded(FSETable),
    RepeatLast,
}

impl FseTableMode {
    /// The table later blocks may repeat after this block, `None` if it stays what it was
    fn into_previous(self) -> Option<PreviousTable> {
        match self {
            Self::Predefined => Some(PreviousTable::Predefined),
            // Repeating an RLE "table" is valid, but never chosen
            Self::Rle(_) => Some(PreviousTable::None),
            Self::Encoded(table) => Some(PreviousTable::Encoded(table)),
            Self::RepeatLast => None,
        }
    }
}

/// The table to encode with, `None` for RLE
fn resolve<'a>(
    mode: &'a FseTableMode,
    previous: &'a PreviousTable,
    default: &'a mut Option<FSETable>,
    make_default: fn() -> FSETable,
) -> Option<&'a FSETable> {
    match mode {
        FseTableMode::Rle(_) => None,
        FseTableMode::Encoded(table) => Some(table),
        FseTableMode::Predefined => Some(default.get_or_insert_with(make_default)),
        FseTableMode::RepeatLast => match previous {
            PreviousTable::Encoded(table) => Some(table),
            PreviousTable::Predefined => Some(default.get_or_insert_with(make_default)),
            PreviousTable::None => unreachable!("repeat without a previous table"),
        },
    }
}

/// Pick the cheapest of the predefined table, an RLE symbol, the previous block's table and a new table, by the
/// estimated number of bits of the table description plus the encoded symbols
fn choose_table(
    previous: &PreviousTable,
    counts: &[u32],
    max_log: u8,
    default_probs: &[i16],
    default_log: u8,
) -> FseTableMode {
    let mut used = 0;
    let mut last_used = 0;
    for (symbol, &count) in counts.iter().enumerate() {
        if count > 0 {
            used += 1;
            last_used = symbol;
        }
    }
    let predefined = fse_encoder::cost_with_probs(default_probs, default_log, counts);
    let repeat = match previous {
        PreviousTable::None => None,
        PreviousTable::Predefined => predefined,
        PreviousTable::Encoded(table) => table.cost(counts),
    };
    let mut best = FseTableMode::Predefined;
    let mut best_cost = predefined.unwrap_or(u64::MAX);
    if let Some(cost) = repeat {
        if cost < best_cost {
            best = FseTableMode::RepeatLast;
            best_cost = cost;
        }
    }
    if used == 1 {
        // One byte for the symbol, nothing per sequence
        if 8 * 256 < best_cost {
            best = FseTableMode::Rle(last_used as u8);
        }
        return best;
    }
    let (probs, table_log) = fse_encoder::normalize(&counts[..=last_used], max_log);
    let new_cost = (fse_encoder::description_bits(&probs, table_log) as u64) * 256
        + fse_encoder::cost_with_probs(&probs, table_log, counts).unwrap();
    if new_cost < best_cost {
        best = FseTableMode::Encoded(fse_encoder::build_table_from_probabilities(
            &probs, table_log,
        ));
    }
    best
}

fn encode_table(mode: &FseTableMode, writer: &mut BitWriter<&mut Vec<u8>>) {
    match mode {
        FseTableMode::Predefined => {}
        FseTableMode::RepeatLast => {}
        FseTableMode::Rle(symbol) => writer.write_bits(*symbol, 8),
        FseTableMode::Encoded(table) => table.write_table(writer),
    }
}

fn encode_fse_table_modes(
    ll_mode: &FseTableMode,
    ml_mode: &FseTableMode,
    of_mode: &FseTableMode,
) -> u8 {
    fn mode_to_bits(mode: &FseTableMode) -> u8 {
        match mode {
            FseTableMode::Predefined => 0,
            FseTableMode::Rle(_) => 1,
            FseTableMode::Encoded(_) => 2,
            FseTableMode::RepeatLast => 3,
        }
    }
    mode_to_bits(ll_mode) << 6 | mode_to_bits(of_mode) << 4 | mode_to_bits(ml_mode) << 2
}

fn encode_sequences(
    sequences: &[CodedSequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_table: Option<&FSETable>,
    ml_table: Option<&FSETable>,
    of_table: Option<&FSETable>,
) {
    // RLE fields have no state and write no bits. Dispatch once, so the loop is specialized for each case.
    match (ll_table, ml_table, of_table) {
        (Some(ll), Some(ml), Some(of)) => {
            encode_sequences_with(sequences, writer, Some(ll), Some(ml), Some(of))
        }
        (ll, ml, of) => encode_sequences_with(sequences, writer, ll, ml, of),
    }
}

#[inline(always)]
fn encode_sequences_with(
    sequences: &[CodedSequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_table: Option<&FSETable>,
    ml_table: Option<&FSETable>,
    of_table: Option<&FSETable>,
) {
    let init = |table: Option<&FSETable>, symbol| table.map(|t| t.init_state(symbol));
    let last = sequences[sequences.len() - 1];
    writer.flush_full_bytes();
    let mut ll_state = init(ll_table, last.ll_code);
    let mut ml_state = init(ml_table, last.ml_code);
    let mut of_state = init(of_table, last.of_code);
    write_extra_bits(&last, writer);

    // encode backwards so the decoder reads the first sequence first
    for seq in sequences[..sequences.len() - 1].iter().rev() {
        // At most 7 bits are buffered here, and the three states add at most 26 (9 + 9 + 8)
        if let (Some(t), Some(s)) = (of_table, of_state.as_mut()) {
            t.encode(s, seq.of_code, writer);
        }
        if let (Some(t), Some(s)) = (ml_table, ml_state.as_mut()) {
            t.encode(s, seq.ml_code, writer);
        }
        if let (Some(t), Some(s)) = (ll_table, ll_state.as_mut()) {
            t.encode(s, seq.ll_code, writer);
        }
        write_extra_bits(seq, writer);
    }
    let flush = |table: Option<&FSETable>, state: &Option<State>, writer: &mut BitWriter<_>| {
        if let (Some(t), Some(s)) = (table, state) {
            t.flush_state(s, writer);
        }
    };
    flush(ml_table, &ml_state, writer);
    flush(of_table, &of_state, writer);
    writer.flush_full_bytes();
    flush(ll_table, &ll_state, writer);
    writer.flush_full_bytes();

    let bits_to_fill = writer.misaligned();
    if bits_to_fill == 0 {
        writer.write_bits(1u32, 8);
    } else {
        writer.write_bits(1u32, bits_to_fill);
    }
}

/// Write the extra bits of a sequence and flush the writer. At most 33 bits may be buffered before (7 left over
/// from the last flush and up to 26 bits of the three states).
#[inline(always)]
fn write_extra_bits(seq: &CodedSequence, writer: &mut BitWriter<&mut Vec<u8>>) {
    let ll_bits = LL_BITS[seq.ll_code as usize] as usize;
    let ml_bits = ML_BITS[seq.ml_code as usize] as usize;
    let of_bits = seq.of_code as usize;
    let extra = ll_bits + ml_bits + of_bits;
    if extra > 30 {
        writer.flush_full_bytes();
    }
    // At most 33 + 30 or 7 + 16 + 16 bits buffered after these two
    writer.add_bits(u64::from(seq.ll - LL_BASE[seq.ll_code as usize]), ll_bits);
    writer.add_bits(u64::from(seq.ml - ML_BASE[seq.ml_code as usize]), ml_bits);
    if extra > 56 {
        writer.flush_full_bytes();
    }
    // At most 7 + 31 or 7 + 56 bits buffered after this one
    writer.add_bits(u64::from(seq.of & ((1 << of_bits) - 1)), of_bits);
    writer.flush_full_bytes();
}
/// Number of extra bits of each literal length code (RFC 8878, section 3.1.1.3.2.1.1)
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
/// Smallest literal length of each code
const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
/// Number of extra bits of each match length code
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
/// Smallest match length of each code
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];

/// The literal length code of each length below 64
const LL_CODE: [u8; 64] = {
    let mut table = [0u8; 64];
    let mut code = 0;
    let mut len = 0;
    while len < 64 {
        while code + 1 < LL_BASE.len() && LL_BASE[code + 1] <= len as u32 {
            code += 1;
        }
        table[len] = code as u8;
        len += 1;
    }
    table
};

/// The match length code of each `match_len - 3` below 128
const ML_CODE: [u8; 128] = {
    let mut table = [0u8; 128];
    let mut code = 0;
    let mut len = 0;
    while len < 128 {
        while code + 1 < ML_BASE.len() && ML_BASE[code + 1] <= len as u32 + 3 {
            code += 1;
        }
        table[len] = code as u8;
        len += 1;
    }
    table
};

#[inline(always)]
fn ll_code(len: u32) -> u8 {
    if len < 64 {
        LL_CODE[len as usize]
    } else {
        // From 64 on every code covers a power of two
        len.ilog2() as u8 + 19
    }
}

#[inline(always)]
fn ml_code(len: u32) -> u8 {
    debug_assert!(len >= 3);
    let base = len - 3;
    if base < 128 {
        ML_CODE[base as usize]
    } else {
        base.ilog2() as u8 + 36
    }
}

fn encode_seqnum(seqnum: usize, writer: &mut BitWriter<impl AsMut<Vec<u8>>>) {
    const UPPER_LIMIT: usize = 0xFFFF + 0x7F00;
    match seqnum {
        1..=127 => writer.write_bits(seqnum as u32, 8),
        128..=0x7FFF => {
            let upper = ((seqnum >> 8) | 0x80) as u8;
            let lower = seqnum as u8;
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        0x8000..=UPPER_LIMIT => {
            let encode = seqnum - 0x7F00;
            let upper = (encode >> 8) as u8;
            let lower = encode as u8;
            writer.write_bits(255u8, 8);
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        _ => unreachable!(),
    }
}

fn raw_literals(literals: &[u8], writer: &mut BitWriter<&mut Vec<u8>>) {
    writer.write_bits(0u8, 2);
    writer.write_bits(0b11u8, 2);
    writer.write_bits(literals.len() as u32, 20);
    writer.append_bytes(literals);
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
) -> Option<huff0_encoder::HuffmanTable> {
    let reset_idx = writer.index();

    let new_encoder_table = huff0_encoder::HuffmanTable::build_from_data(literals);
    if !new_encoder_table.can_be_described() {
        raw_literals(literals, writer);
        return None;
    }

    let (encoder_table, new_table) = if let Some(_table) = last_table {
        if let Some(diff) = _table.can_encode(&new_encoder_table) {
            // TODO this is a very simple heuristic, maybe we should try to do better
            if diff > 5 {
                (&new_encoder_table, true)
            } else {
                (_table, false)
            }
        } else {
            (&new_encoder_table, true)
        }
    } else {
        (&new_encoder_table, true)
    };

    if new_table {
        writer.write_bits(2u8, 2); // compressed literals type
    } else {
        writer.write_bits(3u8, 2); // treeless compressed literals type
    }

    let (size_format, size_bits) = match literals.len() {
        0..6 => (0b00u8, 10),
        6..1024 => (0b01, 10),
        1024..16384 => (0b10, 14),
        16384..262144 => (0b11, 18),
        _ => unimplemented!("too many literals"),
    };

    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(encoder_table, writer);
    if size_format == 0 {
        encoder.encode(literals, new_table)
    } else {
        encoder.encode4x(literals, new_table)
    };
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    // If encoded len is bigger than the raw literals we are better off just writing the raw literals here
    if total_len >= literals.len() {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        None
    } else if new_table {
        Some(new_encoder_table)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::encode_offset_value;

    #[test]
    fn offset_values_follow_the_repeat_offset_rules() {
        let mut history = [1, 4, 8];
        // New offsets are shifted by 3 and pushed to the front
        assert_eq!(encode_offset_value(100, 5, &mut history), 103);
        assert_eq!(history, [100, 1, 4]);
        // The most recent offset, with literals: code 1, history unchanged
        assert_eq!(encode_offset_value(100, 2, &mut history), 1);
        assert_eq!(history, [100, 1, 4]);
        // The second and third recent offsets move to the front
        assert_eq!(encode_offset_value(1, 1, &mut history), 2);
        assert_eq!(history, [1, 100, 4]);
        assert_eq!(encode_offset_value(4, 1, &mut history), 3);
        assert_eq!(history, [4, 1, 100]);
    }

    #[test]
    fn literals_of_any_length_are_copied() {
        let literals: alloc::vec::Vec<u8> = (0..200u8).collect();
        for len in 0..=literals.len() {
            let mut buf = alloc::vec![0xEEu8; 3 + len + super::SHORT_RUN];
            super::push_literals(&mut buf, 3, &literals[..len]);
            assert_eq!(&buf[..3], &[0xEE; 3]);
            assert_eq!(&buf[3..3 + len], &literals[..len], "{len} literals");
        }
    }

    #[test]
    fn offset_values_by_selection_are_the_same() {
        // Few distinct values, so that every relation between the offset and the history occurs
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n) as u32
        };
        let mut history = [1, 4, 8];
        for round in 0..200_000 {
            if round % 64 == 0 {
                history = [1 + next(4), 1 + next(4), 1 + next(4)];
            }
            let offset = 1 + next(5);
            let literal_len = next(3);
            let mut expected = history;
            let value = encode_offset_value(offset, literal_len, &mut expected);
            assert_eq!(
                super::offset_value(offset, literal_len, &mut history),
                value,
                "offset {offset}, {literal_len} literals"
            );
            assert_eq!(history, expected, "offset {offset}, {literal_len} literals");
        }
    }

    #[test]
    fn offset_values_without_literals_are_shifted() {
        let mut history = [10, 20, 30];
        // Without literals the most recent offset can not be repeated with a repeat code
        assert_eq!(encode_offset_value(10, 0, &mut history), 13);
        assert_eq!(history, [10, 10, 20]);
        let mut history = [10, 20, 30];
        assert_eq!(encode_offset_value(20, 0, &mut history), 1);
        assert_eq!(history, [20, 10, 30]);
        let mut history = [10, 20, 30];
        assert_eq!(encode_offset_value(30, 0, &mut history), 2);
        assert_eq!(history, [30, 10, 20]);
        let mut history = [10, 20, 30];
        assert_eq!(encode_offset_value(9, 0, &mut history), 3);
        assert_eq!(history, [9, 10, 20]);
        // The most recent offset minus one does not exist if that offset is 1
        let mut history = [1, 20, 30];
        assert_eq!(encode_offset_value(1, 0, &mut history), 4);
        assert_eq!(history, [1, 1, 20]);
    }
}
