//! Matchers that keep the whole window in one contiguous buffer
//!
//! This is the matching used for every compression level. The levels differ in the strategy, which follows the
//! strategies of the reference implementation of the same name:
//!
//! * [`Strategy::Fast`] ([`CompressionLevel::Fastest`]): one hash table, greedy matching.
//! * [`Strategy::DoubleFast`] ([`CompressionLevel::Default`]): a table of 8-byte prefixes and a table of
//!   `min_match`-byte prefixes, greedy matching. The long table finds the long matches, the short one the rest.
//! * [`Strategy::Lazy`] ([`CompressionLevel::Better`] and [`CompressionLevel::Best`]): hash chains, and before a
//!   match is emitted the matcher checks whether a match starting one (or two) bytes later is better. A match is
//!   worth roughly four points per matched byte, minus the number of bits needed to encode its offset.
//!
//! All strategies first try the most recent offsets, which the encoder can write as repeat codes.
//!
//! All data of the current window is kept in one contiguous buffer, so matches can span block boundaries and reach
//! back several blocks (up to the configured window size).
//!
//! Table entries are *absolute* positions: the index in the buffer plus `base`. Every frame starts at a base above
//! all positions of the previous frame, so entries left over from earlier frames are below the lowest valid
//! position and are ignored without clearing the tables. When the oldest data is dropped from the buffer the base
//! grows by the same amount, so the entries stay valid. The table contents are only ever used as *hints*: every
//! candidate is verified by comparing the actual bytes, so stale entries can never produce an invalid sequence.
//!
//! If the size of a frame is known up front (see [`super::Matcher::source_size_hint`]) the window and the tables
//! are shrunk to fit it, as the reference implementation does. This keeps small frames cheap.
//!
//! The content of a dictionary is placed in the buffer in front of the first block of a frame. Matches may reference
//! it as long as the total amount of data in the frame does not exceed the window size, which is the limit the
//! Zstd specification places on dictionary references.

use alloc::vec::Vec;
use core::convert::TryInto;

use super::blocks::encode_offset_value;
use super::frame_compressor::INITIAL_OFFSET_HISTORY;
use super::{CompressionLevel, Sequence};
use crate::common::MAX_BLOCK_SIZE;

/// The number of bytes hashed by the hash chains of [`Strategy::Lazy`], also the minimum match length there
const LAZY_MIN_MATCH: usize = 5;

/// Bytes read at once for hashing and comparing. The greedy strategies stop searching this far before the end of
/// a block.
const READ_SIZE: usize = 8;

/// In regions where no matches are found the matcher starts to skip positions. The step size grows
/// by one for every `1 << SEARCH_STRENGTH` bytes that passed since the last match.
const SEARCH_STRENGTH: usize = 8;

/// How many full blocks may accumulate on top of the window before the oldest data is dropped from the buffer.
const SLACK_BLOCKS: usize = 4;

/// How many positions ahead the row of a position is fetched into the cache ([`Strategy::Lazy`] with rows)
const ROW_LOOKAHEAD: usize = 8;

/// The smallest hash table that is split into rows: 16 rows of 16 entries
const MIN_ROW_HASH_LOG: u32 = 8;

/// Above this base the tables are cleared and positions start from the bottom again, so they never overflow
const MAX_BASE: u32 = u32::MAX / 2;

/// The matching strategy, see the module documentation
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    Fast,
    DoubleFast,
    Lazy,
}

/// Parameters of the [`WindowMatcher`]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct MatcherParams {
    pub(crate) strategy: Strategy,
    /// log2 of the maximum back-reference distance
    pub(crate) window_log: u32,
    /// log2 of the number of entries in the hash table (the table of 8-byte prefixes for [`Strategy::DoubleFast`])
    pub(crate) hash_log: u32,
    /// log2 of the number of entries in the chain table ([`Strategy::Lazy`]) or in the table of short prefixes
    /// ([`Strategy::DoubleFast`]). Not used by [`Strategy::Fast`].
    pub(crate) chain_log: u32,
    /// Number of bytes hashed by the (short) hash table of the greedy strategies, between 4 and 8
    pub(crate) min_match: u32,
    /// How many candidates are inspected at most per search ([`Strategy::Lazy`])
    pub(crate) search_depth: u32,
    /// How many positions after a found match are inspected for a better match (0, 1 or 2, [`Strategy::Lazy`])
    pub(crate) lazy_steps: u32,
}

impl MatcherParams {
    /// Parameters used for the given compression level, `None` if the level does not match at all
    pub(crate) fn for_level(level: CompressionLevel) -> Option<Self> {
        match level {
            CompressionLevel::Uncompressed => None,
            CompressionLevel::Fastest => Some(Self {
                strategy: Strategy::Fast,
                window_log: 19,
                hash_log: 14,
                chain_log: 0,
                min_match: 6,
                search_depth: 1,
                lazy_steps: 0,
            }),
            CompressionLevel::Default => Some(Self {
                strategy: Strategy::DoubleFast,
                window_log: 21,
                hash_log: 17,
                chain_log: 16,
                min_match: 5,
                search_depth: 1,
                lazy_steps: 0,
            }),
            CompressionLevel::Better => Some(Self {
                strategy: Strategy::Lazy,
                window_log: 22,
                hash_log: 19,
                chain_log: 18,
                min_match: LAZY_MIN_MATCH as u32,
                search_depth: 16,
                lazy_steps: 2,
            }),
            CompressionLevel::Best => Some(Self {
                strategy: Strategy::Lazy,
                window_log: 23,
                hash_log: 20,
                chain_log: 19,
                min_match: LAZY_MIN_MATCH as u32,
                search_depth: 128,
                lazy_steps: 2,
            }),
        }
    }

    /// The parameters for a frame that references at most `size` bytes (its data and the dictionary content). The
    /// window is not made larger than needed, and no table larger than about twice the window.
    pub(crate) fn for_size(mut self, size: u64) -> Self {
        let size_log = size.max(1).next_power_of_two().ilog2();
        let window_log = size_log.clamp(10, self.window_log);
        if window_log < self.window_log {
            self.window_log = window_log;
            self.hash_log = self.hash_log.min(window_log + 1);
            self.chain_log = self.chain_log.min(window_log);
        }
        // Like the reference implementation, small inputs use shorter minimum matches with the greedy strategies
        match self.strategy {
            Strategy::Fast if size <= 16 * 1024 => self.min_match = 5,
            Strategy::DoubleFast if size <= 16 * 1024 => self.min_match = 4,
            _ => {}
        }
        self
    }

    fn window_size(&self) -> usize {
        1 << self.window_log
    }

    /// Whether a dictionary snapshot built with `other` can be used with these parameters
    fn same_tables(&self, other: &Self) -> bool {
        self.strategy == other.strategy
            && self.hash_log == other.hash_log
            && self.chain_log == other.chain_log
            && self.min_match == other.min_match
    }
}

/// Hash and chain tables. Separate from the data buffer so the two can be borrowed independently.
struct Tables {
    /// The hash table. With rows (see `row_log`) the positions of the rows, `1 << row_log` to a row.
    hash: Vec<u32>,
    chain: Vec<u32>,
    /// log2 of the entries of a row of [`Strategy::Lazy`] without a dictionary, 0 with hash chains
    row_log: u32,
    /// A byte of the hash of each position in `hash`, to compare before the position is looked at. The first
    /// byte of a row is not a tag but the entry written last (the first entry of a row is not used): entries
    /// are written downwards, so the entries from there upwards (wrapping around) go from the most recent
    /// position to the oldest.
    tags: Vec<u8>,
    /// Record the slots written, see `restore`. Only set while a dictionary is in use.
    tracking: bool,
    /// The tables are the dictionary snapshot plus the slots in `touched_*`
    dict_baseline: bool,
    touched_hash: Vec<u32>,
    touched_chain: Vec<u32>,
    touched_limit: usize,
}

impl Tables {
    fn new() -> Self {
        Self {
            hash: Vec::new(),
            chain: Vec::new(),
            row_log: 0,
            tags: Vec::new(),
            tracking: false,
            dict_baseline: false,
            touched_hash: Vec::new(),
            touched_chain: Vec::new(),
            touched_limit: 0,
        }
    }

    /// Give the tables the sizes of the parameters. Entries are kept if the size does not change: they are stale
    /// anyway once a new frame starts at a higher base.
    fn resize(&mut self, params: &MatcherParams, row_log: u32) {
        let hash_len = 1usize << params.hash_log;
        let chain_len = match params.strategy {
            Strategy::Fast => 0,
            Strategy::Lazy if row_log != 0 => 0,
            Strategy::DoubleFast | Strategy::Lazy => 1usize << params.chain_log,
        };
        let tags_len = if row_log != 0 { hash_len } else { 0 };
        if self.row_log != row_log || self.tags.len() != tags_len {
            // Positions left by the other layout are below the base of the next frame, like any stale entry
            self.row_log = row_log;
            self.tags = alloc::vec![0; tags_len];
        }
        if self.hash.len() != hash_len {
            self.hash = alloc::vec![0; hash_len];
            self.dict_baseline = false;
        }
        if self.chain.len() != chain_len {
            self.chain = alloc::vec![0; chain_len];
            self.dict_baseline = false;
        }
        self.touched_limit = (hash_len + chain_len) / 8;
    }

    fn clear(&mut self) {
        self.hash.fill(0);
        self.chain.fill(0);
        self.tags.fill(0);
        self.dict_baseline = false;
    }

    #[inline(always)]
    fn set_hash(&mut self, slot: usize, value: u32) {
        self.set_hash_t::<true>(slot, value)
    }

    #[inline(always)]
    fn set_chain(&mut self, slot: usize, value: u32) {
        self.set_chain_t::<true>(slot, value)
    }

    /// Set a slot of the hash table. With `TRACK == false` the slot is not recorded, which is only correct while
    /// `tracking` is off.
    #[inline(always)]
    fn set_hash_t<const TRACK: bool>(&mut self, slot: usize, value: u32) {
        self.hash[slot] = value;
        if TRACK && self.tracking {
            self.touch(slot, true);
        }
    }

    /// See `set_hash_t`
    #[inline(always)]
    fn set_chain_t<const TRACK: bool>(&mut self, slot: usize, value: u32) {
        self.chain[slot] = value;
        if TRACK && self.tracking {
            self.touch(slot, false);
        }
    }

    #[inline(never)]
    fn touch(&mut self, slot: usize, hash: bool) {
        if self.touched_hash.len() + self.touched_chain.len() >= self.touched_limit {
            // Restoring the slots one by one would be more expensive than copying the whole tables
            self.tracking = false;
            self.dict_baseline = false;
            self.touched_hash.clear();
            self.touched_chain.clear();
            return;
        }
        if hash {
            self.touched_hash.push(slot as u32);
        } else {
            self.touched_chain.push(slot as u32);
        }
    }

    /// Make the tables equal to the dictionary snapshot, undoing the last frame's insertions if possible
    fn restore(&mut self, snapshot: &DictSnapshot) {
        if self.dict_baseline {
            for &slot in self.touched_hash.iter() {
                self.hash[slot as usize] = snapshot.hash[slot as usize];
            }
            for &slot in self.touched_chain.iter() {
                self.chain[slot as usize] = snapshot.chain[slot as usize];
            }
        } else {
            self.hash.copy_from_slice(&snapshot.hash);
            self.chain.copy_from_slice(&snapshot.chain);
        }
        self.touched_hash.clear();
        self.touched_chain.clear();
        self.tracking = true;
        self.dict_baseline = true;
    }

    /// Stop recording, the next frame does not use the dictionary
    fn stop_tracking(&mut self) {
        self.tracking = false;
        self.dict_baseline = false;
        self.touched_hash.clear();
        self.touched_chain.clear();
    }
}

/// The table state right after a dictionary has been indexed (with base 1). Restoring it is a lot cheaper than
/// re-indexing the dictionary for every frame, which matters for many small inputs.
struct DictSnapshot {
    content: Vec<u8>,
    params: MatcherParams,
    hash: Vec<u32>,
    chain: Vec<u32>,
}

/// A [`super::Matcher`] implementation over a contiguous window, see the module documentation
pub(crate) struct WindowMatcher {
    /// The parameters of the compression level
    level_params: MatcherParams,
    /// The parameters of the current frame
    params: MatcherParams,
    /// The size of the current frame's data, if known
    size_hint: Option<u64>,
    /// The frame's parameters still have to be determined and the tables sized
    pending_setup: bool,
    /// Contiguous window data, the current block is at the end
    buf: Vec<u8>,
    /// The buffer is never allowed to grow larger than this
    capacity: usize,
    /// Index in `buf` where the current block starts
    block_start: usize,
    /// Index in `buf` where the data of the current frame starts. Everything before it is dictionary content.
    frame_base: usize,
    /// Table entries are `buf` indices plus this value
    base: u32,
    tables: Tables,
    /// All absolute positions below this one have been inserted into the chains ([`Strategy::Lazy`])
    next_to_update: u32,
    dict_snapshot: Option<DictSnapshot>,
    /// The offset history the encoder will have after the sequences emitted so far. Matches at these offsets are
    /// encoded as repeat codes, which cost almost no bits, so they are tried first.
    rep: [u32; 3],
    /// Buffers handed out by `get_next_space`
    vec_pool: Vec<Vec<u8>>,
}

impl WindowMatcher {
    pub(crate) fn new(params: MatcherParams) -> Self {
        Self {
            level_params: params,
            params,
            size_hint: None,
            pending_setup: true,
            buf: Vec::new(),
            capacity: 0,
            block_start: 0,
            frame_base: 0,
            base: 1,
            tables: Tables::new(),
            next_to_update: 1,
            dict_snapshot: None,
            rep: INITIAL_OFFSET_HISTORY,
            vec_pool: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn params(&self) -> MatcherParams {
        self.params
    }

    /// Reset for a new frame with the given level parameters
    pub(crate) fn reset(&mut self, params: MatcherParams) {
        // Everything the previous frame inserted lies below the new base
        let next_base = self.base as u64 + self.buf.len() as u64 + 1;
        self.base = next_base as u32;
        self.buf.clear();
        self.block_start = 0;
        self.frame_base = 0;
        self.rep = INITIAL_OFFSET_HISTORY;
        self.level_params = params;
        self.params = params;
        self.size_hint = None;
        self.pending_setup = true;
        if next_base > MAX_BASE as u64 {
            self.tables.clear();
            self.base = 1;
        }
        self.next_to_update = self.base;
    }

    /// The size of the next frame's data
    pub(crate) fn source_size_hint(&mut self, size: u64) {
        debug_assert!(
            self.pending_setup,
            "the size must be given before data is added"
        );
        self.size_hint = Some(size);
    }

    /// Determine the parameters of the frame, now that the data size and the dictionary are known
    fn setup(&mut self, dict_len: usize) {
        if !self.pending_setup {
            return;
        }
        self.pending_setup = false;
        self.params = match self.size_hint {
            Some(size) => self.level_params.for_size(size + dict_len as u64),
            None => self.level_params,
        };
        // Rows of tagged positions are searched much faster than hash chains. Frames with a dictionary keep the
        // chains, whose table state after the dictionary is cheap to restore for every frame.
        let row_log = match self.params.strategy {
            Strategy::Lazy if dict_len == 0 && self.params.hash_log >= MIN_ROW_HASH_LOG => {
                self.params.search_depth.max(1).ilog2().clamp(4, 6)
            }
            _ => 0,
        };
        self.tables.resize(&self.params, row_log);
        self.capacity =
            self.params.window_size() + dict_len + SLACK_BLOCKS * MAX_BLOCK_SIZE as usize;
        if self.size_hint.is_none() {
            // Growing the buffer block by block would copy the data again at every doubling. Reserving the whole
            // buffer only costs address space, the pages are used as the data arrives.
            self.buf.reserve(self.capacity);
        }
    }

    /// Place dictionary content in front of the data of the next frame. Must be called right after `reset` (and
    /// `source_size_hint`).
    pub(crate) fn load_dictionary(&mut self, content: &[u8]) {
        assert!(
            self.buf.is_empty(),
            "a dictionary can only be loaded right after a reset"
        );
        if content.is_empty() {
            return;
        }
        self.setup(content.len());
        // The dictionary stays referencable until the frame holds `window_size` bytes, make sure the buffer
        // does not have to drop data before that
        self.capacity = usize::max(
            self.capacity,
            content.len() + self.params.window_size() + MAX_BLOCK_SIZE as usize,
        );
        self.buf.extend_from_slice(content);
        self.block_start = self.buf.len();
        self.frame_base = self.buf.len();
        // Dictionary frames index from the bottom, like the snapshot
        self.base = 1;

        let params = self.params;
        let reuse = self
            .dict_snapshot
            .as_ref()
            .is_some_and(|s| s.content == content && s.params.same_tables(&params));
        if reuse {
            self.tables.restore(self.dict_snapshot.as_ref().unwrap());
        } else {
            self.tables.stop_tracking();
            self.tables.clear();
            self.index_dictionary();
            self.dict_snapshot = Some(DictSnapshot {
                content: content.to_vec(),
                params,
                hash: self.tables.hash.clone(),
                chain: self.tables.chain.clone(),
            });
            self.tables.touched_hash.clear();
            self.tables.touched_chain.clear();
            self.tables.tracking = true;
            self.tables.dict_baseline = true;
        }
        self.next_to_update = match params.strategy {
            Strategy::Lazy => self.base + content.len().saturating_sub(LAZY_MIN_MATCH - 1) as u32,
            _ => self.base + self.buf.len() as u32,
        };
    }

    /// Insert every position of the dictionary content (which is all of `buf`) into the tables
    fn index_dictionary(&mut self) {
        let buf = self.buf.as_slice();
        let base = self.base;
        let params = self.params;
        let tables = &mut self.tables;
        match params.strategy {
            Strategy::Fast | Strategy::DoubleFast => {
                let short = ShortHash::new(params.min_match, params.hash_log);
                let short_in_chain = ShortHash::new(params.min_match, params.chain_log);
                for pos in 0..buf.len().saturating_sub(READ_SIZE - 1) {
                    let v = read_u64(buf, pos);
                    let abs = pos as u32 + base;
                    if params.strategy == Strategy::Fast {
                        tables.hash[short.hash(v)] = abs;
                    } else {
                        tables.hash[hash8(v, params.hash_log)] = abs;
                        tables.chain[short_in_chain.hash(v)] = abs;
                    }
                }
            }
            Strategy::Lazy => {
                let indexable = buf.len().saturating_sub(LAZY_MIN_MATCH - 1);
                let chain_mask = tables.chain.len() - 1;
                for pos in 0..indexable {
                    let h = hash_lazy(buf, pos, params.hash_log);
                    let abs = pos as u32 + base;
                    tables.chain[abs as usize & chain_mask] = tables.hash[h];
                    tables.hash[h] = abs;
                }
            }
        }
    }

    pub(crate) fn window_size(&self) -> u64 {
        let params = match (self.pending_setup, self.size_hint) {
            (true, Some(size)) => self.level_params.for_size(size),
            _ => self.params,
        };
        params.window_size() as u64
    }

    pub(crate) fn get_next_space(&mut self) -> Vec<u8> {
        // An empty space is filled up to the maximum block size by the frame compressor
        self.vec_pool.pop().unwrap_or_default()
    }

    pub(crate) fn get_last_space(&self) -> &[u8] {
        &self.buf[self.block_start..]
    }

    pub(crate) fn commit_space(&mut self, mut space: Vec<u8>) {
        assert!(space.len() <= MAX_BLOCK_SIZE as usize);
        self.setup(0);
        if self.dict_snapshot.is_none() || self.frame_base == 0 {
            // No dictionary in this frame
            if self.tables.tracking {
                self.tables.stop_tracking();
            }
        }
        if self.buf.len() + space.len() > self.capacity {
            // Drop everything that is older than one window before the start of the new block. The absolute
            // positions of the remaining data stay the same.
            let keep = usize::min(self.buf.len(), self.params.window_size());
            let shift = self.buf.len() - keep;
            self.buf.copy_within(shift.., 0);
            self.buf.truncate(keep);
            self.base += shift as u32;
            self.frame_base = self.frame_base.saturating_sub(shift);
            if self.base > MAX_BASE {
                // Start the positions from the bottom again
                self.tables.clear();
                self.tables.stop_tracking();
                self.base = 1;
                self.next_to_update = 1 + self.buf.len() as u32;
            }
        }
        self.block_start = self.buf.len();
        if self.buf.is_empty() && self.buf.capacity() < space.len() {
            // Take over the space instead of copying it
            core::mem::swap(&mut self.buf, &mut space);
        } else {
            self.buf.extend_from_slice(&space);
        }
        space.clear();
        self.vec_pool.push(space);
    }

    /// Nothing to do here. The next search starts at the next block; the positions of this block
    /// are inserted lazily by the next search ([`Strategy::Lazy`]) or not at all.
    pub(crate) fn skip_matching(&mut self) {}

    /// Dictionary content may be referenced while the frame does not exceed the window size. While that is the
    /// case every offset reaching back into the dictionary is valid, no matter how far back it points.
    fn dict_usable(&self, block_end: usize) -> bool {
        self.frame_base > 0 && block_end - self.frame_base <= self.params.window_size()
    }

    /// Index in `buf` of the oldest data matches may reference, ignoring the window
    fn src_floor(&self, block_end: usize) -> usize {
        if self.dict_usable(block_end) {
            0
        } else {
            self.frame_base
        }
    }

    pub(crate) fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        match self.params.strategy {
            // Recording the written slots is only needed in frames with a dictionary
            Strategy::Fast if self.tables.tracking => {
                self.match_greedy::<false, true>(handle_sequence)
            }
            Strategy::Fast => self.match_greedy::<false, false>(handle_sequence),
            Strategy::DoubleFast if self.tables.tracking => {
                self.match_greedy::<true, true>(handle_sequence)
            }
            Strategy::DoubleFast => self.match_greedy::<true, false>(handle_sequence),
            Strategy::Lazy => self.match_lazy(handle_sequence),
        }
    }

    /// The greedy strategies: [`Strategy::Fast`] (`DOUBLE == false`) and [`Strategy::DoubleFast`]
    fn match_greedy<const DOUBLE: bool, const TRACK: bool>(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        let block_start = self.block_start;
        let block_end = self.buf.len();
        if block_end - block_start <= READ_SIZE {
            if block_end > block_start {
                handle_sequence(Sequence::Literals {
                    literals: &self.buf[block_start..],
                });
            }
            return;
        }
        // The lowest index a match may start at. Computed for the end of the block, so it holds for every position
        // in it.
        let low = if self.dict_usable(block_end) {
            0
        } else {
            usize::max(
                self.frame_base,
                block_end.saturating_sub(self.params.window_size()),
            )
        };
        let base = self.base;
        let low_abs = low as u32 + base;
        let params = self.params;
        let buf = self.buf.as_slice();
        let tables = &mut self.tables;
        let mut rep = self.rep;
        // Last position at which a search can be started
        let ilimit = block_end - READ_SIZE;
        // The short hash lives in `hash` for Fast and in `chain` for DoubleFast
        let short = ShortHash::new(
            params.min_match,
            if DOUBLE {
                params.chain_log
            } else {
                params.hash_log
            },
        );
        let long_log = params.hash_log;

        let mut ip = block_start;
        let mut anchor = block_start;
        // The first byte of the frame has nothing to match against
        if ip == low {
            ip += 1;
        }

        'search: while ip < ilimit {
            let v = read_u64(buf, ip);
            let cur = ip as u32 + base;
            let h_s = short.hash(v);
            let (cand_long, cand_short) = if DOUBLE {
                let h_l = hash8(v, long_log);
                let cand_long = tables.hash[h_l];
                let cand_short = tables.chain[h_s];
                tables.set_hash_t::<TRACK>(h_l, cur);
                tables.set_chain_t::<TRACK>(h_s, cur);
                (cand_long, cand_short)
            } else {
                let cand = tables.hash[h_s];
                tables.set_hash_t::<TRACK>(h_s, cur);
                (0, cand)
            };

            // Search: the most recent offset at the next position, then a long match, then a short match
            let (start, offset, match_len);
            let r1 = rep[0] as usize;
            if r1 <= ip + 1 - low && read_u32(buf, ip + 1 - r1) == read_u32(buf, ip + 1) {
                start = ip + 1;
                offset = r1;
                match_len = 4 + common_prefix_len(buf, start - r1 + 4, start + 4, block_end);
            } else {
                let found = if DOUBLE && cand_long >= low_abs {
                    let m = (cand_long - base) as usize;
                    if read_u64(buf, m) == v {
                        Some((ip, m, 8 + common_prefix_len(buf, m + 8, ip + 8, block_end)))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let found = match found {
                    Some(found) => found,
                    None => {
                        if cand_short < low_abs {
                            ip += ((ip - anchor) >> SEARCH_STRENGTH) + 1;
                            continue 'search;
                        }
                        let m = (cand_short - base) as usize;
                        if read_u32(buf, m) != v as u32 {
                            ip += ((ip - anchor) >> SEARCH_STRENGTH) + 1;
                            continue 'search;
                        }
                        // A short match. A long match at the next position might be better.
                        let long_next = if DOUBLE {
                            let v1 = read_u64(buf, ip + 1);
                            let h_l1 = hash8(v1, long_log);
                            let cand = tables.hash[h_l1];
                            tables.set_hash_t::<TRACK>(h_l1, cur + 1);
                            let m1 = (cand.wrapping_sub(base)) as usize;
                            if cand >= low_abs && read_u64(buf, m1) == v1 {
                                Some((
                                    ip + 1,
                                    m1,
                                    8 + common_prefix_len(buf, m1 + 8, ip + 9, block_end),
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        long_next.unwrap_or_else(|| {
                            (ip, m, 4 + common_prefix_len(buf, m + 4, ip + 4, block_end))
                        })
                    }
                };
                // Catch up: the match might extend backwards into the literals
                let (mut s, mut m, mut len) = found;
                while s > anchor && m > low && buf[s - 1] == buf[m - 1] {
                    s -= 1;
                    m -= 1;
                    len += 1;
                }
                start = s;
                offset = s - m;
                match_len = len;
            }

            encode_offset_value(offset as u32, (start - anchor) as u32, &mut rep);
            handle_sequence(Sequence::Triple {
                literals: &buf[anchor..start],
                offset,
                match_len,
            });
            let searched = ip;
            ip = start + match_len;
            anchor = ip;

            if ip <= ilimit {
                // Fill the tables with a few positions of the match
                let fill = searched + 2;
                if DOUBLE {
                    let v = read_u64(buf, fill);
                    tables.set_hash_t::<TRACK>(hash8(v, long_log), fill as u32 + base);
                    tables.set_chain_t::<TRACK>(short.hash(v), fill as u32 + base);
                    tables.set_hash_t::<TRACK>(
                        hash8(read_u64(buf, ip - 2), long_log),
                        (ip - 2) as u32 + base,
                    );
                    tables.set_chain_t::<TRACK>(
                        short.hash(read_u64(buf, ip - 1)),
                        (ip - 1) as u32 + base,
                    );
                } else {
                    tables.set_hash_t::<TRACK>(short.hash(read_u64(buf, fill)), fill as u32 + base);
                    tables.set_hash_t::<TRACK>(
                        short.hash(read_u64(buf, ip - 2)),
                        (ip - 2) as u32 + base,
                    );
                }

                // An immediate match at the second most recent offset, which is cheap to encode without literals
                while ip <= ilimit {
                    let r2 = rep[1] as usize;
                    if r2 > ip - low || read_u32(buf, ip - r2) != read_u32(buf, ip) {
                        break;
                    }
                    let len = 4 + common_prefix_len(buf, ip - r2 + 4, ip + 4, block_end);
                    let v = read_u64(buf, ip);
                    if DOUBLE {
                        tables.set_hash_t::<TRACK>(hash8(v, long_log), ip as u32 + base);
                        tables.set_chain_t::<TRACK>(short.hash(v), ip as u32 + base);
                    } else {
                        tables.set_hash_t::<TRACK>(short.hash(v), ip as u32 + base);
                    }
                    encode_offset_value(r2 as u32, 0, &mut rep);
                    handle_sequence(Sequence::Triple {
                        literals: &buf[ip..ip],
                        offset: r2,
                        match_len: len,
                    });
                    ip += len;
                    anchor = ip;
                }
            }
        }

        self.rep = rep;
        if anchor < block_end {
            handle_sequence(Sequence::Literals {
                literals: &buf[anchor..block_end],
            });
        }
    }

    fn match_lazy(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        let block_start = self.block_start;
        let block_end = self.buf.len();
        if block_end - block_start < LAZY_MIN_MATCH {
            if block_end > block_start {
                handle_sequence(Sequence::Literals {
                    literals: &self.buf[block_start..],
                });
            }
            return;
        }
        let src_floor = self.src_floor(block_end);
        let dict_usable = self.dict_usable(block_end);
        let rep = &mut self.rep;
        let buf = self.buf.as_slice();
        let tables = &mut self.tables;
        let next_to_update = &mut self.next_to_update;
        let params = &self.params;
        let base = self.base;

        // Positions of skipped blocks that were dropped from the buffer can not be inserted anymore
        if *next_to_update < base {
            *next_to_update = base;
        }
        let window_size = params.window_size();
        let depth = params.search_depth as usize;
        let chain_mask = tables.chain.len().wrapping_sub(1);
        let chain_size = tables.chain.len();
        let hash_log = params.hash_log;
        // Rows: a hash picks the row (its upper bits) and the tag (its lowest byte)
        let row_log = tables.row_log;
        let rows = row_log != 0;
        let row_entries = 1usize << row_log;
        let row_mask = row_entries - 1;
        let row_hash_bits = hash_log - row_log + 8;
        // Last position at which a search can be started
        let ilimit = block_end - LAZY_MIN_MATCH;

        // The hashes of the next positions to insert, so that their rows can be fetched into the cache ahead of
        // time: the rows are spread over tables larger than the processor's caches, and waiting for a row at
        // every position costs more than everything else the search does.
        let mut row_hashes = [0u32; ROW_LOOKAHEAD];
        if rows {
            let first = (*next_to_update - base) as usize;
            for pos in first..usize::min(first + ROW_LOOKAHEAD, ilimit + 1) {
                row_hashes[pos % ROW_LOOKAHEAD] = hash_lazy(buf, pos, row_hash_bits) as u32;
            }
        }
        // The hash of `pos`, the position after the last one asked for
        let row_hash =
            |tables: &Tables, row_hashes: &mut [u32; ROW_LOOKAHEAD], pos: usize| -> usize {
                let slot = pos % ROW_LOOKAHEAD;
                let h = row_hashes[slot] as usize;
                let ahead = pos + ROW_LOOKAHEAD;
                if ahead <= ilimit {
                    let next = hash_lazy(buf, ahead, row_hash_bits);
                    row_hashes[slot] = next as u32;
                    let first = (next >> 8) << row_log;
                    prefetch(&tables.tags, first);
                    prefetch(&tables.hash, first);
                    if row_log > 4 {
                        prefetch(&tables.hash, first + 16);
                    }
                }
                h
            };
        // Writes a position to its row, before the entry written last
        let insert_in_row = |tables: &mut Tables, h: usize, abs: u32| {
            let first = (h >> 8) << row_log;
            let mut head = (tables.tags[first] as usize).wrapping_sub(1) & row_mask;
            if head == 0 {
                head = row_mask;
            }
            tables.tags[first] = head as u8;
            tables.tags[first + head] = h as u8;
            tables.hash[first + head] = abs;
        };

        let insert =
            |tables: &mut Tables, row_hashes: &mut [u32; ROW_LOOKAHEAD], pos: usize| -> u32 {
                let abs = pos as u32 + base;
                if rows {
                    let h = row_hash(tables, row_hashes, pos);
                    insert_in_row(tables, h, abs);
                    return 0;
                }
                let h = hash_lazy(buf, pos, hash_log);
                let head = tables.hash[h];
                tables.set_chain(abs as usize & chain_mask, head);
                tables.set_hash(h, abs);
                head
            };

        // Searches the repeat offsets and the tables for the most valuable match at `ip` and inserts `ip` into the
        // tables. Returns the length, offset and value (see `score`) of the match; the length is 0 if none was found.
        let mut search = |tables: &mut Tables, rep: &[u32; 3], ip: usize| -> (usize, usize, i64) {
            let cur = ip as u32 + base;
            while *next_to_update < cur {
                insert(tables, &mut row_hashes, (*next_to_update - base) as usize);
                *next_to_update += 1;
            }
            // With rows the position is inserted after its row has been searched
            let mut candidate = if rows {
                0
            } else {
                insert(tables, &mut row_hashes, ip)
            };
            *next_to_update = cur + 1;

            let low_limit = if dict_usable {
                0
            } else {
                usize::max(ip.saturating_sub(window_size), src_floor)
            };
            let low_abs = low_limit as u32 + base;
            let mut best_len = 0;
            let mut best_offset = 0;
            let mut best_score = i64::MIN;
            // A match is at least five bytes: most candidates differ in the first four, which one comparison tells
            let first_bytes = read_u32(buf, ip);
            for &offset in rep {
                let offset = offset as usize;
                if offset > ip || ip - offset < low_limit || offset == best_offset {
                    continue;
                }
                if read_u32(buf, ip - offset) != first_bytes {
                    continue;
                }
                let len = common_prefix_len(buf, ip - offset, ip, block_end);
                if len >= LAZY_MIN_MATCH && score_rep(len) > best_score {
                    best_len = len;
                    best_offset = offset;
                    best_score = score_rep(len);
                }
            }
            if ip + best_len == block_end {
                if rows {
                    insert(tables, &mut row_hashes, ip);
                }
                return (best_len, best_offset, best_score);
            }
            if rows {
                let h = row_hash(tables, &mut row_hashes, ip);
                let first = (h >> 8) << row_log;
                let head = tables.tags[first] as usize & row_mask;
                // The entries with the tag of this position (but the first, which is no entry), the most recent
                // in the lowest bit
                let mut found = tag_matches(&tables.tags[first..first + row_entries], h as u8) & !1;
                found = if row_entries < 64 {
                    ((found >> head) | (found << (row_entries - head)))
                        & ((1u64 << row_entries) - 1)
                } else {
                    found.rotate_right(head as u32)
                };
                // The bytes of the candidates are fetched into the cache before any is compared, so that the waits
                // for them overlap instead of following one another
                let positions = &tables.hash[first..first + row_entries];
                let mut count = 0;
                let mut ahead = found;
                while ahead != 0 && count < depth {
                    let candidate = positions[(head + ahead.trailing_zeros() as usize) & row_mask];
                    if candidate < low_abs || candidate >= cur {
                        // Older entries are further away still
                        break;
                    }
                    ahead &= ahead - 1;
                    prefetch(buf, (candidate - base) as usize);
                    count += 1;
                }
                for _ in 0..count {
                    let candidate = positions[(head + found.trailing_zeros() as usize) & row_mask];
                    found &= found - 1;
                    let cand = (candidate - base) as usize;
                    if read_u32(buf, cand) == first_bytes
                        && buf[cand + best_len] == buf[ip + best_len]
                    {
                        let len = common_prefix_len(buf, cand, ip, block_end);
                        let offset = ip - cand;
                        let value = if rep.contains(&(offset as u32)) {
                            score_rep(len)
                        } else {
                            score(len, offset)
                        };
                        if len >= LAZY_MIN_MATCH && value > best_score {
                            best_len = len;
                            best_offset = offset;
                            best_score = value;
                            if ip + len == block_end {
                                break;
                            }
                        }
                    }
                }
                insert_in_row(tables, h, cur);
                return (best_len, best_offset, best_score);
            }
            let min_chain = cur.saturating_sub(chain_size as u32);
            for _ in 0..depth {
                if candidate < low_abs || candidate >= cur {
                    break;
                }
                let cand = (candidate - base) as usize;
                // Quick rejection: the byte right after the current best match has to match for a longer match
                if buf[cand + best_len] == buf[ip + best_len] {
                    let len = common_prefix_len(buf, cand, ip, block_end);
                    let offset = ip - cand;
                    let value = if rep.contains(&(offset as u32)) {
                        score_rep(len)
                    } else {
                        score(len, offset)
                    };
                    if len >= LAZY_MIN_MATCH && value > best_score {
                        best_len = len;
                        best_offset = offset;
                        best_score = value;
                        if ip + len == block_end {
                            break;
                        }
                    }
                }
                if candidate <= min_chain {
                    // The chain entry of this candidate may have been overwritten already
                    break;
                }
                let next = tables.chain[candidate as usize & chain_mask];
                if next >= candidate {
                    break;
                }
                candidate = next;
            }
            (best_len, best_offset, best_score)
        };

        let mut ip = block_start;
        let mut anchor = block_start;

        while ip <= ilimit {
            let (mut match_len, mut offset, mut value) = search(tables, rep, ip);
            let mut start = ip;
            if match_len < LAZY_MIN_MATCH {
                // Nothing found, skip faster the longer we did not find anything
                ip += ((ip - anchor) >> SEARCH_STRENGTH) + 1;
                continue;
            }

            // Lazy evaluation: is a match starting a little later a better choice?
            let mut lazy_step = 0;
            while lazy_step < params.lazy_steps as usize && ip < ilimit {
                ip += 1;
                let (len2, offset2, value2) = search(tables, rep, ip);
                if len2 >= LAZY_MIN_MATCH {
                    // The further we already moved away from `start` the more the new match has to gain
                    let gain1 = value + 4 + 3 * lazy_step as i64;
                    if value2 > gain1 {
                        match_len = len2;
                        offset = offset2;
                        value = value2;
                        start = ip;
                        lazy_step = 0;
                        continue;
                    }
                }
                lazy_step += 1;
            }

            // Catch up: the match might extend backwards into the literals
            while start > anchor
                && start > offset + src_floor
                && buf[start - 1] == buf[start - 1 - offset]
            {
                start -= 1;
                match_len += 1;
            }

            // Track the history the encoder will have, so the next searches know the repeat offsets
            encode_offset_value(offset as u32, (start - anchor) as u32, rep);
            handle_sequence(Sequence::Triple {
                literals: &buf[anchor..start],
                offset,
                match_len,
            });

            ip = start + match_len;
            anchor = ip;
        }

        if anchor < block_end {
            handle_sequence(Sequence::Literals {
                literals: &buf[anchor..block_end],
            });
        }
    }
}

/// Hash of the first `min_match` bytes of a little endian `u64`
#[derive(Clone, Copy)]
struct ShortHash {
    shift: u32,
    prime: u64,
    hash_shift: u32,
}

impl ShortHash {
    fn new(min_match: u32, hash_log: u32) -> Self {
        // The multipliers of the reference implementation
        const PRIMES: [u64; 5] = [
            2_654_435_761,
            889_523_592_379,
            227_718_039_650_203,
            58_295_818_150_454_627,
            0xCF1B_BCDC_B7A5_6463,
        ];
        let min_match = min_match.clamp(4, 8);
        Self {
            shift: 64 - 8 * min_match,
            prime: PRIMES[min_match as usize - 4],
            hash_shift: 64 - hash_log.max(1),
        }
    }

    #[inline(always)]
    fn hash(&self, v: u64) -> usize {
        ((v << self.shift).wrapping_mul(self.prime) >> self.hash_shift) as usize
    }
}

#[inline(always)]
fn hash8(v: u64, hash_log: u32) -> usize {
    (v.wrapping_mul(0xCF1B_BCDC_B7A5_6463) >> (64 - hash_log)) as usize
}

/// Asks the processor to fetch the cache line of `slice[index]`, which is about to be read.
///
/// Only where the standard library has a stable instruction for it. Elsewhere the element is read instead, which
/// has the same effect as long as the processor runs ahead of the read.
#[inline(always)]
fn prefetch<T: Copy>(slice: &[T], index: usize) {
    if let Some(item) = slice.get(index) {
        #[cfg(target_arch = "x86_64")]
        {
            use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            // SAFETY: a prefetch reads no memory, and the pointer is that of an element of the slice.
            #[allow(unused_unsafe)]
            unsafe {
                _mm_prefetch::<_MM_HINT_T0>(core::ptr::from_ref(item).cast::<i8>())
            };
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            core::hint::black_box(*item);
        }
    }
}

/// The entries of a row (16, 32 or 64 tags) equal to `tag`, as a bit each: the lowest bit is the first entry.
///
/// Eight tags are compared at once in a `u64`: a byte of `x` is zero where the tags are equal, and the usual
/// zero-byte test (exact, without the carries of the shorter one) sets the high bit of those bytes, which a
/// multiplication gathers into one byte.
#[inline(always)]
fn tag_matches(tags: &[u8], tag: u8) -> u64 {
    const LOW7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    let pattern = u64::from(tag) * 0x0101_0101_0101_0101;
    let mut matches = 0;
    for (i, chunk) in tags.chunks_exact(8).enumerate() {
        let x = u64::from_le_bytes(chunk.try_into().unwrap()) ^ pattern;
        let zero = !(((x & LOW7) + LOW7) | x | LOW7);
        matches |= ((zero >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56) << (8 * i);
    }
    matches
}

/// Hash of the 5 bytes at `pos`, used by the hash chains
#[inline(always)]
fn hash_lazy(buf: &[u8], pos: usize, hash_log: u32) -> usize {
    const PRIME_5_BYTES: u64 = 889_523_592_379;
    let v = if let Some(bytes) = buf.get(pos..pos + 8) {
        u64::from_le_bytes(bytes.try_into().unwrap())
    } else {
        let mut v = 0u64;
        for (i, b) in buf[pos..pos + LAZY_MIN_MATCH].iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        v
    };
    ((v << (64 - 8 * LAZY_MIN_MATCH)).wrapping_mul(PRIME_5_BYTES) >> (64 - hash_log)) as usize
}

#[inline(always)]
fn read_u32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap())
}

#[inline(always)]
fn read_u64(buf: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap())
}

/// Heuristic value of a match: about 4 points per byte, minus the bits needed to encode the offset
#[inline(always)]
fn score(match_len: usize, offset: usize) -> i64 {
    4 * match_len as i64 - (offset + 1).ilog2() as i64
}

/// Heuristic value of a match at one of the repeat offsets, which costs almost no offset bits
#[inline(always)]
fn score_rep(match_len: usize) -> i64 {
    4 * match_len as i64
}

/// Length of the common prefix of `buf[a..]` and `buf[b..end]` where `a < b <= end <= buf.len()`
#[inline(always)]
fn common_prefix_len(buf: &[u8], mut a: usize, mut b: usize, end: usize) -> usize {
    let start = b;
    while b + 8 <= end {
        let x = read_u64(buf, a);
        let y = read_u64(buf, b);
        let diff = x ^ y;
        if diff != 0 {
            return b - start + (diff.trailing_zeros() / 8) as usize;
        }
        a += 8;
        b += 8;
    }
    while b < end && buf[a] == buf[b] {
        a += 1;
        b += 1;
    }
    b - start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(seq: Sequence<'_>, reconstructed: &mut Vec<u8>, window_size: usize) {
        match seq {
            Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                reconstructed.extend_from_slice(literals);
                assert!(offset >= 1);
                assert!(offset <= window_size);
                assert!(match_len >= 4);
                let start = reconstructed.len() - offset;
                for i in 0..match_len {
                    let byte = reconstructed[start + i];
                    reconstructed.push(byte);
                }
            }
        }
    }

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.extend_from_slice(&state.to_le_bytes());
        }
        data.truncate(len);
        data
    }

    /// Feeds data block by block through a matcher and checks that the sequences reproduce the input. Returns the
    /// number of sequences.
    fn roundtrip(
        matcher: &mut WindowMatcher,
        params: MatcherParams,
        data: &[u8],
        skip_every: Option<usize>,
        hint: bool,
    ) -> usize {
        matcher.reset(params);
        if hint {
            matcher.source_size_hint(data.len() as u64);
        }
        let window_size = matcher.window_size() as usize;
        let mut reconstructed = Vec::with_capacity(data.len());
        let mut sequences = 0;
        for (block_idx, block) in data.chunks(MAX_BLOCK_SIZE as usize).enumerate() {
            let mut space = matcher.get_next_space();
            space.clear();
            space.extend_from_slice(block);
            matcher.commit_space(space);
            assert_eq!(matcher.get_last_space(), block);
            if skip_every.is_some_and(|n| block_idx % n == 1) {
                matcher.skip_matching();
                reconstructed.extend_from_slice(block);
            } else {
                matcher.start_matching(|seq| {
                    if let Sequence::Triple { .. } = seq {
                        sequences += 1;
                    }
                    apply(seq, &mut reconstructed, window_size)
                });
            }
            assert_eq!(
                reconstructed.len(),
                block_idx * MAX_BLOCK_SIZE as usize + block.len()
            );
        }
        assert_eq!(reconstructed, data);
        sequences
    }

    fn all_params() -> Vec<MatcherParams> {
        let mut params: Vec<MatcherParams> = [
            CompressionLevel::Fastest,
            CompressionLevel::Default,
            CompressionLevel::Better,
            CompressionLevel::Best,
        ]
        .iter()
        .map(|&level| MatcherParams::for_level(level).unwrap())
        .collect();
        // Small tables, so that collisions and chain overwrites happen
        for strategy in [Strategy::Fast, Strategy::DoubleFast, Strategy::Lazy] {
            params.push(MatcherParams {
                strategy,
                window_log: 20,
                hash_log: 12,
                chain_log: 10,
                min_match: 4,
                search_depth: 8,
                lazy_steps: 2,
            });
        }
        params
    }

    fn sequences_of(params: MatcherParams, data: &[u8]) -> Vec<(Vec<u8>, usize, usize)> {
        let mut matcher = WindowMatcher::new(params);
        matcher.reset(params);
        let mut space = matcher.get_next_space();
        space.extend_from_slice(data);
        matcher.commit_space(space);
        let mut seqs = Vec::new();
        matcher.start_matching(|seq| match seq {
            Sequence::Literals { literals } => seqs.push((literals.to_vec(), 0, 0)),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => seqs.push((literals.to_vec(), offset, match_len)),
        });
        seqs
    }

    #[test]
    fn finds_simple_matches() {
        let data = b"abcdefghij_abcdefghij_abcdefghijklmnop";
        for params in all_params() {
            let seqs = sequences_of(params, data);
            assert_eq!(seqs[0], (b"abcdefghij_".to_vec(), 11, 21), "{:?}", params);
            assert_eq!(seqs[1], (b"klmnop".to_vec(), 0, 0), "{:?}", params);
            assert_eq!(seqs.len(), 2);
        }
    }

    #[test]
    fn uses_the_most_recent_offsets() {
        // Records of 16 bytes where only the first 4 bytes change: after the first match every record is a
        // literal run followed by a match at the same offset
        let mut data = Vec::new();
        for i in 0..64u32 {
            data.extend_from_slice(&(i.wrapping_mul(0x9E37_79B9)).to_le_bytes());
            data.extend_from_slice(b"fixed record");
        }
        for params in all_params() {
            let seqs = sequences_of(params, &data);
            let matches: Vec<_> = seqs.iter().filter(|(_, offset, _)| *offset > 0).collect();
            assert!(matches.len() > 50, "{:?}", params);
            assert!(
                matches.iter().all(|(_, offset, _)| *offset == 16),
                "{:?}: {:?}",
                params,
                matches
            );
        }
    }

    #[test]
    fn roundtrip_random_and_repetitive() {
        for params in all_params() {
            let mut matcher = WindowMatcher::new(params);
            for hint in [false, true] {
                roundtrip(&mut matcher, params, &[], None, hint);
                roundtrip(&mut matcher, params, b"abc", None, hint);
                roundtrip(
                    &mut matcher,
                    params,
                    b"abcabcabcabcabcabcabcabc",
                    None,
                    hint,
                );
                roundtrip(&mut matcher, params, &pseudo_random(300_000, 1), None, hint);
                let mut data = pseudo_random(5000, 2);
                for i in 0..100usize {
                    data.extend_from_slice(&pseudo_random(2000 + i, 3 + i as u64));
                    data.extend_from_slice(&pseudo_random(5000, 2));
                }
                roundtrip(&mut matcher, params, &data, None, hint);
                roundtrip(&mut matcher, params, &data, Some(3), hint);
                let zeros = alloc::vec![0u8; 400_000];
                roundtrip(&mut matcher, params, &zeros, None, hint);
                roundtrip(&mut matcher, params, &zeros, Some(2), hint);
            }
        }
    }

    #[test]
    fn roundtrip_across_window_shifts() {
        // Small window so that the buffer gets shifted several times
        for strategy in [Strategy::Fast, Strategy::DoubleFast, Strategy::Lazy] {
            let params = MatcherParams {
                strategy,
                window_log: 18,
                hash_log: 12,
                chain_log: 10,
                min_match: 5,
                search_depth: 4,
                lazy_steps: 1,
            };
            let mut matcher = WindowMatcher::new(params);
            let unit = pseudo_random(100_000, 7);
            let mut data = Vec::new();
            for i in 0..40u8 {
                data.extend_from_slice(&unit);
                data.extend_from_slice(&[i; 1000]);
            }
            roundtrip(&mut matcher, params, &data, None, false);
            roundtrip(&mut matcher, params, &data, Some(5), false);
            // Same matcher, a new frame
            roundtrip(&mut matcher, params, &data[..250_000], None, false);
        }
    }

    #[test]
    fn stale_entries_of_earlier_frames_are_ignored() {
        // Every frame repeats content of the previous frame. A match into the previous frame would reach before the
        // start of the reconstruction, which `apply` does not allow.
        for params in all_params() {
            let mut matcher = WindowMatcher::new(params);
            let unit = pseudo_random(3000, 11);
            for round in 0..20 {
                let mut data = unit.clone();
                data.extend_from_slice(&pseudo_random(500, round));
                let sequences = roundtrip(&mut matcher, params, &data, None, round % 2 == 0);
                // Nothing to match within one frame, except a short match by accident
                assert!(sequences <= 2, "{:?}", params);
            }
        }
    }

    #[test]
    fn positions_restart_before_they_overflow() {
        for params in all_params() {
            let mut matcher = WindowMatcher::new(params);
            let data = {
                let mut d = pseudo_random(20_000, 5);
                d.extend_from_slice(&d.clone());
                d
            };
            roundtrip(&mut matcher, params, &data, None, false);
            // The next frame starts above the limit
            matcher.base = MAX_BASE - 1000;
            let sequences = roundtrip(&mut matcher, params, &data, None, false);
            assert!(matcher.base < 1000);
            // Rows keep `1 << hash_log` positions in all: the small ones have forgotten the first half by then
            let remembered =
                matcher.tables.row_log == 0 || (1usize << params.hash_log) > 2 * 20_000;
            assert!(sequences >= 1 || !remembered);
            // A shift of the window crosses the limit in the middle of a frame
            matcher.reset(params);
            matcher.base = MAX_BASE - 10;
            matcher.next_to_update = matcher.base;
            let mut long = pseudo_random(2_000_000, 3);
            long.extend_from_within(..2_000_000);
            feed(&mut matcher, &long);
            if params.window_size() + SLACK_BLOCKS * (MAX_BLOCK_SIZE as usize) < long.len() {
                assert!(matcher.base < MAX_BASE - 10);
            }
            roundtrip(&mut matcher, params, &data, None, false);
        }
    }

    /// Like `roundtrip`, without resetting the matcher first
    fn feed(matcher: &mut WindowMatcher, data: &[u8]) {
        let window_size = matcher.window_size() as usize;
        let mut reconstructed = Vec::with_capacity(data.len());
        for block in data.chunks(MAX_BLOCK_SIZE as usize) {
            let mut space = matcher.get_next_space();
            space.extend_from_slice(block);
            matcher.commit_space(space);
            matcher.start_matching(|seq| apply(seq, &mut reconstructed, window_size));
        }
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn small_frames_get_small_tables() {
        let params = MatcherParams::for_level(CompressionLevel::Default).unwrap();
        let mut matcher = WindowMatcher::new(params);
        roundtrip(&mut matcher, params, &pseudo_random(1500, 1), None, true);
        assert_eq!(matcher.params().window_log, 11);
        assert_eq!(matcher.params().min_match, 4);
        assert!(matcher.tables.hash.len() <= 1 << 12);
        assert!(matcher.tables.chain.len() <= 1 << 11);
        // Without a hint the level's parameters are used
        roundtrip(&mut matcher, params, &pseudo_random(1500, 1), None, false);
        assert_eq!(matcher.params(), params);
        assert_eq!(matcher.tables.hash.len(), 1 << params.hash_log);
        // The window never shrinks below 1 KiB, and never grows past the level's
        assert_eq!(params.for_size(10).window_log, 10);
        assert_eq!(params.for_size(1 << 30).window_log, params.window_log);
    }
}
