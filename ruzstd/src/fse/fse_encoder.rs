//! FSE encoding tables and the encoder used for the Huffman table description.
//!
//! The table layout follows the usual tANS formulation: the encoder state is a value in
//! `table_size..2 * table_size`, every symbol has a transform `(delta_nb_bits, delta_find_state)` that yields the
//! number of bits to flush and the position of the next state in `state_table`. Encoding a symbol is a few
//! arithmetic operations and one table lookup, independent of the symbol's probability.
//!
//! The symbol spreading is the one of the format (RFC 8878, section 4.1.1), so a table built here and the decoding
//! table built from the same normalized probabilities describe the same states.

use crate::bit_io::BitWriter;
use alloc::vec::Vec;

/// Largest accuracy log the encoder builds. The states are stored as `u16` values in `table_size..2 * table_size`.
const MAX_TABLE_LOG: u8 = 15;
/// Smallest accuracy log the format can describe
const MIN_TABLE_LOG: u8 = 5;

pub(crate) struct FSEEncoder<'output, V: AsMut<Vec<u8>>> {
    pub(super) table: FSETable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> FSEEncoder<'_, V> {
    pub fn new(table: FSETable, writer: &mut BitWriter<V>) -> FSEEncoder<'_, V> {
        FSEEncoder { table, writer }
    }

    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn into_table(self) -> FSETable {
        self.table
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Last state index
    /// * Padding bits to fill up last byte
    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn encode(&mut self, data: &[u8]) {
        self.write_table();

        let mut state = self.table.init_state(data[data.len() - 1]);
        self.writer.flush_full_bytes();
        for x in data[0..data.len() - 1].iter().rev().copied() {
            self.table.encode(&mut state, x, self.writer);
            self.writer.flush_full_bytes();
        }
        self.table.flush_state(&state, self.writer);

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    /// Encodes the data using the provided table but with two interleaved streams
    /// Writes
    /// * Table description
    /// * Encoded data with two interleaved states
    /// * Both Last state indexes
    /// * Padding bits to fill up last byte
    pub fn encode_interleaved(&mut self, data: &[u8]) {
        self.write_table();

        let table = &self.table;
        self.writer.flush_full_bytes();
        let mut state_1 = table.init_state(data[data.len() - 1]);
        let mut state_2 = table.init_state(data[data.len() - 2]);

        // The first two symbols are represented by the start states
        // Then encode the state transitions for two symbols at a time
        let mut idx = data.len() - 4;
        loop {
            table.encode(&mut state_1, data[idx + 1], self.writer);
            table.encode(&mut state_2, data[idx], self.writer);
            self.writer.flush_full_bytes();

            if idx < 2 {
                break;
            }
            idx -= 2;
        }

        // Determine if we have an even or odd number of symbols to encode
        // If odd we need to encode the last states transition and encode the final states in the flipped order
        if idx == 1 {
            table.encode(&mut state_1, data[0], self.writer);
            self.writer.flush_full_bytes();
            table.flush_state(&state_2, self.writer);
            table.flush_state(&state_1, self.writer);
        } else {
            table.flush_state(&state_1, self.writer);
            table.flush_state(&state_2, self.writer);
        }

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    fn write_table(&mut self) {
        self.table.write_table(self.writer);
    }

    #[cfg(any(test, feature = "fuzz_exports"))]
    pub(super) fn acc_log(&self) -> u8 {
        self.table.acc_log()
    }
}

/// How to get from the current state to the next one when encoding a symbol
#[derive(Debug, Clone, Copy, Default)]
struct SymbolTransform {
    /// `(state + delta_nb_bits) >> 16` is the number of bits flushed for this symbol
    delta_nb_bits: u32,
    /// Added to `state >> nb_bits` to find the next state in `state_table`
    delta_find_state: i32,
}

/// An FSE encoding table, built from normalized probabilities
#[derive(Debug, Clone)]
pub struct FSETable {
    table_log: u8,
    /// Number of entries in `probs` that are part of the table description (highest used symbol + 1)
    num_symbols: usize,
    /// The normalized probabilities as they appear in the table description, -1 means "less than one"
    probs: [i16; 256],
    /// The encoder states, ordered by symbol and then by their position in the decoding table
    state_table: Vec<u16>,
    transforms: [SymbolTransform; 256],
    pub(crate) table_size: usize,
}

/// The state of an FSE encoder, a value in `table_size..2 * table_size`
#[derive(Debug, Clone, Copy)]
pub(crate) struct State(u32);

impl FSETable {
    /// The state from which the first encoded symbol (the last one in the data) is decoded. Emits no bits.
    #[inline(always)]
    pub(crate) fn init_state(&self, symbol: u8) -> State {
        let tt = self.transforms[symbol as usize];
        let nb_bits = (tt.delta_nb_bits + (1 << 15)) >> 16;
        let value = (nb_bits << 16).wrapping_sub(tt.delta_nb_bits);
        State(self.state_table[((value >> nb_bits) as i32 + tt.delta_find_state) as usize] as u32)
    }

    /// Move to a state that decodes `symbol`, writing the bits the decoder needs to get back to the current state.
    ///
    /// Adds at most `acc_log` bits to the writer without flushing it, see [`BitWriter::add_bits`].
    #[inline(always)]
    pub(crate) fn encode<V: AsMut<Vec<u8>>>(
        &self,
        state: &mut State,
        symbol: u8,
        writer: &mut BitWriter<V>,
    ) {
        let tt = self.transforms[symbol as usize];
        let nb_bits = (state.0 + tt.delta_nb_bits) >> 16;
        writer.add_bits(u64::from(state.0 & ((1 << nb_bits) - 1)), nb_bits as usize);
        state.0 =
            self.state_table[((state.0 >> nb_bits) as i32 + tt.delta_find_state) as usize] as u32;
    }

    /// Write the final state, which the decoder reads first
    #[inline(always)]
    pub(crate) fn flush_state<V: AsMut<Vec<u8>>>(&self, state: &State, writer: &mut BitWriter<V>) {
        writer.write_bits_64(
            u64::from(state.0 - self.table_size as u32),
            self.table_log as usize,
        );
    }

    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn acc_log(&self) -> u8 {
        self.table_log
    }

    /// The normalized probability of a symbol, 0 if the table can not encode it
    #[cfg(any(test, feature = "fuzz_exports"))]
    pub(crate) fn probability(&self, symbol: u8) -> i16 {
        self.probs[symbol as usize]
    }

    /// Whether the encoder can be in the state that is decoded at `index` of the decoding table after encoding
    /// `symbol`
    #[cfg(any(test, feature = "fuzz_exports"))]
    pub(crate) fn encodes_state(&self, symbol: u8, index: usize) -> bool {
        let prob = self.probs[symbol as usize];
        let count = if prob == -1 { 1 } else { prob.max(0) as usize };
        let start = (self.transforms[symbol as usize].delta_find_state + count as i32) as usize;
        self.state_table[start..start + count]
            .iter()
            .any(|&state| state as usize == self.table_size + index)
    }

    /// Estimated cost in 1/256 bits of encoding symbols with the given counts with this table, `None` if a symbol
    /// with a nonzero count can not be encoded with it
    pub(crate) fn cost(&self, counts: &[u32]) -> Option<u64> {
        cost_with_probs(&self.probs[..self.num_symbols], self.table_log, counts)
    }

    pub(crate) fn write_table<V: AsMut<Vec<u8>>>(&self, writer: &mut BitWriter<V>) {
        write_description(&self.probs[..self.num_symbols], self.table_log, writer);
    }
}

/// Estimated cost in 1/256 bits of symbols with `counts` under the normalized probabilities `probs`
pub(crate) fn cost_with_probs(probs: &[i16], table_log: u8, counts: &[u32]) -> Option<u64> {
    let mut cost = 0u64;
    for (symbol, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let prob = *probs.get(symbol)?;
        if prob == 0 {
            return None;
        }
        let prob = prob.max(1) as u32;
        let bits = (u32::from(table_log) << 8) - log2_fixed8(prob);
        cost += u64::from(count) * u64::from(bits);
    }
    Some(cost)
}

/// `log2(1 + i / 256) * 256`, rounded
const LOG2_FRACTION: [u16; 256] = [
    0, 1, 3, 4, 6, 7, 9, 10, 11, 13, 14, 16, 17, 18, 20, 21, 22, 24, 25, 26, 28, 29, 30, 32, 33,
    34, 36, 37, 38, 40, 41, 42, 44, 45, 46, 47, 49, 50, 51, 52, 54, 55, 56, 57, 59, 60, 61, 62, 63,
    65, 66, 67, 68, 69, 71, 72, 73, 74, 75, 77, 78, 79, 80, 81, 82, 84, 85, 86, 87, 88, 89, 90, 92,
    93, 94, 95, 96, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113,
    114, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133,
    134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152,
    153, 154, 155, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 169,
    170, 171, 172, 173, 174, 175, 176, 177, 178, 178, 179, 180, 181, 182, 183, 184, 185, 185, 186,
    187, 188, 189, 190, 191, 192, 192, 193, 194, 195, 196, 197, 198, 198, 199, 200, 201, 202, 203,
    203, 204, 205, 206, 207, 208, 208, 209, 210, 211, 212, 212, 213, 214, 215, 216, 216, 217, 218,
    219, 220, 220, 221, 222, 223, 224, 224, 225, 226, 227, 228, 228, 229, 230, 231, 231, 232, 233,
    234, 234, 235, 236, 237, 238, 238, 239, 240, 241, 241, 242, 243, 244, 244, 245, 246, 247, 247,
    248, 249, 249, 250, 251, 252, 252, 253, 254, 255, 255,
];

/// `log2(x) * 256`, with an error below 1/256 for `x < 2^24`
pub(crate) fn log2_fixed8(x: u32) -> u32 {
    debug_assert!(x > 0);
    let hb = x.ilog2();
    let mantissa = if hb >= 8 {
        (x >> (hb - 8)) & 0xFF
    } else {
        (x << (8 - hb)) & 0xFF
    };
    (hb << 8) + u32::from(LOG2_FRACTION[mantissa as usize])
}

/// Size in bits of the table description of the given normalized probabilities
pub(crate) fn description_bits(probs: &[i16], table_log: u8) -> usize {
    let mut bits = 4;
    let probability_sum = 1usize << table_log;
    let mut probability_counter = 0usize;
    let mut prob_idx = 0;
    while probability_counter < probability_sum {
        let max_remaining_value = probability_sum - probability_counter + 1;
        let bits_to_write = max_remaining_value.ilog2() as usize + 1;
        let low_threshold = ((1 << bits_to_write) - 1) - max_remaining_value;
        let prob = probs[prob_idx];
        prob_idx += 1;
        let value = (prob + 1) as usize;
        bits += if value < low_threshold {
            bits_to_write - 1
        } else {
            bits_to_write
        };
        if prob == -1 {
            probability_counter += 1;
        } else if prob > 0 {
            probability_counter += prob as usize;
        } else {
            let mut zeros = 0;
            while probs[prob_idx] == 0 {
                zeros += 1;
                prob_idx += 1;
                if zeros == 3 {
                    bits += 2;
                    zeros = 0;
                }
            }
            bits += 2;
        }
    }
    bits.next_multiple_of(8)
}

fn write_description<V: AsMut<Vec<u8>>>(probs: &[i16], table_log: u8, writer: &mut BitWriter<V>) {
    writer.write_bits(table_log - 5, 4);
    let mut probability_counter = 0usize;
    let probability_sum = 1 << table_log;

    let mut prob_idx = 0;
    while probability_counter < probability_sum {
        let max_remaining_value = probability_sum - probability_counter + 1;
        let bits_to_write = max_remaining_value.ilog2() + 1;
        let low_threshold = ((1 << bits_to_write) - 1) - (max_remaining_value);
        let mask = (1 << (bits_to_write - 1)) - 1;

        let prob = probs[prob_idx];
        prob_idx += 1;
        let value = (prob + 1) as u32;
        if value < low_threshold as u32 {
            writer.write_bits(value, bits_to_write as usize - 1);
        } else if value > mask {
            writer.write_bits(value + low_threshold as u32, bits_to_write as usize);
        } else {
            writer.write_bits(value, bits_to_write as usize);
        }

        if prob == -1 {
            probability_counter += 1;
        } else if prob > 0 {
            probability_counter += prob as usize;
        } else {
            let mut zeros = 0u8;
            while probs[prob_idx] == 0 {
                zeros += 1;
                prob_idx += 1;
                if zeros == 3 {
                    writer.write_bits(3u8, 2);
                    zeros = 0;
                }
            }
            writer.write_bits(zeros, 2);
        }
    }
    writer.write_bits(0u8, writer.misaligned());
}

/// Build a table for the symbols in `data`
///
/// `avoid_0_numbit` is accepted for compatibility and has no effect: states that emit zero bits are valid and
/// decoded correctly, limiting the probability of the most frequent symbol only cost compression.
pub fn build_table_from_data(
    data: impl Iterator<Item = u8>,
    max_log: u8,
    avoid_0_numbit: bool,
) -> FSETable {
    let mut counts = [0u32; 256];
    for x in data {
        counts[x as usize] += 1;
    }
    let _ = avoid_0_numbit;
    build_table_from_counts(&counts, max_log)
}

/// Build a table for symbols with the given counts. At least one count must be nonzero.
pub(crate) fn build_table_from_counts(counts: &[u32], max_log: u8) -> FSETable {
    let (probs, table_log) = normalize(counts, max_log);
    build_table_from_probabilities(&probs, table_log)
}

/// Normalized probabilities for symbols with the given counts, padded to the length of `counts`, and their
/// accuracy log. If only one symbol occurs a second one with probability 1 is added, so the table can be described.
pub(crate) fn normalize(counts: &[u32], max_log: u8) -> (Vec<i16>, u8) {
    let num_symbols = counts.iter().rposition(|&c| c > 0).expect("no symbols") + 1;
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    let mut probs = alloc::vec![0i16; counts.len().max(2)];
    let used = counts.iter().filter(|&&c| c > 0).count();
    if used == 1 {
        // A table needs at least two symbols with a probability, give the unused neighbour the smallest one
        let symbol = num_symbols - 1;
        let table_log = MIN_TABLE_LOG;
        let other = if symbol == 0 { 1 } else { symbol - 1 };
        if probs.len() <= other {
            probs.resize(other + 1, 0);
        }
        probs[symbol] = (1 << table_log) - 1;
        probs[other] = 1;
        return (probs, table_log);
    }
    let table_log = optimal_table_log(max_log, total, num_symbols - 1);
    debug_assert!(used <= 1 << table_log);
    normalize_counts(
        &mut probs[..num_symbols],
        table_log,
        &counts[..num_symbols],
        total,
    );
    (probs, table_log)
}

/// The accuracy log for symbols with the given total count: large enough to represent the distribution, but not so
/// large that the table description costs more than it saves
fn optimal_table_log(max_log: u8, total: u64, max_symbol: usize) -> u8 {
    let max_log = max_log.min(MAX_TABLE_LOG);
    let total_log = (total - 1).max(1).ilog2() as u8;
    // A table larger than a quarter of the number of symbols to encode does not pay off
    let max_bits_src = total_log.saturating_sub(2);
    // The table must have room for every symbol
    let min_bits = u8::min(total_log + 1, (max_symbol.max(1).ilog2() + 2) as u8);
    let mut table_log = max_log;
    if max_bits_src < table_log {
        table_log = max_bits_src;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(MIN_TABLE_LOG, max_log.max(MIN_TABLE_LOG))
}

/// Scale the counts to a sum of `1 << table_log`, keeping every used symbol at a probability of at least one.
///
/// Probabilities are rounded down, except small ones, which are rounded up a little more eagerly because every
/// rounding error costs more at low probabilities. The difference is given to (or taken from) the most frequent
/// symbol. If that would distort its probability too much the slower `normalize_spread` is used instead.
fn normalize_counts(probs: &mut [i16], table_log: u8, counts: &[u32], total: u64) {
    /// Thresholds for rounding up probabilities below 8, in millionths of the probability step
    const ROUND_UP_THRESHOLD: [u64; 8] =
        [0, 473195, 504333, 520860, 550000, 700000, 750000, 830000];
    let scale = 62 - u32::from(table_log);
    let step = (1u64 << 62) / total;
    let v_step = 1u64 << (scale - 20);
    let low_threshold = total >> table_log;
    let mut still_to_distribute = 1i32 << table_log;
    let mut largest = 0;
    let mut largest_prob = 0i16;
    for (symbol, &count) in counts.iter().enumerate() {
        let count = u64::from(count);
        if count == 0 {
            probs[symbol] = 0;
            continue;
        }
        if count <= low_threshold {
            probs[symbol] = 1;
            still_to_distribute -= 1;
            continue;
        }
        let scaled = count * step;
        let mut prob = (scaled >> scale) as i16;
        if prob < 8 {
            let rest_to_beat = v_step * ROUND_UP_THRESHOLD[prob as usize];
            if scaled - ((prob as u64) << scale) > rest_to_beat {
                prob += 1;
            }
        }
        if prob > largest_prob {
            largest_prob = prob;
            largest = symbol;
        }
        probs[symbol] = prob;
        still_to_distribute -= i32::from(prob);
    }
    if -still_to_distribute >= i32::from(probs[largest] >> 1) {
        normalize_spread(probs, table_log, counts, total);
    } else {
        probs[largest] = (i32::from(probs[largest]) + still_to_distribute) as i16;
    }
}

/// Fallback for `normalize_counts` for flat distributions: symbols with small counts get probability 1, the
/// rest of the table is spread over the other symbols in proportion to their counts
fn normalize_spread(probs: &mut [i16], table_log: u8, counts: &[u32], mut total: u64) {
    const NOT_YET_ASSIGNED: i16 = -2;
    let mut distributed = 0u32;
    let low_threshold = total >> table_log;
    let mut low_one = (total * 3) >> (table_log + 1);
    for (symbol, &count) in counts.iter().enumerate() {
        let count = u64::from(count);
        if count == 0 {
            probs[symbol] = 0;
        } else if count <= low_threshold || count <= low_one {
            probs[symbol] = 1;
            distributed += 1;
            total -= count;
        } else {
            probs[symbol] = NOT_YET_ASSIGNED;
        }
    }
    let mut to_distribute = (1u32 << table_log) - distributed;
    if to_distribute == 0 {
        return;
    }
    if total / u64::from(to_distribute) > low_one {
        // The remaining symbols are still very unevenly distributed, give the small ones probability 1 too
        low_one = (total * 3) / (u64::from(to_distribute) * 2);
        for (symbol, &count) in counts.iter().enumerate() {
            let count = u64::from(count);
            if probs[symbol] == NOT_YET_ASSIGNED && count <= low_one {
                probs[symbol] = 1;
                distributed += 1;
                total -= count;
            }
        }
        to_distribute = (1u32 << table_log) - distributed;
    }
    if total == 0 {
        // Every symbol got probability 1, hand out the rest round robin
        let mut symbol = 0;
        while to_distribute > 0 {
            if probs[symbol] > 0 {
                probs[symbol] += 1;
                to_distribute -= 1;
            }
            symbol = (symbol + 1) % probs.len();
        }
        return;
    }
    if !probs.contains(&NOT_YET_ASSIGNED) {
        // Every symbol is small, give the rest to the most frequent one
        let max = (0..counts.len()).max_by_key(|&s| counts[s]).unwrap();
        probs[max] += to_distribute as i16;
        return;
    }
    let v_step_log = 62 - u32::from(table_log);
    let mid = (1u64 << (v_step_log - 1)) - 1;
    let r_step = (((1u128 << v_step_log) * u128::from(to_distribute) + u128::from(mid))
        / u128::from(total)) as u64;
    let mut tmp_total = mid;
    for (symbol, &count) in counts.iter().enumerate() {
        if probs[symbol] == NOT_YET_ASSIGNED {
            let end = tmp_total + u64::from(count) * r_step;
            let start_cell = tmp_total >> v_step_log;
            let end_cell = end >> v_step_log;
            let weight = (end_cell - start_cell) as i16;
            debug_assert!(weight >= 1);
            probs[symbol] = weight.max(1);
            tmp_total = end;
        }
    }
}

/// Build the encoding table for the given normalized probabilities (-1 means "less than one")
pub(crate) fn build_table_from_probabilities<P: Copy + Into<i32>>(
    probs: &[P],
    acc_log: u8,
) -> FSETable {
    assert!(acc_log <= MAX_TABLE_LOG);
    assert!(probs.len() <= 256);
    let table_size = 1usize << acc_log;
    let mut table = FSETable {
        table_log: acc_log,
        num_symbols: probs.len(),
        probs: [0; 256],
        state_table: alloc::vec![0; table_size],
        transforms: [SymbolTransform::default(); 256],
        table_size,
    };
    for (dst, &src) in table.probs.iter_mut().zip(probs) {
        *dst = src.into() as i16;
    }
    let probs = &table.probs[..probs.len()];
    let table_size_i = table_size as u32;

    // Where the states of each symbol start in `state_table`
    let mut cumul = [0u32; 257];
    // Spread the symbols over the table exactly as the decoder does: "less than one" symbols at the end, the
    // others in steps through the remaining positions
    let mut symbol_at = alloc::vec![0u8; table_size];
    let mut high_threshold = table_size - 1;
    for (symbol, &prob) in probs.iter().enumerate() {
        if prob == -1 {
            symbol_at[high_threshold] = symbol as u8;
            high_threshold = high_threshold.wrapping_sub(1);
            cumul[symbol + 1] = cumul[symbol] + 1;
        } else {
            cumul[symbol + 1] = cumul[symbol] + prob.max(0) as u32;
        }
    }
    assert_eq!(
        cumul[probs.len()] as usize,
        table_size,
        "invalid probabilities"
    );
    let step = (table_size >> 1) + (table_size >> 3) + 3;
    let mask = table_size - 1;
    let mut position = 0;
    for (symbol, &prob) in probs.iter().enumerate() {
        for _ in 0..prob.max(0) {
            symbol_at[position] = symbol as u8;
            position = (position + step) & mask;
            while position > high_threshold {
                position = (position + step) & mask;
            }
        }
    }
    debug_assert_eq!(position, 0);

    // The states of a symbol are ordered by their position in the decoding table
    for (u, &symbol) in symbol_at.iter().enumerate() {
        let slot = &mut cumul[symbol as usize];
        table.state_table[*slot as usize] = (table_size_i + u as u32) as u16;
        *slot += 1;
    }

    let mut total = 0i32;
    for (symbol, &prob) in probs.iter().enumerate() {
        let tt = &mut table.transforms[symbol];
        match prob {
            0 => {
                // Not encodable, only set for completeness
                tt.delta_nb_bits = ((u32::from(acc_log) + 1) << 16) - table_size as u32;
            }
            -1 | 1 => {
                tt.delta_nb_bits = (u32::from(acc_log) << 16) - table_size as u32;
                tt.delta_find_state = total - 1;
                total += 1;
            }
            _ => {
                let prob = prob as u32;
                let max_bits_out = u32::from(acc_log) - (prob - 1).ilog2();
                let min_state_plus = prob << max_bits_out;
                tt.delta_nb_bits = (max_bits_out << 16) - min_state_plus;
                tt.delta_find_state = total - prob as i32;
                total += prob as i32;
            }
        }
    }
    table
}

pub(crate) const ML_DIST: &[i16] = &[
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

pub(crate) const LL_DIST: &[i16] = &[
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

pub(crate) const OF_DIST: &[i16] = &[
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

pub(crate) fn default_ml_table() -> FSETable {
    build_table_from_probabilities(ML_DIST, 6)
}

pub(crate) fn default_ll_table() -> FSETable {
    build_table_from_probabilities(LL_DIST, 6)
}

pub(crate) fn default_of_table() -> FSETable {
    build_table_from_probabilities(OF_DIST, 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(probs: &[i16]) -> i32 {
        probs
            .iter()
            .map(|&p| i32::from(p.max(0)) + i32::from(p == -1))
            .sum()
    }

    #[test]
    fn single_symbol_zero_builds_a_table() {
        // A block in which every sequence has e.g. zero literals produces only the symbol 0.
        for count in [1, 2, 17, 1000] {
            let table = build_table_from_data(core::iter::repeat_n(0u8, count), 9, true);
            assert!(table.table_size >= 32);
            assert!(table.probability(0) > 0);
        }
        let table = build_table_from_data(core::iter::repeat_n(0u8, 5), 9, false);
        assert!(table.probability(0) > 0);
    }

    #[test]
    fn normalized_counts_sum_to_the_table_size() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for round in 0..2000 {
            let n = 2 + round % 60;
            let mut counts = alloc::vec![0u32; n];
            for c in counts.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Mix of flat, skewed and sparse distributions
                *c = match round % 4 {
                    0 => (state % 1000) as u32,
                    1 => (state % 3) as u32,
                    2 => ((state % 64) as u32).pow(3),
                    _ => (state & 1) as u32 * 5000 + (state % 2) as u32,
                };
            }
            if counts.iter().filter(|&&c| c > 0).count() < 1 {
                continue;
            }
            for max_log in [6, 8, 9, 12] {
                let (probs, table_log) = normalize(&counts, max_log);
                assert!(table_log <= max_log.max(MIN_TABLE_LOG));
                assert_eq!(sum(&probs), 1 << table_log, "{:?}", counts);
                for (s, &c) in counts.iter().enumerate() {
                    assert!(c == 0 || probs[s] != 0, "{:?}", counts);
                }
                // The table can be built and encodes every used symbol
                let table = build_table_from_probabilities(&probs, table_log);
                assert!(table.cost(&counts).is_some());
            }
        }
    }

    #[test]
    fn proportional_probabilities() {
        // Counts that are an exact multiple of the table size are reproduced exactly
        let counts = [1024u32, 512, 256, 128, 64, 32, 16, 16];
        let (probs, table_log) = normalize(&counts, 7);
        assert_eq!(table_log, 7);
        assert_eq!(&probs[..], &[64, 32, 16, 8, 4, 2, 1, 1]);
    }

    #[test]
    fn description_bits_match_the_written_description() {
        let counts = [100u32, 0, 0, 0, 5, 3, 0, 0, 0, 0, 0, 1, 80, 2];
        let (probs, table_log) = normalize(&counts, 9);
        let table = build_table_from_probabilities(&probs, table_log);
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        table.write_table(&mut writer);
        writer.flush();
        assert_eq!(description_bits(&probs, table_log), out.len() * 8);
    }

    #[test]
    fn fixed_point_log2() {
        for x in [1u32, 2, 3, 5, 100, 255, 256, 257, 1000, 65535, 1 << 20] {
            let exact = (x as f64).log2() * 256.0;
            assert!((log2_fixed8(x) as f64 - exact).abs() <= 1.0, "{}", x);
        }
    }
}
