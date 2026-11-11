//! A reader of the backward bitstreams of Zstandard (Huffman-coded literals and FSE-coded
//! sequences) for the decoder's hot loops: it keeps the unread bits at the top of a 64-bit word,
//! so that a table lookup needs one shift, and refills with one unaligned load.

/// The bits of a stream, read from its end.
///
/// `bits` holds the unread bits of the last eight bytes of `src`, most significant first;
/// `consumed` counts the bits read from those eight bytes. After [`BackBits::reload`] at least 57
/// bits are ready, until the start of the stream is near. Bits read past the start of the stream
/// are zeros: [`BackBits::finished`] tells whether exactly the stream was read.
#[derive(Clone, Copy)]
pub(crate) struct BackBits<'s> {
    /// The stream up to the end of the eight bytes `bits` was loaded from: a reload drops the
    /// bytes that were read from its end. That it is the slice that gets shorter, not a
    /// position in it that moves, leaves one comparison to tell that the eight bytes are there.
    src: &'s [u8],
    pub(crate) bits: u64,
    pub(crate) consumed: u32,
    /// The bits of the first word: 64, or fewer for a stream shorter than eight bytes.
    limit: u32,
}

impl<'s> BackBits<'s> {
    /// A reader past the padding of the stream's last byte (zeros, then a one), or `None` when
    /// the stream is empty or its last byte is zero.
    #[inline]
    pub(crate) fn new(src: &'s [u8]) -> Option<Self> {
        let last = *src.last()?;
        if last == 0 {
            return None;
        }
        let consumed = last.leading_zeros() + 1;
        let (word, limit) = if let Some(word) = src.last_chunk::<8>() {
            (u64::from_le_bytes(*word), 64)
        } else {
            let mut buf = [0u8; 8];
            buf[8 - src.len()..].copy_from_slice(src);
            (u64::from_le_bytes(buf), src.len() as u32 * 8)
        };
        Some(BackBits {
            src,
            bits: word << consumed,
            consumed,
            limit,
        })
    }

    /// The bytes before the eight that `bits` was loaded from.
    #[inline(always)]
    fn before(&self) -> usize {
        self.src.len().saturating_sub(8)
    }

    /// Whether [`BackBits::reload_fast`] may be called: eight more bytes precede the word.
    #[inline(always)]
    pub(crate) fn can_reload_fast(&self) -> bool {
        self.src.len() >= 16
    }

    /// Refills the word. Only when [`BackBits::can_reload_fast`], and no more than 64 bits were
    /// read since the last reload.
    #[inline(always)]
    pub(crate) fn reload_fast(&mut self) {
        self.src = &self.src[..self.src.len() - (self.consumed >> 3) as usize];
        self.consumed &= 7;
        let word = self.src.last_chunk::<8>().unwrap();
        self.bits = u64::from_le_bytes(*word) << self.consumed;
    }

    /// Refills the word, as far as the start of the stream allows.
    #[inline(always)]
    pub(crate) fn reload(&mut self) {
        // Without the bytes that were read. Where those are more than the bytes there are, this
        // wraps to more than there are, too.
        let keep = self.src.len().wrapping_sub((self.consumed >> 3) as usize);
        let rest = self.src.get(..keep);
        if let Some((rest, word)) = rest.and_then(|rest| Some((rest, rest.last_chunk::<8>()?))) {
            self.src = rest;
            self.consumed &= 7;
            self.bits = u64::from_le_bytes(*word) << self.consumed;
        } else {
            *self = self.reloaded_near_start();
        }
    }

    /// The reader refilled within eight bytes of the start of the stream. It takes the reader
    /// and gives it back, instead of changing it in place, so that the loops that call it can
    /// keep theirs in registers.
    #[cold]
    #[inline(never)]
    fn reloaded_near_start(mut self) -> Self {
        let before = self.before();
        if before == 0 {
            return self;
        }
        let bytes = ((self.consumed >> 3) as usize).min(before);
        self.src = &self.src[..self.src.len() - bytes];
        self.consumed -= bytes as u32 * 8;
        // `before` is not zero, so eight bytes are left.
        let word = self.src.last_chunk::<8>().unwrap();
        self.bits = u64::from_le_bytes(*word) << (self.consumed & 63);
        self
    }

    /// The next `n` bits (at most 32), which are zeros past the start of the stream.
    #[inline(always)]
    pub(crate) fn read(&mut self, n: u8) -> u32 {
        let value = ((self.bits >> 1) >> (63 - u32::from(n))) as u32;
        self.bits <<= n;
        self.consumed += u32::from(n);
        value
    }

    /// Where the reader of a stream of eight bytes or more is: the position in the stream of the
    /// eight bytes it holds, and the bits read of them. For a loop that reads on by itself and
    /// comes back with [`BackBits::seek`].
    #[inline]
    pub(crate) fn place(&self) -> (usize, u32) {
        (self.before(), self.consumed)
    }

    /// Moves the reader of a stream of eight bytes or more to a place as [`BackBits::place`]
    /// gives it, not after the one it is at. `None` when the place is not in what is left of
    /// the stream.
    #[inline]
    pub(crate) fn seek(&mut self, idx: usize, consumed: u32) -> Option<()> {
        let src = self.src.get(..idx.checked_add(8)?)?;
        let word = src.last_chunk::<8>()?;
        if consumed >= 64 {
            return None;
        }
        self.src = src;
        self.consumed = consumed;
        self.bits = u64::from_le_bytes(*word) << consumed;
        Some(())
    }

    /// Whether exactly the bits of the stream were read. Call after [`BackBits::reload`].
    #[inline]
    pub(crate) fn finished(&self) -> bool {
        self.before() == 0 && self.consumed == self.limit
    }

    /// Whether more bits than the stream holds were read. Call after [`BackBits::reload`].
    #[inline]
    pub(crate) fn overread(&self) -> bool {
        self.before() == 0 && self.consumed > self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::BackBits;

    #[test]
    fn reads_as_the_reversed_reader_does() {
        for len in 1..40usize {
            let data: alloc::vec::Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8 | 1).collect();
            let mut old = crate::bit_io::BitReaderReversed::new(&data);
            let mut skipped = 0;
            while old.get_bits(1) == 0 {
                skipped += 1;
            }
            assert!(skipped < 8);
            let mut new = BackBits::new(&data).unwrap();
            let mut left = old.bits_remaining();
            let mut n = 1u8;
            while left > 0 {
                let take = isize::from(n).min(left) as u8;
                new.reload();
                assert_eq!(u64::from(new.read(take)), old.get_bits(take), "len {len}");
                left -= isize::from(take);
                n = n % 25 + 3;
            }
            new.reload();
            assert!(new.finished(), "len {}", len);
            assert!(!new.overread());
            new.read(1);
            new.reload();
            assert!(new.overread());
        }
        assert!(BackBits::new(&[]).is_none());
        assert!(BackBits::new(&[1, 0]).is_none());
    }

    /// A reader that is moved to a place reads on as one that read its way there.
    #[test]
    fn seeks_to_where_a_reader_was() {
        let data: alloc::vec::Vec<u8> = (0..64usize).map(|i| (i * 29 + 7) as u8 | 1).collect();
        let mut reader = BackBits::new(&data).unwrap();
        let start = BackBits::new(&data).unwrap();
        for step in 0..40 {
            reader.reload();
            let (idx, consumed) = reader.place();
            // From the start of the stream, and from where a reader already is.
            for mut moved in [start, reader] {
                moved.seek(idx, consumed).unwrap();
                assert_eq!(moved.place(), (idx, consumed), "step {step}");
                let mut expected = reader;
                for n in [3u8, 11, 0, 7, 25] {
                    assert_eq!(moved.read(n), expected.read(n));
                }
                moved.reload();
                expected.reload();
                assert_eq!(moved.place(), expected.place());
            }
            reader.read(13);
        }
        // Not past the end of the stream, nor back to where it has been.
        let mut moved = start;
        assert!(moved.seek(data.len() - 7, 0).is_none());
        assert!(moved.seek(usize::MAX, 0).is_none());
        assert!(moved.seek(0, 64).is_none());
        moved.seek(10, 3).unwrap();
        assert!(moved.seek(11, 3).is_none());
    }
}
