//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::{BackBits, BitReaderReversed};
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;
use core::convert::TryInto;

/// The Zstandard specification limits the maximum length of a code to 11 bits.
pub(crate) const MAX_MAX_NUM_BITS: u8 = 11;

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    decode: Vec<Entry>,
    /// `decode` widened to [`MAX_MAX_NUM_BITS`] bits, each entry a symbol (low byte) and its
    /// number of bits (high byte): the table of [`HuffmanTable::decode_stream`] and
    /// [`HuffmanTable::decode_four_streams`], indexed by the next 11 bits of a stream.
    fast: Vec<u16>,
    /// The weight of a symbol is the number of occurences in a table.
    /// This value is used in constructing a binary tree referred to as
    /// a Huffman tree. Once this tree is constructed, it can be used to build the
    /// lookup table
    weights: Vec<u8>,
    /// The maximum size in bits a prefix code in the encoded data can be.
    /// This value is used so that the decoder knows how many bits
    /// to read from the bitstream before checking the table. This
    /// value must be 11 or lower.
    pub max_num_bits: u8,
    bits: Vec<u8>,
    bit_ranks: Vec<u32>,
    rank_indexes: Vec<usize>,
    /// In some cases, the list of weights is compressed using FSE compression.
    fse_table: FSETable,
}

impl HuffmanTable {
    /// Create a new, empty table.
    pub fn new() -> HuffmanTable {
        HuffmanTable {
            decode: Vec::new(),
            fast: Vec::new(),

            weights: Vec::with_capacity(256),
            max_num_bits: 0,
            bits: Vec::with_capacity(256),
            bit_ranks: Vec::with_capacity(11),
            rank_indexes: Vec::with_capacity(11),
            fse_table: FSETable::new(255),
        }
    }

    /// Completely empty the table then repopulate as a replica
    /// of `other`.
    pub fn reinit_from(&mut self, other: &Self) {
        self.reset();
        self.decode.extend_from_slice(&other.decode);
        self.fast.extend_from_slice(&other.fast);
        self.weights.extend_from_slice(&other.weights);
        self.max_num_bits = other.max_num_bits;
        self.bits.extend_from_slice(&other.bits);
        self.rank_indexes.extend_from_slice(&other.rank_indexes);
        self.fse_table.reinit_from(&other.fse_table);
    }

    /// Completely empty the table of all data.
    pub fn reset(&mut self) {
        self.decode.clear();
        self.fast.clear();
        self.weights.clear();
        self.max_num_bits = 0;
        self.bits.clear();
        self.bit_ranks.clear();
        self.rank_indexes.clear();
        self.fse_table.reset();
    }

    /// Read from `source` and decode the input, populating the huffman decoding table.
    ///
    /// Returns the number of bytes read.
    pub fn build_decoder(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        self.decode.clear();

        let bytes_used = self.read_weights(source)?;
        self.build_table_from_weights()?;
        Ok(bytes_used)
    }

    /// Read weights from the provided source.
    ///
    /// The huffman table is represented in the input data as a list of weights.
    /// After the header, weights are read, then a Huffman decoding table
    /// can be constructed using that list of weights.
    ///
    /// Returns the number of bytes read.
    fn read_weights(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        use HuffmanTableError as err;

        if source.is_empty() {
            return Err(err::SourceIsEmpty);
        }
        let header = source[0];
        let mut bits_read = 8;

        match header {
            // If the header byte is less than 128, the series of weights
            // is compressed using two interleaved FSE streams that share
            // a distribution table.
            0..=127 => {
                let fse_stream = &source[1..];
                if header as usize > fse_stream.len() {
                    return Err(err::NotEnoughBytesForWeights {
                        got_bytes: fse_stream.len(),
                        expected_bytes: header,
                    });
                }
                //fse decompress weights
                let bytes_used_by_fse_header = self.fse_table.build_decoder(fse_stream, 6)?;

                if bytes_used_by_fse_header > header as usize {
                    return Err(err::FSETableUsedTooManyBytes {
                        used: bytes_used_by_fse_header,
                        available_bytes: header,
                    });
                }

                vprintln!(
                    "Building fse table for huffman weights used: {}",
                    bytes_used_by_fse_header
                );
                // Huffman headers are compressed using two interleaved
                // FSE bitstreams, where the first state (decoder) handles
                // even symbols, and the second handles odd symbols.
                let mut dec1 = FSEDecoder::new(&self.fse_table);
                let mut dec2 = FSEDecoder::new(&self.fse_table);

                let compressed_start = bytes_used_by_fse_header;
                let compressed_length = header as usize - bytes_used_by_fse_header;

                let compressed_weights = &fse_stream[compressed_start..];
                if compressed_weights.len() < compressed_length {
                    return Err(err::NotEnoughBytesToDecompressWeights {
                        have: compressed_weights.len(),
                        need: compressed_length,
                    });
                }
                let compressed_weights = &compressed_weights[..compressed_length];
                let mut br = BitReaderReversed::new(compressed_weights);

                bits_read += (bytes_used_by_fse_header + compressed_length) * 8;

                //skip the 0 padding at the end of the last byte of the bit stream and throw away the first 1 found
                let mut skipped_bits = 0;
                loop {
                    let val = br.get_bits(1);
                    skipped_bits += 1;
                    if val == 1 || skipped_bits > 8 {
                        break;
                    }
                }
                if skipped_bits > 8 {
                    //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
                    return Err(err::ExtraPadding { skipped_bits });
                }

                dec1.init_state(&mut br)?;
                dec2.init_state(&mut br)?;

                self.weights.clear();

                // The two decoders take turns decoding a single symbol and updating their state.
                loop {
                    let w = dec1.decode_symbol();
                    self.weights.push(w);
                    dec1.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec2.decode_symbol());
                        break;
                    }

                    let w = dec2.decode_symbol();
                    self.weights.push(w);
                    dec2.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec1.decode_symbol());
                        break;
                    }
                    //maximum number of weights is 255 because we use u8 symbols and the last weight is inferred from the sum of all others
                    if self.weights.len() > 255 {
                        return Err(err::TooManyWeights {
                            got: self.weights.len(),
                        });
                    }
                }
            }
            // If the header byte is greater than or equal to 128,
            // weights are directly represented, where each weight is
            // encoded directly as a 4 bit field. The weights will
            // always be encoded with full bytes, meaning if there's
            // an odd number of weights, the last weight will still
            // occupy a full byte.
            _ => {
                // weights are directly encoded
                let weights_raw = &source[1..];
                let num_weights = header - 127;
                self.weights.resize(num_weights as usize, 0);

                let bytes_needed = if num_weights.is_multiple_of(2) {
                    num_weights as usize / 2
                } else {
                    (num_weights as usize / 2) + 1
                };

                if weights_raw.len() < bytes_needed {
                    return Err(err::NotEnoughBytesInSource {
                        got: weights_raw.len(),
                        need: bytes_needed,
                    });
                }

                for idx in 0..num_weights {
                    if idx % 2 == 0 {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] >> 4;
                    } else {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] & 0xF;
                    }
                    bits_read += 4;
                }
            }
        }

        let bytes_read = if bits_read % 8 == 0 {
            bits_read / 8
        } else {
            (bits_read / 8) + 1
        };
        Ok(bytes_read as u32)
    }

    /// Once the weights have been read from the data, you can decode the weights
    /// into a table, and use that table to decode the actual compressed data.
    ///
    /// This function populates the rest of the table from the series of weights.
    fn build_table_from_weights(&mut self) -> Result<(), HuffmanTableError> {
        use HuffmanTableError as err;

        self.bits.clear();
        self.bits.resize(self.weights.len() + 1, 0);

        let mut weight_sum: u32 = 0;
        for w in &self.weights {
            if *w > MAX_MAX_NUM_BITS {
                return Err(err::WeightBiggerThanMaxNumBits { got: *w });
            }
            weight_sum += if *w > 0 { 1_u32 << (*w - 1) } else { 0 };
        }

        if weight_sum == 0 {
            return Err(err::MissingWeights);
        }

        let max_bits = highest_bit_set(weight_sum) as u8;
        let left_over = (1 << max_bits) - weight_sum;

        //left_over must be power of two
        if !left_over.is_power_of_two() {
            return Err(err::LeftoverIsNotAPowerOf2 { got: left_over });
        }

        let last_weight = highest_bit_set(left_over) as u8;

        for symbol in 0..self.weights.len() {
            let bits = if self.weights[symbol] > 0 {
                max_bits + 1 - self.weights[symbol]
            } else {
                0
            };
            self.bits[symbol] = bits;
        }

        self.bits[self.weights.len()] = max_bits + 1 - last_weight;
        self.max_num_bits = max_bits;

        if max_bits > MAX_MAX_NUM_BITS {
            return Err(err::MaxBitsTooHigh { got: max_bits });
        }

        self.bit_ranks.clear();
        self.bit_ranks.resize((max_bits + 1) as usize, 0);
        for num_bits in &self.bits {
            self.bit_ranks[(*num_bits) as usize] += 1;
        }

        //fill with dummy symbols
        self.decode.resize(
            1 << self.max_num_bits,
            Entry {
                symbol: 0,
                num_bits: 0,
            },
        );

        //starting codes for each rank
        self.rank_indexes.clear();
        self.rank_indexes.resize((max_bits + 1) as usize, 0);

        self.rank_indexes[max_bits as usize] = 0;
        for bits in (1..self.rank_indexes.len() as u8).rev() {
            self.rank_indexes[bits as usize - 1] = self.rank_indexes[bits as usize]
                + self.bit_ranks[bits as usize] as usize * (1 << (max_bits - bits));
        }

        assert!(
            self.rank_indexes[0] == self.decode.len(),
            "rank_idx[0]: {} should be: {}",
            self.rank_indexes[0],
            self.decode.len()
        );

        for symbol in 0..self.bits.len() {
            let bits_for_symbol = self.bits[symbol];
            if bits_for_symbol != 0 {
                // allocate code for the symbol and set in the table
                // a code ignores all max_bits - bits[symbol] bits, so it gets
                // a range that spans all of those in the decoding table
                let base_idx = self.rank_indexes[bits_for_symbol as usize];
                let len = 1 << (max_bits - bits_for_symbol);
                self.rank_indexes[bits_for_symbol as usize] += len;
                for idx in 0..len {
                    self.decode[base_idx + idx].symbol = symbol as u8;
                    self.decode[base_idx + idx].num_bits = bits_for_symbol;
                }
            }
        }

        // Each entry repeated to fill an 11-bit table.
        let repeat = 1usize << (MAX_MAX_NUM_BITS - max_bits);
        self.fast.clear();
        self.fast.reserve(FAST_TABLE_SIZE);
        for entry in &self.decode {
            let packed = u16::from(entry.symbol) | (u16::from(entry.num_bits) << 8);
            self.fast.extend(core::iter::repeat_n(packed, repeat));
        }

        Ok(())
    }

    /// Decodes one stream of Huffman codes into exactly `out`, or `None` when the stream is not
    /// exactly that many symbols (or there is no table).
    pub(crate) fn decode_stream(&self, src: &[u8], out: &mut [u8]) -> Option<()> {
        let table: &[u16; FAST_TABLE_SIZE] = self.fast.as_slice().try_into().ok()?;
        let mut bits = BackBits::new(src)?;
        // 56 bits are ready after a reload (and at the start): five symbols of at most 11.
        let mut done = 0;
        while bits.can_reload_fast() {
            let Some(chunk) = out.get_mut(done..done + SYMBOLS_PER_RELOAD) else {
                break;
            };
            for byte in chunk.iter_mut() {
                *byte = symbol(table, &mut bits);
            }
            bits.reload_fast();
            done += SYMBOLS_PER_RELOAD;
        }
        finish_stream(table, bits, &mut out[done..])
    }

    /// Decodes the four streams of a literals section into exactly `out`: the first three streams
    /// each hold a quarter of the symbols (rounded up), the fourth the rest. `None` when a stream
    /// is not exactly its symbols (or there is no table).
    ///
    /// The four streams are decoded side by side, a symbol of each in turn, so that the processor
    /// works on four independent chains of lookups at once: that, and refilling once per five
    /// symbols, is where the speed of Huffman decoding comes from.
    pub(crate) fn decode_four_streams(&self, src: [&[u8]; 4], out: &mut [u8]) -> Option<()> {
        let table: &[u16; FAST_TABLE_SIZE] = self.fast.as_slice().try_into().ok()?;
        let segment = out.len().div_ceil(4);
        if segment * 3 > out.len() {
            return None;
        }
        let (out0, rest) = out.split_at_mut(segment);
        let (out1, rest) = rest.split_at_mut(segment);
        let (out2, out3) = rest.split_at_mut(segment);
        let mut b0 = BackBits::new(src[0])?;
        let mut b1 = BackBits::new(src[1])?;
        let mut b2 = BackBits::new(src[2])?;
        let mut b3 = BackBits::new(src[3])?;

        const N: usize = SYMBOLS_PER_RELOAD;
        let mut done = 0;
        {
            let mut c0 = out0.chunks_exact_mut(N);
            let mut c1 = out1.chunks_exact_mut(N);
            let mut c2 = out2.chunks_exact_mut(N);
            let mut c3 = out3.chunks_exact_mut(N);
            while b0.can_reload_fast()
                && b1.can_reload_fast()
                && b2.can_reload_fast()
                && b3.can_reload_fast()
            {
                let (Some(o0), Some(o1), Some(o2), Some(o3)) =
                    (c0.next(), c1.next(), c2.next(), c3.next())
                else {
                    break;
                };
                for i in 0..N {
                    o0[i] = symbol(table, &mut b0);
                    o1[i] = symbol(table, &mut b1);
                    o2[i] = symbol(table, &mut b2);
                    o3[i] = symbol(table, &mut b3);
                }
                b0.reload_fast();
                b1.reload_fast();
                b2.reload_fast();
                b3.reload_fast();
                done += N;
            }
        }
        // `out3` may be up to three symbols shorter than the others, so `done` is within each.
        finish_stream(table, b0, &mut out0[done..])?;
        finish_stream(table, b1, &mut out1[done..])?;
        finish_stream(table, b2, &mut out2[done..])?;
        finish_stream(table, b3, &mut out3[done..])
    }
}

/// The size of [`HuffmanTable::fast`].
const FAST_TABLE_SIZE: usize = 1 << MAX_MAX_NUM_BITS;
/// The symbols decoded between two reloads: 56 bits are ready, a symbol takes at most 11.
const SYMBOLS_PER_RELOAD: usize = 5;

/// The next symbol of `bits`.
#[inline(always)]
fn symbol(table: &[u16; FAST_TABLE_SIZE], bits: &mut BackBits<'_>) -> u8 {
    let entry = table[(bits.bits >> (64 - MAX_MAX_NUM_BITS)) as usize];
    let num_bits = (entry >> 8) as u32;
    bits.bits <<= num_bits;
    bits.consumed += num_bits;
    entry as u8
}

/// The last symbols of a stream, reloading before each: `None` unless the stream ends with them.
fn finish_stream(
    table: &[u16; FAST_TABLE_SIZE],
    mut bits: BackBits<'_>,
    out: &mut [u8],
) -> Option<()> {
    for byte in out.iter_mut() {
        bits.reload();
        *byte = symbol(table, &mut bits);
    }
    bits.reload();
    bits.finished().then_some(())
}
impl Default for HuffmanTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A single entry in the table contains the decoded symbol/literal and the
/// size of the prefix code.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// The byte that the prefix code replaces during encoding.
    symbol: u8,
    /// The number of bits the prefix code occupies.
    num_bits: u8,
}

/// Assert that the provided value is greater than zero, and returns the
/// 32 - the number of leading zeros
fn highest_bit_set(x: u32) -> u32 {
    assert!(x > 0);
    u32::BITS - x.leading_zeros()
}
