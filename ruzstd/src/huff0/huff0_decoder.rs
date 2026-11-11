//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::{BackBits, BitReaderReversed};
use crate::common::cpu;
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;
use core::convert::{TryFrom, TryInto};

/// The Zstandard specification limits the maximum length of a code to 11 bits.
pub(crate) const MAX_MAX_NUM_BITS: u8 = 11;

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    /// The codes by the next [`MAX_MAX_NUM_BITS`] bits of a stream, each entry the number of bits
    /// of a code (low byte) and its symbol (high byte): the table of [`HuffmanTable::decode_stream`] and
    /// [`HuffmanTable::decode_four_streams`], indexed by the next 11 bits of a stream.
    fast: Vec<u16>,
    /// The entries of `fast` with, where its code fits in the bits after the first one, a second
    /// symbol. Built when it pays, and empty until then.
    fast_pairs: Vec<Pair>,
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
            fast: Vec::new(),
            fast_pairs: Vec::new(),

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
        self.fast.extend_from_slice(&other.fast);
        self.fast_pairs.clear();
        self.weights.extend_from_slice(&other.weights);
        self.max_num_bits = other.max_num_bits;
        self.bits.extend_from_slice(&other.bits);
        self.rank_indexes.extend_from_slice(&other.rank_indexes);
        self.fse_table.reinit_from(&other.fse_table);
    }

    /// Completely empty the table of all data.
    pub fn reset(&mut self) {
        self.fast.clear();
        self.fast_pairs.clear();
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

        //starting codes for each rank
        self.rank_indexes.clear();
        self.rank_indexes.resize((max_bits + 1) as usize, 0);

        self.rank_indexes[max_bits as usize] = 0;
        for bits in (1..self.rank_indexes.len() as u8).rev() {
            self.rank_indexes[bits as usize - 1] = self.rank_indexes[bits as usize]
                + self.bit_ranks[bits as usize] as usize * (1 << (max_bits - bits));
        }

        assert!(
            self.rank_indexes[0] == 1 << max_bits,
            "rank_idx[0]: {} should be: {}",
            self.rank_indexes[0],
            1 << max_bits
        );

        // The table is indexed by the next 11 bits of a stream, whatever the longest code is: a
        // code of fewer bits ignores the bits after it, so it gets the range of all of them.
        let widen = MAX_MAX_NUM_BITS - max_bits;
        self.fast.clear();
        self.fast_pairs.clear();
        self.fast.resize(FAST_TABLE_SIZE, 0);
        for symbol in 0..self.bits.len() {
            let bits_for_symbol = self.bits[symbol];
            if bits_for_symbol != 0 {
                let base_idx = self.rank_indexes[bits_for_symbol as usize];
                let len = 1 << (max_bits - bits_for_symbol);
                self.rank_indexes[bits_for_symbol as usize] += len;
                let packed = u16::from(bits_for_symbol) | ((symbol as u16) << 8);
                self.fast[base_idx << widen..(base_idx + len) << widen].fill(packed);
            }
        }

        Ok(())
    }

    /// Decodes one stream of Huffman codes into exactly `out`, or `None` when the stream is not
    /// exactly that many symbols (or there is no table).
    pub(crate) fn decode_stream(&mut self, src: &[u8], out: &mut [u8]) -> Option<()> {
        let pairs = pairs_pay(src.len(), out.len());
        if pairs {
            self.build_pairs();
        }
        let single: &[u16; FAST_TABLE_SIZE] = self.fast.as_slice().try_into().ok()?;
        let mut bits = BackBits::new(src)?;
        let mut done = 0;
        // The loop of pairs needs eight bytes of the stream to start with.
        let lane = if pairs && src.len() >= 8 {
            Lane::of(&bits, 0)
        } else {
            None
        };
        if let Some(mut lane) = lane {
            let double: &[Pair; FAST_TABLE_SIZE] = self.fast_pairs.as_slice().try_into().ok()?;
            loop {
                let room = out.len() - done;
                let iterations = (lane.at() / LANE_STEP).min(iterations_of_pairs(room));
                if iterations == 0 {
                    break;
                }
                for _ in 0..iterations {
                    let window = out.get_mut(done..)?.first_chunk_mut()?;
                    let mut at = 0;
                    for _ in 0..LOOKUPS_PER_RELOAD {
                        at += pair(double, &mut lane.bits, window, at);
                    }
                    lane.reload(src)?;
                    done += at;
                }
            }
            bits.seek(lane.at(), lane.bits.trailing_zeros())?;
        }
        finish_stream(single, bits, out.get_mut(done..)?)
    }

    /// Decodes the four streams of a literals section into exactly `out`. `src` holds the
    /// streams one after the other: the second, the third and the fourth start at `jumps`. The
    /// first three streams each hold a quarter of the symbols (rounded up), the fourth the rest.
    /// `None` when a stream is not exactly its symbols (or there is no table).
    ///
    /// The four streams are decoded side by side, a lookup of each in turn, so that the processor
    /// works on four independent chains of lookups at once: that, refilling once per five lookups
    /// and, for literals that compress well, two symbols per lookup are where the speed of
    /// Huffman decoding comes from.
    pub(crate) fn decode_four_streams(
        &mut self,
        src: &[u8],
        jumps: [usize; 3],
        out: &mut [u8],
    ) -> Option<()> {
        let pairs = pairs_pay(src.len(), out.len());
        if pairs {
            self.build_pairs();
        }
        let single: &[u16; FAST_TABLE_SIZE] = self.fast.as_slice().try_into().ok()?;
        let segment = out.len().div_ceil(4);
        if segment * 3 > out.len() {
            return None;
        }
        let (out0, rest) = out.split_at_mut(segment);
        let (out1, rest) = rest.split_at_mut(segment);
        let (out2, out3) = rest.split_at_mut(segment);
        let mut outs = [out0, out1, out2, out3];
        let starts = [0, jumps[0], jumps[1], jumps[2]];
        let ends = [jumps[0], jumps[1], jumps[2], src.len()];
        let mut readers = [
            BackBits::new(src.get(starts[0]..ends[0])?)?,
            BackBits::new(src.get(starts[1]..ends[1])?)?,
            BackBits::new(src.get(starts[2]..ends[2])?)?,
            BackBits::new(src.get(starts[3]..ends[3])?)?,
        ];
        // The symbols decoded of each stream
        let mut done = [0; 4];

        // The loops over all four streams run while every stream has input and room for what
        // an iteration may decode. A stream needs eight bytes for them.
        let lanes = if starts.iter().zip(ends).all(|(start, end)| end - start >= 8) {
            (|| {
                Some([
                    Lane::of(&readers[0], starts[0])?,
                    Lane::of(&readers[1], starts[1])?,
                    Lane::of(&readers[2], starts[2])?,
                    Lane::of(&readers[3], starts[3])?,
                ])
            })()
        } else {
            None
        };
        if let Some(mut lanes) = lanes {
            if pairs {
                let double: &[Pair; FAST_TABLE_SIZE] =
                    self.fast_pairs.as_slice().try_into().ok()?;
                cpu::fastest(
                    #[inline(always)]
                    |_| {
                        four_streams_of_pairs(double, src, starts, &mut lanes, &mut outs, &mut done)
                    },
                )?;
            } else {
                let symbols = cpu::fastest(
                    #[inline(always)]
                    |_| four_streams_of_symbols(single, src, starts, &mut lanes, &mut outs),
                )?;
                done = [symbols; 4];
            }
            for ((lane, reader), start) in lanes.iter().zip(&mut readers).zip(starts) {
                reader.seek(lane.at().checked_sub(start)?, lane.bits.trailing_zeros())?;
            }
        }
        for ((reader, out), done) in IntoIterator::into_iter(readers).zip(outs).zip(done) {
            finish_stream(single, reader, out.get_mut(done..)?)?;
        }
        Some(())
    }

    /// Builds [`HuffmanTable::fast_pairs`] from [`HuffmanTable::fast`], unless it is built.
    ///
    /// The 11 bits of an index are a first code and, after it, the bits that follow it in the
    /// stream: where a second code fits in them, the entry holds both symbols.
    fn build_pairs(&mut self) {
        if self.fast_pairs.len() == self.fast.len() {
            return;
        }
        let Ok(single) = <&[u16; FAST_TABLE_SIZE]>::try_from(self.fast.as_slice()) else {
            return;
        };
        self.fast_pairs.clear();
        self.fast_pairs.extend((0..FAST_TABLE_SIZE).map(|index| {
            let first = single[index];
            let first_bits = u32::from(first & 0xFF);
            let second = single[(index << first_bits) % FAST_TABLE_SIZE];
            let second_bits = u32::from(second & 0xFF);
            if first_bits + second_bits <= u32::from(MAX_MAX_NUM_BITS) {
                Pair {
                    symbols: [(first >> 8) as u8, (second >> 8) as u8],
                    bits: (first_bits + second_bits) as u8,
                    count: PairCount::Two,
                }
            } else {
                Pair {
                    symbols: [(first >> 8) as u8, 0],
                    bits: first_bits as u8,
                    count: PairCount::One,
                }
            }
        }));
    }
}

/// The size of [`HuffmanTable::fast`] and [`HuffmanTable::fast_pairs`].
const FAST_TABLE_SIZE: usize = 1 << MAX_MAX_NUM_BITS;
/// The lookups between two reloads: 56 bits are ready, a lookup takes at most 11.
const LOOKUPS_PER_RELOAD: usize = 5;
/// An entry of the table of pairs: what the next 11 bits of a stream decode to.
#[derive(Clone, Copy)]
struct Pair {
    /// The symbol of the first code and, if its code fits in the bits after the first one, the
    /// symbol of the second.
    symbols: [u8; 2],
    /// The bits of the codes
    bits: u8,
    count: PairCount,
}

/// The number of symbols of a [`Pair`]. It is an enum, not a `u8`, for what it tells the
/// compiler: the sum of a few of them is seen to be small, so writing after that many symbols
/// of a window needs no check.
#[derive(Clone, Copy)]
#[repr(u8)]
enum PairCount {
    One = 1,
    Two = 2,
}

/// The bytes the lookups between two reloads may write to: they decode at most two symbols each,
/// ten in all. A lookup writes two bytes after the symbols before it, of which there are at most
/// eight; the window is longer than those ten bytes for a compiler that only sees that four
/// numbers of two bits add up to less than sixteen.
const WINDOW: usize = 18;

/// Whether decoding `regenerated` symbols from `compressed` bytes two symbols per lookup is worth
/// building the table of pairs: where the symbols take few bits (many pairs fit in 11 bits) and
/// there are enough of them. The reference decoder draws the line at the same place for blocks
/// of this size: symbols of less than seven bits on average.
fn pairs_pay(compressed: usize, regenerated: usize) -> bool {
    regenerated >= 4096 && compressed * 16 < regenerated * 14
}

/// One of the streams in the loops that decode the four of them side by side.
///
/// `bits` holds the unread bits of the eight bytes at `at` (a position in the bytes of all the
/// streams), most significant first, and after them a one: the zeros below it count the bits
/// read. So a symbol takes one shift and no counter, as in the reference decoder's loop
/// (`HUF_decompress4X1_usingDTable_internal_fast_c_loop`). The one takes the place of the last
/// bit of the eight bytes, which is not read before the next reload: at most 63 bits are.
#[derive(Clone, Copy)]
struct Lane {
    bits: u64,
    /// Of 32 bits: eight more do not overflow a `usize`, so that one comparison tells that the
    /// eight bytes there are in the source.
    at: u32,
}

/// The bytes a lane moves back by in an iteration, at most: it reads no more than 63 bits.
const LANE_STEP: usize = 7;

impl Lane {
    /// The lane of a reader at the start of its stream, which starts at `start` and has eight
    /// bytes or more: only the padding is read, eight bits at most. `None` for a position that
    /// is not one of 32 bits, which the streams of a block do not have.
    fn of(reader: &BackBits<'_>, start: usize) -> Option<Self> {
        let (idx, consumed) = reader.place();
        Some(Lane {
            bits: reader.bits | (1 << (consumed & 63)),
            at: u32::try_from(start.checked_add(idx)?).ok()?,
        })
    }

    /// The position in the source of the eight bytes the bits are of.
    #[inline(always)]
    fn at(&self) -> usize {
        self.at as usize
    }

    /// Refills the bits. `None` when the eight bytes are not in `src`, which does not happen
    /// while the lane reads no more than the bytes of its stream.
    #[inline(always)]
    fn reload(&mut self, src: &[u8]) -> Option<()> {
        let read = self.bits.trailing_zeros();
        self.at = self.at.wrapping_sub(read >> 3);
        let word = src.get(self.at()..self.at().wrapping_add(8))?;
        self.bits = (u64::from_le_bytes(word.try_into().ok()?) | 1) << (read & 7);
        Some(())
    }
}

/// The iterations of a loop over the four streams that need no check of the input: each moves a
/// lane back by [`LANE_STEP`] bytes at most, and a lane has to stay within its stream. `None`
/// when a lane is before its stream.
#[inline(always)]
fn iterations_of_input(lanes: &[Lane; 4], starts: [usize; 4]) -> Option<usize> {
    let mut iterations = usize::MAX;
    for (lane, start) in lanes.iter().zip(starts) {
        iterations = iterations.min(lane.at().checked_sub(start)? / LANE_STEP);
    }
    Some(iterations)
}

/// The iterations of a loop of pairs that fit in `room` symbols of output: an iteration decodes
/// ten symbols at most, and each needs its window.
#[inline(always)]
fn iterations_of_pairs(room: usize) -> usize {
    room.saturating_sub(WINDOW - 2 * LOOKUPS_PER_RELOAD) / (2 * LOOKUPS_PER_RELOAD)
}

/// Decodes the four streams a symbol per lookup while each has input and room for the five
/// symbols of an iteration: the symbols decoded of each stream.
///
/// How many iterations are safe is computed up front, so that the loop checks nothing but its
/// count, as the reference decoder does. The streams give out at different rates, so the count
/// is taken again until it is zero.
#[inline(always)]
fn four_streams_of_symbols(
    table: &[u16; FAST_TABLE_SIZE],
    src: &[u8],
    starts: [usize; 4],
    lanes: &mut [Lane; 4],
    outs: &mut [&mut [u8]; 4],
) -> Option<usize> {
    const N: usize = LOOKUPS_PER_RELOAD;
    let [out0, out1, out2, out3] = outs;
    let [mut l0, mut l1, mut l2, mut l3] = *lanes;
    let mut done = 0;
    loop {
        // The fourth stream has the fewest symbols.
        let room = out3.len().checked_sub(done)? / N;
        let iterations = iterations_of_input(&[l0, l1, l2, l3], starts)?.min(room);
        if iterations == 0 {
            break;
        }
        let chunks = out0
            .get_mut(done..)?
            .chunks_exact_mut(N)
            .zip(out1.get_mut(done..)?.chunks_exact_mut(N))
            .zip(out2.get_mut(done..)?.chunks_exact_mut(N))
            .zip(out3.get_mut(done..)?.chunks_exact_mut(N))
            .take(iterations);
        for (((o0, o1), o2), o3) in chunks {
            for i in 0..N {
                o0[i] = symbol(table, &mut l0.bits);
                o1[i] = symbol(table, &mut l1.bits);
                o2[i] = symbol(table, &mut l2.bits);
                o3[i] = symbol(table, &mut l3.bits);
            }
            l0.reload(src)?;
            l1.reload(src)?;
            l2.reload(src)?;
            l3.reload(src)?;
        }
        done += iterations * N;
    }
    *lanes = [l0, l1, l2, l3];
    Some(done)
}

/// Decodes the four streams one or two symbols per lookup while each has input and room for
/// what an iteration may decode, and adds the symbols decoded of each stream to `done`.
#[inline(always)]
fn four_streams_of_pairs(
    table: &[Pair; FAST_TABLE_SIZE],
    src: &[u8],
    starts: [usize; 4],
    lanes: &mut [Lane; 4],
    outs: &mut [&mut [u8]; 4],
    done: &mut [usize; 4],
) -> Option<()> {
    let [out0, out1, out2, out3] = outs;
    let [mut l0, mut l1, mut l2, mut l3] = *lanes;
    // What is left to decode of each stream. The slices get shorter, instead of positions in
    // them moving on: a window is then there if the slice is long enough, one comparison.
    let mut r0 = out0.get_mut(done[0]..)?;
    let mut r1 = out1.get_mut(done[1]..)?;
    let mut r2 = out2.get_mut(done[2]..)?;
    let mut r3 = out3.get_mut(done[3]..)?;
    loop {
        let mut iterations = iterations_of_input(&[l0, l1, l2, l3], starts)?;
        for room in [r0.len(), r1.len(), r2.len(), r3.len()] {
            iterations = iterations.min(iterations_of_pairs(room));
        }
        if iterations == 0 {
            break;
        }
        for _ in 0..iterations {
            let w0 = r0.first_chunk_mut()?;
            let w1 = r1.first_chunk_mut()?;
            let w2 = r2.first_chunk_mut()?;
            let w3 = r3.first_chunk_mut()?;
            let (mut a0, mut a1, mut a2, mut a3) = (0, 0, 0, 0);
            for _ in 0..LOOKUPS_PER_RELOAD {
                a0 += pair(table, &mut l0.bits, w0, a0);
                a1 += pair(table, &mut l1.bits, w1, a1);
                a2 += pair(table, &mut l2.bits, w2, a2);
                a3 += pair(table, &mut l3.bits, w3, a3);
            }
            l0.reload(src)?;
            l1.reload(src)?;
            l2.reload(src)?;
            l3.reload(src)?;
            r0 = core::mem::take(&mut r0).get_mut(a0..)?;
            r1 = core::mem::take(&mut r1).get_mut(a1..)?;
            r2 = core::mem::take(&mut r2).get_mut(a2..)?;
            r3 = core::mem::take(&mut r3).get_mut(a3..)?;
        }
    }
    *lanes = [l0, l1, l2, l3];
    let left = [r0.len(), r1.len(), r2.len(), r3.len()];
    for ((done, out), left) in done.iter_mut().zip([out0, out1, out2, out3]).zip(left) {
        *done = out.len() - left;
    }
    Some(())
}

/// The next symbol of `bits` (the unread bits of a stream, most significant first), which it
/// shifts out.
#[inline(always)]
fn symbol(table: &[u16; FAST_TABLE_SIZE], bits: &mut u64) -> u8 {
    let entry = table[(*bits >> (64 - MAX_MAX_NUM_BITS)) as usize];
    // The number of bits is the low byte: a shift takes its count from there.
    *bits <<= entry & 63;
    (entry >> 8) as u8
}

/// The next symbol of a reader that counts the bits it reads.
#[inline(always)]
fn counted_symbol(table: &[u16; FAST_TABLE_SIZE], bits: &mut BackBits<'_>) -> u8 {
    let entry = table[(bits.bits >> (64 - MAX_MAX_NUM_BITS)) as usize];
    let num_bits = u32::from(entry & 0xFF);
    bits.bits <<= num_bits;
    bits.consumed += num_bits;
    (entry >> 8) as u8
}

/// Writes the next symbol or two of `bits` to `window[at..]` and shifts them out: the number of
/// symbols. Two bytes are written either way; `at` is the sum of the numbers of symbols of fewer
/// than [`LOOKUPS_PER_RELOAD`] lookups.
#[inline(always)]
fn pair(
    table: &[Pair; FAST_TABLE_SIZE],
    bits: &mut u64,
    window: &mut [u8; WINDOW],
    at: usize,
) -> usize {
    // What an entry holds is read where it is used, not all at once: the entry is in the cache.
    let entry = &table[(*bits >> (64 - MAX_MAX_NUM_BITS)) as usize];
    window[at..at + 2].copy_from_slice(&entry.symbols);
    *bits <<= entry.bits & 63;
    entry.count as usize
}

/// The rest of a stream, into exactly `out`: `None` unless the stream ends with these symbols.
fn finish_stream(
    table: &[u16; FAST_TABLE_SIZE],
    mut bits: BackBits<'_>,
    out: &mut [u8],
) -> Option<()> {
    // 56 bits are ready after a reload (and at the start): five symbols of at most 11.
    let mut done = 0;
    while bits.can_reload_fast() {
        let Some(chunk) = out.get_mut(done..done + LOOKUPS_PER_RELOAD) else {
            break;
        };
        for byte in chunk.iter_mut() {
            *byte = counted_symbol(table, &mut bits);
        }
        bits.reload_fast();
        done += LOOKUPS_PER_RELOAD;
    }
    // Then a reload before each symbol, as near the start of the stream as it may be.
    for byte in &mut out[done..] {
        bits.reload();
        *byte = counted_symbol(table, &mut bits);
    }
    bits.reload();
    bits.finished().then_some(())
}

impl Default for HuffmanTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Assert that the provided value is greater than zero, and returns the
/// 32 - the number of leading zeros
fn highest_bit_set(x: u32) -> u32 {
    assert!(x > 0);
    u32::BITS - x.leading_zeros()
}
