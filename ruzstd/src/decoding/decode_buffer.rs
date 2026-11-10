//! The window of the streaming decoder: the decoded bytes that were not collected yet, of which
//! the last `window_size` are kept for the matches of the blocks to come.
//!
//! Blocks are decoded straight into the buffer ([`DecodeBuffer::block_target`]), so the bytes of
//! the window have to lie in one piece wherever possible. The buffer is therefore not a ring that
//! wraps at any byte: a block is always written in one piece, and when there is no room for one
//! after the newest bytes, the next block starts over at the front of the buffer, in the space
//! that collecting has freed. The window is then in two pieces, the older one ending where the
//! last block before the turn ended, until the older piece is collected. The reference decoder's
//! streaming mode keeps its window the same way.
//!
//! A reader that asks for all of a frame at once ([`DecodeBuffer::lend`]) gets it decoded
//! straight into its vector, which is the buffer for that time.
//!
//! All of this is safe code over a zeroed `Vec<u8>`.

use crate::io::{Error, Read, Write};
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
#[cfg(feature = "hash")]
use core::hash::Hasher;

use super::direct::History;
use crate::common::MAX_BLOCK_SIZE;

/// The bytes after those of a block that decoding it may write to: see
/// [`BlockScratch::decode_block`](super::direct::BlockScratch::decode_block).
const SLACK: usize = 32;

/// The largest size the buffer starts with for a frame that states the size of its content.
const FIRST_SIZE_STATED: usize = 8 << 20;

/// The bytes not yet collected, oldest first: `buf[head..tail]`, or when `turned`
/// `buf[head..older_end]` and then `buf[..tail]`.
///
/// Invariants: `head <= tail <= buf.len()` when not `turned`; `tail <= head < older_end <=
/// buf.len()` when `turned`. Every byte of `buf` is initialized, those outside the ranges above
/// with whatever was there before.
struct Window {
    buf: Vec<u8>,
    head: usize,
    tail: usize,
    older_end: usize,
    turned: bool,
    /// The buffer is the vector of a reader, whose bytes before `head` are not the decoder's:
    /// it only grows at its end and never turns.
    lent: bool,
    /// Where the frame ends in a lent buffer, if its header states its size.
    lent_end: usize,
    /// The bytes of a lent buffer before this are in the checksum.
    lent_hashed: usize,
}

impl Window {
    fn len(&self) -> usize {
        if self.turned {
            self.older_end - self.head + self.tail
        } else {
            self.tail - self.head
        }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.older_end = 0;
        self.turned = false;
    }

    fn as_slices(&self) -> (&[u8], &[u8]) {
        if self.turned {
            (&self.buf[self.head..self.older_end], &self.buf[..self.tail])
        } else {
            (&self.buf[self.head..self.tail], &[])
        }
    }

    /// Forgets the `n` oldest bytes (`n` is at most `len()`).
    fn drop_first_n(&mut self, n: usize) {
        debug_assert!(n <= self.len());
        if self.turned {
            let older = self.older_end - self.head;
            if n < older {
                self.head += n;
            } else {
                self.turned = false;
                self.older_end = 0;
                self.head = n - older;
            }
        } else {
            self.head += n;
        }
        if !self.turned && self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
    }

    /// Makes `need` bytes writable at the end of a lent buffer, or fewer when the frame states
    /// that it ends before (`exact` asks for all of them).
    ///
    /// The vector is not made larger than the frame states it has to be: a reader that gave it
    /// that capacity keeps its allocation. A compressed block that then decodes to more than
    /// the frame states is left is an error. Past the stated size the vector grows as for a
    /// frame that states none.
    fn make_room_lent(&mut self, need: usize, exact: bool) {
        let want = self.tail + need;
        let new_len = if exact || want <= self.buf.capacity() || self.tail >= self.lent_end {
            want
        } else {
            want.min(self.lent_end)
        };
        // Zeroed block by block, so that no more is touched than is decoded
        if new_len > self.buf.len() {
            self.buf.resize(new_len, 0);
        }
    }

    /// Makes `room` bytes writable at `tail`, in one piece.
    ///
    /// A buffer that is too small is replaced by one of twice the size: of `first` bytes to
    /// begin with, of no more than `goal` bytes unless the bytes not yet collected take more.
    fn make_room(&mut self, room: usize, goal: usize, first: usize) {
        debug_assert!(!self.lent);
        let len = self.len();
        let size = self.buf.len();
        if self.turned {
            if self.head - self.tail >= room {
                return;
            }
        } else {
            if size - self.tail >= room {
                return;
            }
            if self.head >= room {
                // Start over at the front, in what was collected
                self.older_end = self.tail;
                self.tail = 0;
                self.turned = true;
                return;
            }
            if size >= goal && size - len >= room {
                // Less than a block was collected, by a reader that then fell behind. (A
                // buffer below its goal grows instead: it would be moved again and again.)
                self.buf.copy_within(self.head..self.tail, 0);
                self.head = 0;
                self.tail = len;
                return;
            }
        }
        // Collecting did not keep up, or the buffer is not yet as large as it gets: a larger
        // one, with the bytes in one piece at its front. It is zeroed by the allocator, which
        // for a large one need not touch the memory.
        let grown = if size == 0 {
            first
        } else if size < goal {
            size.saturating_mul(2).min(goal)
        } else {
            size.saturating_mul(2)
        };
        let mut buf = vec![0; grown.max(len + room)];
        let (older, newer) = self.as_slices();
        buf[..older.len()].copy_from_slice(older);
        buf[older.len()..len].copy_from_slice(newer);
        self.buf = buf;
        self.clear();
        self.tail = len;
    }
}

pub struct DecodeBuffer {
    window: Window,
    pub dict_content: Vec<u8>,

    pub window_size: usize,
    /// The size of the content of the frame, if its header states it.
    content_size: Option<usize>,
    total_output_counter: u64,
    #[cfg(feature = "hash")]
    pub hash: twox_hash::XxHash64,
}

/// Where the next block of a frame is decoded to.
pub(crate) struct BlockTarget<'a> {
    /// The block goes to `output[pos..]`, which has room for a block of the largest size and
    /// [`SLACK`] bytes (in a lent buffer, at least for what the frame states is left of it).
    /// The bytes before `pos` are the newest of the window.
    pub(crate) output: &'a mut [u8],
    pub(crate) pos: usize,
    /// What precedes `output`.
    pub(crate) history: History<'a>,
}

impl Read for DecodeBuffer {
    fn read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let max_amount = self.can_drain_to_window_size().unwrap_or(0);
        let amount = max_amount.min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }
}

impl DecodeBuffer {
    pub fn new(window_size: usize) -> DecodeBuffer {
        DecodeBuffer {
            window: Window {
                buf: Vec::new(),
                head: 0,
                tail: 0,
                older_end: 0,
                turned: false,
                lent: false,
                lent_end: usize::MAX,
                lent_hashed: 0,
            },
            dict_content: Vec::new(),
            window_size,
            content_size: None,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
        }
    }

    /// Empties the buffer for a frame of this window size. `content_size` is the size of its
    /// content if the frame header states one: no more than that and a block is allocated.
    pub fn reset(&mut self, window_size: usize, content_size: Option<u64>) {
        self.window_size = window_size;
        self.content_size = content_size.map(|size| usize::try_from(size).unwrap_or(usize::MAX));
        self.window.clear();
        self.dict_content.clear();
        self.total_output_counter = 0;
        #[cfg(feature = "hash")]
        {
            self.hash = twox_hash::XxHash64::with_seed(0);
        }
    }

    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Block_Maximum_Size (RFC 8878 3.1.1.2.4): the smaller of the window and 128 KiB.
    pub fn max_block_len(&self) -> usize {
        self.window_size.min(MAX_BLOCK_SIZE as usize)
    }

    /// Room at the end of the window, in one piece: for `exact` bytes, or for a block of the
    /// largest size.
    fn make_room(&mut self, exact: Option<usize>) {
        let room = self.max_block_len() + SLACK;
        if self.window.lent {
            self.window
                .make_room_lent(exact.unwrap_or(room), exact.is_some());
            return;
        }
        // With the window and two blocks the buffer never has to grow or move for a reader that
        // collects after every block: when a block does not fit at the end, more than a block
        // was collected at the front. A frame of a known size needs no more than that size.
        let goal = self
            .window_size
            .saturating_add(2 * room)
            .min(self.content_size.unwrap_or(usize::MAX).saturating_add(room));
        // A header may claim any window and size, and a stream of a few bytes may come with a
        // window of megabytes: the buffer starts smaller than that and grows as bytes arrive
        let first = goal.min(if self.content_size.is_some() {
            FIRST_SIZE_STATED
        } else {
            2 * room
        });
        self.window.make_room(room, goal, first);
    }

    /// Where to decode the next block. [`DecodeBuffer::commit`] adds what was decoded.
    pub(crate) fn block_target(&mut self) -> BlockTarget<'_> {
        self.target(None)
    }

    /// Where the next bytes go: `exact` of them, or a block.
    fn target(&mut self, exact: Option<usize>) -> BlockTarget<'_> {
        self.make_room(exact);
        let window = &mut self.window;
        let (low, high) = window.buf.split_at_mut(window.head);
        let (output, pos, older): (&mut [u8], usize, &[u8]) = if window.turned {
            (low, window.tail, &high[..window.older_end - window.head])
        } else {
            (high, window.tail - window.head, &[])
        };
        // A frame may refer to its dictionary until it has decoded more than its window size
        let window_size = self.window_size as u64;
        let dict_end = if self.total_output_counter <= window_size {
            let left = usize::try_from(window_size - self.total_output_counter);
            pos.saturating_add(left.unwrap_or(usize::MAX))
                .saturating_add(1)
        } else {
            0
        };
        BlockTarget {
            output,
            pos,
            history: History {
                window: older,
                dict: &self.dict_content,
                dict_end,
            },
        }
    }

    /// Makes `output` the buffer, to decode the frame after the bytes it holds: only when the
    /// buffer is empty. Returns the buffer's own allocation, for [`DecodeBuffer::take_back`].
    pub(crate) fn lend(&mut self, output: &mut Vec<u8>) -> Vec<u8> {
        debug_assert_eq!(self.window.len(), 0);
        if let Some(content_size) = self.content_size {
            // A header may state any size: only what a window or so takes is reserved at once
            let at_once = content_size.min(self.window_size.max(1 << 20));
            output.reserve(at_once);
        }
        let own = core::mem::replace(&mut self.window.buf, core::mem::take(output));
        self.window.clear();
        self.window.lent_end = self.content_size.map_or(usize::MAX, |size| {
            self.window.buf.len().saturating_add(size)
        });
        self.window.head = self.window.buf.len();
        self.window.tail = self.window.buf.len();
        self.window.lent_hashed = self.window.buf.len();
        self.window.lent = true;
        own
    }

    /// Adds what was decoded into the lent buffer to the checksum: after each block, while its
    /// bytes are in the cache.
    pub(crate) fn hash_lent(&mut self) {
        #[cfg(feature = "hash")]
        self.hash
            .write(&self.window.buf[self.window.lent_hashed..self.window.tail]);
        self.window.lent_hashed = self.window.tail;
    }

    /// Returns the vector of [`DecodeBuffer::lend`] to `output`, with what was decoded since.
    /// The bytes count as collected: they are in the checksum and no longer in the buffer.
    pub(crate) fn take_back(&mut self, own: Vec<u8>, output: &mut Vec<u8>) {
        self.hash_lent();
        self.window.buf.truncate(self.window.tail);
        *output = core::mem::replace(&mut self.window.buf, own);
        self.window.clear();
        self.window.lent = false;
    }

    /// Adds the `len` bytes decoded to the [`DecodeBuffer::block_target`] to the window.
    pub(crate) fn commit(&mut self, len: usize) {
        self.window.tail += len;
        self.total_output_counter += len as u64;
    }

    /// Adds `fill_length` (at most a block) times `fill_with`. As before the window was decoded
    /// into, the bytes of blocks that are not compressed do not count towards the size after
    /// which the dictionary is out of reach.
    pub fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        let target = self.target(Some(fill_length));
        target.output[target.pos..target.pos + fill_length].fill(fill_with);
        self.window.tail += fill_length;
    }

    /// Adds `fill_length` bytes (at most a block) of `read`, or nothing when it has fewer.
    pub fn extend_from_reader<R: Read>(
        &mut self,
        mut read: R,
        fill_length: usize,
    ) -> Result<(), crate::io::Error> {
        let target = self.target(Some(fill_length));
        read.read_exact(&mut target.output[target.pos..target.pos + fill_length])?;
        self.window.tail += fill_length;
        Ok(())
    }

    /// Adds `data` (at most a block).
    #[cfg(test)]
    pub fn push(&mut self, data: &[u8]) {
        let target = self.target(Some(data.len()));
        target.output[target.pos..target.pos + data.len()].copy_from_slice(data);
        self.commit(data.len());
    }

    /// Adds the `match_length` bytes (at most a block) that start `offset` bytes back.
    #[cfg(test)]
    pub fn repeat(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), crate::decoding::errors::DecodeBufferError> {
        use crate::decoding::errors::ExecuteSequencesError;
        let target = self.target(Some(match_length));
        if offset <= target.pos {
            for at in target.pos..target.pos + match_length {
                target.output[at] = target.output[at - offset];
            }
        } else {
            super::direct::copy_from_history(
                target.output,
                target.pos,
                match_length,
                offset,
                &target.history,
            )
            .map_err(|e| match e {
                ExecuteSequencesError::DecodebufferError(e) => e,
                other => panic!("{:?}", other),
            })?;
        }
        self.commit(match_length);
        Ok(())
    }

    /// Check if and how many bytes can currently be drawn from the buffer
    pub fn can_drain_to_window_size(&self) -> Option<usize> {
        if self.window.len() > self.window_size {
            Some(self.window.len() - self.window_size)
        } else {
            None
        }
    }

    //How many bytes can be drained if the window_size does not have to be maintained
    pub fn can_drain(&self) -> usize {
        self.window.len()
    }

    /// Drain as much as possible while retaining enough so that decoding si still possible with the required window_size
    /// At best call only if can_drain_to_window_size reports a 'high' number of bytes to reduce allocations
    pub fn drain_to_window_size(&mut self) -> Option<Vec<u8>> {
        //TODO investigate if it is possible to return the std::vec::Drain iterator directly without collecting here
        match self.can_drain_to_window_size() {
            None => None,
            Some(can_drain) => {
                let mut vec = Vec::with_capacity(can_drain);
                self.drain_to(can_drain, |buf| {
                    vec.extend_from_slice(buf);
                    (buf.len(), Ok(()))
                })
                .ok()?;
                Some(vec)
            }
        }
    }

    pub fn drain_to_window_size_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        match self.can_drain_to_window_size() {
            None => Ok(0),
            Some(can_drain) => self.drain_to(can_drain, |buf| write_all_bytes(&mut sink, buf)),
        }
    }

    /// drain the buffer completely
    pub fn drain(&mut self) -> Vec<u8> {
        let (slice1, slice2) = self.window.as_slices();
        #[cfg(feature = "hash")]
        {
            self.hash.write(slice1);
            self.hash.write(slice2);
        }

        let mut vec = Vec::with_capacity(slice1.len() + slice2.len());
        vec.extend_from_slice(slice1);
        vec.extend_from_slice(slice2);
        self.window.clear();
        vec
    }

    pub fn drain_to_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        let write_limit = self.window.len();
        self.drain_to(write_limit, |buf| write_all_bytes(&mut sink, buf))
    }

    pub fn read_all(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let amount = self.window.len().min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }

    /// Semantics of write_bytes:
    /// Should dump as many of the provided bytes as possible to whatever sink until no bytes are left or an error is encountered
    /// Return how many bytes have actually been dumped to the sink.
    fn drain_to(
        &mut self,
        amount: usize,
        mut write_bytes: impl FnMut(&[u8]) -> (usize, Result<(), Error>),
    ) -> Result<usize, Error> {
        if amount == 0 {
            return Ok(0);
        }

        struct DrainGuard<'a> {
            window: &'a mut Window,
            amount: usize,
        }

        impl Drop for DrainGuard<'_> {
            fn drop(&mut self) {
                if self.amount != 0 {
                    self.window.drop_first_n(self.amount);
                }
            }
        }

        let mut drain_guard = DrainGuard {
            window: &mut self.window,
            amount: 0,
        };

        let (slice1, slice2) = drain_guard.window.as_slices();
        let n1 = slice1.len().min(amount);
        let n2 = slice2.len().min(amount - n1);

        if n1 != 0 {
            let (written1, res1) = write_bytes(&slice1[..n1]);
            #[cfg(feature = "hash")]
            self.hash.write(&slice1[..written1]);
            drain_guard.amount += written1;

            // Apparently this is what clippy thinks is the best way of expressing this
            res1?;

            // Only if the first call to write_bytes was not a partial write we can continue with slice2
            // Partial writes SHOULD never happen without res1 being an error, but lets just protect against it anyways.
            if written1 == n1 && n2 != 0 {
                let (written2, res2) = write_bytes(&slice2[..n2]);
                #[cfg(feature = "hash")]
                self.hash.write(&slice2[..written2]);
                drain_guard.amount += written2;

                // Apparently this is what clippy thinks is the best way of expressing this
                res2?;
            }
        }

        let amount_written = drain_guard.amount;
        // Make sure we don't accidentally drop `DrainGuard` earlier.
        drop(drain_guard);

        Ok(amount_written)
    }
}

/// Like Write::write_all but returns partial write length even on error
fn write_all_bytes(mut sink: impl Write, buf: &[u8]) -> (usize, Result<(), Error>) {
    let mut written = 0;
    while written < buf.len() {
        match sink.write(&buf[written..]) {
            Ok(0) => return (written, Ok(())),
            Ok(w) => written += w,
            Err(e) => return (written, Err(e)),
        }
    }
    (written, Ok(()))
}

#[cfg(test)]
mod tests {
    use super::DecodeBuffer;
    use crate::decoding::errors::DecodeBufferError;
    use crate::io::{Error, ErrorKind, Read, Write};

    extern crate std;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn short_writer() {
        struct ShortWriter {
            buf: Vec<u8>,
            write_len: usize,
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
                if buf.len() > self.write_len {
                    self.buf.extend_from_slice(&buf[..self.write_len]);
                    Ok(self.write_len)
                } else {
                    self.buf.extend_from_slice(buf);
                    Ok(buf.len())
                }
            }

            fn flush(&mut self) -> std::result::Result<(), Error> {
                Ok(())
            }
        }

        let mut short_writer = ShortWriter {
            buf: vec![],
            write_len: 10,
        };

        let mut decode_buf = DecodeBuffer::new(100);
        decode_buf.push(b"0123456789");
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
            assert_eq!(decode_buf.len(), 150);
            decode_buf
                .drain_to_window_size_writer(&mut short_writer)
                .unwrap();
            assert_eq!(decode_buf.len(), 100);
        }

        assert_eq!(short_writer.buf.len(), repeats * 50);
        decode_buf.drain_to_writer(&mut short_writer).unwrap();
        assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
        for (at, byte) in short_writer.buf.iter().enumerate() {
            assert_eq!(*byte, b'0' + (at % 10) as u8);
        }
    }

    #[test]
    fn wouldblock_writer() {
        struct WouldblockWriter {
            buf: Vec<u8>,
            last_blocked: usize,
            block_every: usize,
        }

        impl Write for WouldblockWriter {
            fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
                if self.last_blocked < self.block_every {
                    self.buf.extend_from_slice(buf);
                    self.last_blocked += 1;
                    Ok(buf.len())
                } else {
                    self.last_blocked = 0;
                    Err(Error::from(ErrorKind::WouldBlock))
                }
            }

            fn flush(&mut self) -> std::result::Result<(), Error> {
                Ok(())
            }
        }

        let mut short_writer = WouldblockWriter {
            buf: vec![],
            last_blocked: 0,
            block_every: 5,
        };

        let mut decode_buf = DecodeBuffer::new(100);
        decode_buf.push(b"0123456789");
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
            assert_eq!(decode_buf.len(), 150);
            loop {
                match decode_buf.drain_to_window_size_writer(&mut short_writer) {
                    Ok(written) => {
                        if written == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() == ErrorKind::WouldBlock {
                            continue;
                        } else {
                            panic!("Unexpected error {:?}", e);
                        }
                    }
                }
            }
            assert_eq!(decode_buf.len(), 100);
        }

        assert_eq!(short_writer.buf.len(), repeats * 50);
        loop {
            match decode_buf.drain_to_writer(&mut short_writer) {
                Ok(written) => {
                    if written == 0 {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        continue;
                    } else {
                        panic!("Unexpected error {:?}", e);
                    }
                }
            }
        }
        assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
    }

    /// A generator of reproducible numbers.
    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize % n
        }
    }

    /// Literals, matches and reads in a random order against a plain vector, with readers that
    /// collect after every step (the window turns around) and ones that fall behind (it grows).
    #[test]
    fn behaves_as_a_vector_of_all_bytes() {
        for &(window_size, eager, seed) in &[
            (64usize, true, 1u64),
            (64, false, 2),
            (1000, true, 3),
            (1000, false, 4),
            (5000, true, 5),
            (200_000, true, 6),
        ] {
            let mut rng = Lcg(seed);
            let mut buffer = DecodeBuffer::new(1);
            buffer.reset(window_size, None);
            let max = buffer.max_block_len();
            // Everything decoded, and how much of it was collected
            let mut all: Vec<u8> = Vec::new();
            let mut collected: Vec<u8> = Vec::new();
            let mut turns = 0;
            let steps = if window_size > 100_000 { 400 } else { 4000 };
            for step in 0..steps {
                let longest = if rng.below(4) == 0 { max } else { max.min(40) };
                let n = 1 + rng.below(longest);
                let available = all.len() - collected.len();
                if rng.below(3) == 0 || available == 0 {
                    let data: Vec<u8> = (0..n).map(|i| (step * 7 + i * 13) as u8).collect();
                    buffer.push(&data);
                    all.extend_from_slice(&data);
                } else {
                    // Sometimes further back than there are bytes
                    let offset = 1 + rng.below(available + available / 8 + 1);
                    let result = buffer.repeat(offset, n);
                    if offset > available {
                        assert!(result.is_err(), "offset {} of {}", offset, available);
                    } else {
                        result.unwrap();
                        for _ in 0..n {
                            all.push(all[all.len() - offset]);
                        }
                    }
                }
                turns += usize::from(buffer.window.turned);
                assert_eq!(buffer.len(), all.len() - collected.len());
                if eager || rng.below(20) == 0 {
                    let mut out = vec![0; 1 + rng.below(3 * max)];
                    loop {
                        let n = buffer.read(&mut out).unwrap();
                        collected.extend_from_slice(&out[..n]);
                        if n == 0 {
                            break;
                        }
                    }
                    assert!(buffer.len() <= window_size);
                }
            }
            if eager {
                assert!(turns > 0, "window {}", window_size);
                // The window and two blocks, with what a block may write past its end
                assert!(buffer.window.buf.len() <= window_size + 2 * (max + 32));
            }
            collected.extend_from_slice(&buffer.drain());
            assert!(collected == all, "window {} eager {}", window_size, eager);
            assert_eq!(buffer.len(), 0);
        }
    }

    /// Matches reach the dictionary until more than the window size was decoded, also from a
    /// window in two pieces, and run on from the dictionary into the window.
    #[test]
    fn matches_from_the_dictionary() {
        let mut buffer = DecodeBuffer::new(1);
        buffer.reset(100, None);
        buffer.dict_content.extend_from_slice(b"abcdefgh");
        // Nothing decoded: all of the match is in the dictionary, then it runs on
        buffer.repeat(3, 2).unwrap();
        buffer.repeat(8 + 2, 12).unwrap();
        assert!(matches!(
            buffer.repeat(8 + 14 + 1, 1),
            Err(DecodeBufferError::NotEnoughBytesInDictionary { got: 8, need: 9 })
        ));
        buffer.push(&[b'-'; 86]);
        // 100 bytes decoded: the dictionary is still there
        buffer.repeat(108, 1).unwrap();
        // 101 bytes: no more
        assert!(matches!(
            buffer.repeat(109, 1),
            Err(DecodeBufferError::OffsetTooBig {
                offset: 109,
                buf_len: 101
            })
        ));
        let all = buffer.drain();
        assert_eq!(&all[..14], b"fgabcdefghfgab");
        assert_eq!(all[100], b'a');
    }
}
