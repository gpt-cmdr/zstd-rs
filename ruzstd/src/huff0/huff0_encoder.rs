use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::{
    bit_io::BitWriter,
    fse::fse_encoder::{self, FSEEncoder},
};

pub(crate) struct HuffmanEncoder<'output, 'table, V: AsMut<Vec<u8>>> {
    table: &'table HuffmanTable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> HuffmanEncoder<'_, '_, V> {
    pub fn new<'o, 't>(
        table: &'t HuffmanTable,
        writer: &'o mut BitWriter<V>,
    ) -> HuffmanEncoder<'o, 't, V> {
        HuffmanEncoder { table, writer }
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Padding bits to fill up last byte
    pub fn encode(&mut self, data: &[u8], with_table: bool) {
        if with_table {
            self.write_table();
        }
        Self::encode_stream(self.table, self.writer, data);
    }

    /// Encodes the data using the provided table in 4 concatenated streams
    /// Writes
    /// * Table description
    /// * Jumptable
    /// * Encoded data in 4 streams, each padded to fill the last byte
    pub fn encode4x(&mut self, data: &[u8], with_table: bool) {
        assert!(data.len() >= 4);

        // Split data in 4 equally sized parts (the last one might be a bit smaller than the rest)
        let split_size = data.len().div_ceil(4);
        let src1 = &data[..split_size];
        let src2 = &data[split_size..split_size * 2];
        let src3 = &data[split_size * 2..split_size * 3];
        let src4 = &data[split_size * 3..];

        // Write table description
        if with_table {
            self.write_table();
        }

        // Reserve space for the jump table, will be changed later
        let size_idx = self.writer.index();
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);

        // Write the 4 streams, noting the sizes of the encoded streams
        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src1);
        let size1 = (self.writer.index() - index_before) / 8;

        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src2);
        let size2 = (self.writer.index() - index_before) / 8;

        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src3);
        let size3 = (self.writer.index() - index_before) / 8;

        Self::encode_stream(self.table, self.writer, src4);

        // Sanity check, if this doesn't hold we produce a broken stream
        assert!(size1 <= u16::MAX as usize);
        assert!(size2 <= u16::MAX as usize);
        assert!(size3 <= u16::MAX as usize);

        // Update the jumptable with the real sizes
        self.writer.change_bits(size_idx, size1 as u16, 16);
        self.writer.change_bits(size_idx + 16, size2 as u16, 16);
        self.writer.change_bits(size_idx + 32, size3 as u16, 16);
    }

    /// Encode one stream and pad it to fill the last byte
    fn encode_stream<VV: AsMut<Vec<u8>>>(
        table: &HuffmanTable,
        writer: &mut BitWriter<VV>,
        data: &[u8],
    ) {
        // Each code with its number of bits, in sixteen bits (a code has at most eleven)
        let mut codes = [0u16; 256];
        for (packed, &(code, num_bits)) in codes.iter_mut().zip(&table.codes) {
            debug_assert!(num_bits <= 11 && code >> num_bits == 0);
            *packed = ((code as u16) << 4) | u16::from(num_bits);
        }
        writer.add_codes_reversed(data, &codes);

        let bits_to_fill = writer.misaligned();
        if bits_to_fill == 0 {
            writer.write_bits(1u32, 8);
        } else {
            writer.write_bits(1u32, bits_to_fill);
        }
    }

    pub(super) fn weights(&self) -> Vec<u8> {
        table_weights(self.table)
    }

    fn write_table(&mut self) {
        // TODO strategy for determining this?
        let weights = self.weights();
        let weights = &weights[..weights.len() - 1]; // dont encode last weight
        let mut direct = weights.len() <= 16;
        if !direct {
            let size_idx = self.writer.index();
            self.writer.write_bits(0u8, 8);
            let idx_before = self.writer.index();
            fse_compress_weights(weights, self.writer);
            let encoded_len = (self.writer.index() - idx_before) / 8;
            if encoded_len < 128 {
                self.writer.change_bits(size_idx, encoded_len as u8, 8);
            } else {
                // Too long for the header, callers check `can_be_described` so the weights can be written directly
                assert!(weights.len() <= 128);
                self.writer.reset_to(size_idx);
                direct = true;
            }
        }
        if direct {
            self.writer.write_bits(weights.len() as u8 + 127, 8);
            let pairs = weights.chunks_exact(2);
            let remainder = pairs.remainder();
            for pair in pairs.into_iter() {
                let weight1 = pair[0];
                let weight2 = pair[1];
                assert!(weight1 < 16);
                assert!(weight2 < 16);
                self.writer.write_bits(weight2, 4);
                self.writer.write_bits(weight1, 4);
            }
            if !remainder.is_empty() {
                let weight = remainder[0];
                assert!(weight < 16);
                self.writer.write_bits(weight << 4, 8);
            }
        }
    }
}

pub struct HuffmanTable {
    /// Index is the symbol, values are the bitstring in the lower bits of the u32 and the amount of bits in the u8
    codes: Vec<(u32, u8)>,
}

impl HuffmanTable {
    pub fn build_from_data(data: &[u8]) -> Self {
        // Four tables, a byte in four counted in each: counting a symbol again right away then does not wait
        // for the count just written
        let mut parts = [[0u32; 256]; 4];
        let mut chunks = data.chunks_exact(4);
        for chunk in &mut chunks {
            parts[0][chunk[0] as usize] += 1;
            parts[1][chunk[1] as usize] += 1;
            parts[2][chunk[2] as usize] += 1;
            parts[3][chunk[3] as usize] += 1;
        }
        for x in chunks.remainder() {
            parts[0][*x as usize] += 1;
        }
        let mut counts = [0usize; 256];
        let mut max = 0;
        for (symbol, count) in counts.iter_mut().enumerate() {
            *count = parts.iter().map(|part| part[symbol] as usize).sum();
            if *count != 0 {
                max = symbol;
            }
        }

        Self::build_from_counts(&counts[..=max])
    }

    /// Build an optimal prefix code for symbols with the given counts, limited to the maximum code length of the
    /// format. Symbols with a count of zero get no code. At least two symbols must have a nonzero count.
    pub fn build_from_counts(counts: &[usize]) -> Self {
        assert!(counts.len() <= 256);
        let lengths = code_lengths(counts, MAX_CODE_LEN);
        let max = lengths.iter().copied().max().unwrap() as usize;
        let weights = lengths
            .iter()
            .map(|&len| if len == 0 { 0 } else { max + 1 - len as usize })
            .collect::<Vec<_>>();
        Self::build_from_weights(&weights)
    }

    /// Whether the description of this table fits into a literals section header. The weights are FSE compressed
    /// into at most 127 bytes, or written directly if there are at most 128 of them.
    pub(crate) fn can_be_described(&self) -> bool {
        let weights = table_weights(self);
        let weights = &weights[..weights.len() - 1];
        if weights.len() <= 128 {
            return true;
        }
        let mut scratch = Vec::new();
        let mut writer = BitWriter::from(&mut scratch);
        fse_compress_weights(weights, &mut writer);
        writer.flush();
        scratch.len() < 128
    }

    pub fn build_from_weights(weights: &[usize]) -> Self {
        let mut sorted = Vec::with_capacity(weights.len());
        struct SortEntry {
            symbol: u8,
            weight: usize,
        }

        // TODO this doesn't need to be a temporary Vec, it could be done in a [_; 264]
        // only non-zero weights are interesting here
        for (symbol, weight) in weights.iter().copied().enumerate() {
            if weight > 0 {
                sorted.push(SortEntry {
                    symbol: symbol as u8,
                    weight,
                });
            }
        }
        // We process symbols ordered by weight and then ordered by symbol
        sorted.sort_by(|left, right| match left.weight.cmp(&right.weight) {
            Ordering::Equal => left.symbol.cmp(&right.symbol),
            other => other,
        });

        // Prepare huffman table with placeholders
        let mut table = HuffmanTable {
            codes: Vec::with_capacity(weights.len()),
        };
        for _ in 0..weights.len() {
            table.codes.push((0, 0));
        }

        // Determine the number of bits needed for codes with the lowest weight
        let weight_sum = sorted.iter().map(|e| 1 << (e.weight - 1)).sum::<usize>();
        if !weight_sum.is_power_of_two() {
            panic!("This is an internal error");
        }
        let max_num_bits = highest_bit_set(weight_sum) - 1; // this is a log_2 of a clean power of two

        // Starting at the symbols with the lowest weight we update the placeholders in the table
        let mut current_code = 0;
        let mut current_weight = 0;
        let mut current_num_bits = 0;
        for entry in sorted.iter() {
            // If the entry isn't the same weight as the last one we need to change a few things
            if current_weight != entry.weight {
                // The code shifts by the difference of the weights to allow for enough unique values
                current_code >>= entry.weight - current_weight;
                // Encoding a symbol of this weight will take less bits than the previous weight
                current_num_bits = max_num_bits - entry.weight + 1;
                // Run the next update when the weight changes again
                current_weight = entry.weight;
            }
            table.codes[entry.symbol as usize] = (current_code as u32, current_num_bits as u8);
            current_code += 1;
        }

        table
    }

    pub fn can_encode(&self, other: &Self) -> Option<usize> {
        if other.codes.len() > self.codes.len() {
            return None;
        }
        let mut sum = 0;
        for ((_, other_num_bits), (_, self_num_bits)) in other.codes.iter().zip(self.codes.iter()) {
            if *other_num_bits != 0 && *self_num_bits == 0 {
                return None;
            }
            sum += other_num_bits.abs_diff(*self_num_bits) as usize;
        }
        Some(sum)
    }
}

/// Assert that the provided value is greater than zero, and returns index of the first set bit
fn highest_bit_set(x: usize) -> usize {
    assert!(x > 0);
    usize::BITS as usize - x.leading_zeros() as usize
}

/// The weights of the symbols of a table as they are written into the table description
fn table_weights(table: &HuffmanTable) -> Vec<u8> {
    let max = table.codes.iter().map(|(_, nb)| nb).max().unwrap();
    table
        .codes
        .iter()
        .copied()
        .map(|(_, nb)| if nb == 0 { 0 } else { max - nb + 1 })
        .collect::<Vec<u8>>()
}

/// FSE compress the weights of a table description (without the header byte)
fn fse_compress_weights<V: AsMut<Vec<u8>>>(weights: &[u8], writer: &mut BitWriter<V>) {
    let mut encoder = FSEEncoder::new(
        fse_encoder::build_table_from_data(weights.iter().copied(), 6, true),
        writer,
    );
    encoder.encode_interleaved(weights);
}

/// The maximum length of a Huffman code in Zstandard
const MAX_CODE_LEN: u8 = 11;

/// Lengths of an optimal prefix code (Huffman code) for symbols with the given counts, with no code longer than
/// `max_len` bits. Symbols with a count of zero get the length 0. The code is complete (its Kraft sum is exactly 1),
/// which the table description of the format requires.
///
/// The code is built with the two-queue variant of Huffman's algorithm. If it is too deep the long codes are cut to
/// `max_len`, the codes of the least frequent symbols that are still shorter are lengthened until the code is a
/// valid prefix code again, and the space that is left is used to shorten the codes of the most frequent symbols.
fn code_lengths(counts: &[usize], max_len: u8) -> Vec<u8> {
    // (count, symbol) of all used symbols, least frequent first
    let mut leaves: Vec<(usize, usize)> = counts
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, count)| *count > 0)
        .map(|(symbol, count)| (count, symbol))
        .collect();
    assert!(
        leaves.len() >= 2,
        "a Huffman code needs at least two symbols"
    );
    assert!(leaves.len() <= 1 << max_len);
    leaves.sort_unstable();
    let n = leaves.len();

    // Nodes 0..n are the leaves, the following ones the inner nodes in the order they are created. Their weights do
    // not decrease, so the two lightest nodes are always at the front of one of the two queues.
    let mut weight: Vec<usize> = leaves.iter().map(|(count, _)| *count).collect();
    weight.reserve(n - 1);
    let mut parent = alloc::vec![0usize; 2 * n - 1];
    let mut next_leaf = 0;
    let mut next_inner = n;
    let mut take_lightest = |weight: &[usize]| {
        if next_leaf < n && (next_inner == weight.len() || weight[next_leaf] <= weight[next_inner])
        {
            next_leaf += 1;
            next_leaf - 1
        } else {
            next_inner += 1;
            next_inner - 1
        }
    };
    for _ in 0..n - 1 {
        let a = take_lightest(&weight);
        let b = take_lightest(&weight);
        parent[a] = weight.len();
        parent[b] = weight.len();
        weight.push(weight[a] + weight[b]);
    }
    // A parent is always created after its children, so walking backwards from the root visits it first
    let mut depth = alloc::vec![0u32; 2 * n - 1];
    for node in (0..2 * n - 2).rev() {
        depth[node] = depth[parent[node]] + 1;
    }
    let mut lengths = depth[..n].to_vec();

    let max_len = u32::from(max_len);
    if lengths[0] > max_len {
        // The least frequent symbol has the longest code
        let kraft_target = 1u64 << max_len;
        for len in lengths.iter_mut() {
            *len = (*len).min(max_len);
        }
        let mut kraft: u64 = lengths.iter().map(|len| 1u64 << (max_len - len)).sum();
        while kraft > kraft_target {
            for len in lengths.iter_mut() {
                if *len < max_len {
                    kraft -= 1 << (max_len - *len - 1);
                    *len += 1;
                    if kraft <= kraft_target {
                        break;
                    }
                }
            }
        }
        // Lengthening may have overshot, give the space back to the most frequent symbols
        for len in lengths.iter_mut().rev() {
            while *len > 1 && kraft + (1 << (max_len - *len)) <= kraft_target {
                kraft += 1 << (max_len - *len);
                *len -= 1;
            }
        }
        debug_assert_eq!(kraft, kraft_target);
    }

    let mut by_symbol = alloc::vec![0u8; counts.len()];
    for ((_, symbol), len) in leaves.iter().zip(lengths) {
        by_symbol[*symbol] = len as u8;
    }
    by_symbol
}

#[test]
fn huffman() {
    let table = HuffmanTable::build_from_weights(&[2, 2, 2, 1, 1]);
    assert_eq!(table.codes[0], (1, 2));
    assert_eq!(table.codes[1], (2, 2));
    assert_eq!(table.codes[2], (3, 2));
    assert_eq!(table.codes[3], (0, 3));
    assert_eq!(table.codes[4], (1, 3));

    let table = HuffmanTable::build_from_weights(&[4, 3, 2, 0, 1, 1]);
    assert_eq!(table.codes[0], (1, 1));
    assert_eq!(table.codes[1], (1, 2));
    assert_eq!(table.codes[2], (1, 3));
    assert_eq!(table.codes[3], (0, 0));
    assert_eq!(table.codes[4], (0, 4));
    assert_eq!(table.codes[5], (1, 4));
}

#[test]
fn counts() {
    let counts = &[3, 0, 4, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert!(table[3].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[4].1);

    let counts = &[3, 0, 4, 0, 7, 2, 2, 2, 0, 2, 2, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert_eq!(table[3].1, 0);
    assert_eq!(table[8].1, 0);
    assert!(table[11].1 >= table[5].1);
    assert!(table[5].1 >= table[6].1);
    assert!(table[6].1 >= table[7].1);
    assert!(table[7].1 >= table[9].1);
    assert!(table[9].1 >= table[10].1);
    assert!(table[10].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[12].1);
    assert!(table[12].1 >= table[4].1);
}

#[test]
fn from_data() {
    let counts = &[3, 0, 4, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    // The same counts as above
    let data = &[0, 2, 4, 4, 0, 3, 2, 2, 0, 2, 4, 4, 4];
    let table2 = HuffmanTable::build_from_data(data).codes;

    assert_eq!(table, table2);
}

#[cfg(test)]
fn assert_valid_lengths(counts: &[usize], lengths: &[u8]) {
    assert_eq!(counts.len(), lengths.len());
    let mut kraft = 0u64;
    for (count, len) in counts.iter().zip(lengths) {
        assert_eq!(*count == 0, *len == 0, "only used symbols get a code");
        assert!(*len <= MAX_CODE_LEN);
        if *len > 0 {
            kraft += 1 << (MAX_CODE_LEN - len);
        }
    }
    assert_eq!(kraft, 1 << MAX_CODE_LEN, "the code must be complete");
    for (c1, l1) in counts.iter().zip(lengths) {
        for (c2, l2) in counts.iter().zip(lengths) {
            if *c1 > 0 && *c2 > 0 && c1 < c2 {
                assert!(
                    l1 >= l2,
                    "a less frequent symbol must not get a shorter code"
                );
            }
        }
    }
}

#[test]
fn code_lengths_are_optimal_without_limit() {
    // Classic example: the Huffman code for these counts has lengths 1, 2, 3, 4, 4 (or an equivalent code)
    let counts = [45, 13, 12, 16, 9, 5];
    let lengths = code_lengths(&counts, MAX_CODE_LEN);
    assert_valid_lengths(&counts, &lengths);
    let bits: usize = counts
        .iter()
        .zip(&lengths)
        .map(|(c, l)| c * *l as usize)
        .sum();
    assert_eq!(bits, 224);

    // Uniform counts give a balanced code
    let lengths = code_lengths(&[7; 256], MAX_CODE_LEN);
    assert!(lengths.iter().all(|len| *len == 8));
}

#[test]
fn code_lengths_respect_the_limit() {
    // Fibonacci counts produce the deepest possible Huffman tree
    let mut counts = alloc::vec![1usize, 1];
    while counts.len() < 40 {
        let next = counts[counts.len() - 1] + counts[counts.len() - 2];
        counts.push(next);
    }
    let lengths = code_lengths(&counts, MAX_CODE_LEN);
    assert_valid_lengths(&counts, &lengths);
    assert_eq!(*lengths.iter().max().unwrap(), MAX_CODE_LEN);

    // All 256 symbols, one of them very frequent, many rare
    let mut counts = alloc::vec![1usize; 256];
    counts[3] = 1_000_000;
    counts[200] = 50_000;
    let lengths = code_lengths(&counts, MAX_CODE_LEN);
    assert_valid_lengths(&counts, &lengths);
}

#[test]
fn code_lengths_of_random_counts() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for round in 0..300 {
        let symbols = 2 + round % 255;
        let mut counts = alloc::vec![0usize; symbols];
        for count in counts.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Mostly skewed counts, some unused symbols
            let shift = state % 20;
            *count = if state % 7 < 1 {
                0
            } else {
                (state >> 40) as usize >> shift
            };
        }
        counts[0] = counts[0].max(1);
        counts[symbols - 1] = counts[symbols - 1].max(1);
        let lengths = code_lengths(&counts, MAX_CODE_LEN);
        assert_valid_lengths(&counts, &lengths);

        // Without hitting the limit a Huffman code is less than one bit per symbol above the entropy
        if *lengths.iter().max().unwrap() < MAX_CODE_LEN {
            let total: usize = counts.iter().sum();
            let bits: f64 = counts
                .iter()
                .zip(&lengths)
                .map(|(c, l)| (*c * *l as usize) as f64)
                .sum();
            let entropy: f64 = counts
                .iter()
                .filter(|c| **c > 0)
                .map(|c| {
                    let p = *c as f64 / total as f64;
                    -(*c as f64) * libm_log2(p)
                })
                .sum();
            assert!(
                bits < entropy + total as f64,
                "{} bits for an entropy of {}",
                bits,
                entropy
            );
        }

        // The table built from the lengths is a prefix code
        let codes = HuffmanTable::build_from_counts(&counts).codes;
        for (i, (code, num_bits)) in codes.iter().copied().enumerate() {
            assert_eq!(num_bits, lengths[i]);
            for (code2, num_bits2) in codes.iter().copied() {
                if num_bits == 0 || num_bits2 == 0 || (code, num_bits) == (code2, num_bits2) {
                    continue;
                }
                if num_bits <= num_bits2 {
                    assert_ne!(code, code2 >> (num_bits2 - num_bits));
                }
            }
        }
    }
}

/// log2 for the tests, which also run without `std`
#[cfg(test)]
fn libm_log2(x: f64) -> f64 {
    // x is in (0, 1]: split into mantissa and exponent, then use a series for ln of the mantissa
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let mantissa = f64::from_bits((bits & ((1 << 52) - 1)) | (1023 << 52));
    let y = (mantissa - 1.0) / (mantissa + 1.0);
    let mut term = y;
    let mut sum = 0.0;
    for k in 0..40 {
        sum += term / (2 * k + 1) as f64;
        term *= y * y;
    }
    exponent as f64 + 2.0 * sum / core::f64::consts::LN_2
}
