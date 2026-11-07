//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.
//!
//! The algorithms are implemented by [`super::window_matcher::WindowMatcher`]; [`MatchGeneratorDriver`] is the
//! default [`Matcher`] and sets it up for the compression level.

use alloc::vec::Vec;

use super::window_matcher::{MatcherParams, WindowMatcher};
use super::CompressionLevel;
use super::Matcher;
use super::Sequence;

/// This is the default implementation of the `Matcher` trait. It allocates and reuses the buffers when possible.
///
/// Depending on the compression level passed to [`Matcher::reset`] it uses a greedy single hash table
/// ([`CompressionLevel::Fastest`]), a greedy pair of hash tables ([`CompressionLevel::Default`]) or hash chains
/// with lazy match evaluation (the higher levels), all over a window spanning multiple blocks.
pub struct MatchGeneratorDriver {
    matcher: WindowMatcher,
}

impl MatchGeneratorDriver {
    /// Create a driver that is set up for the given compression level
    pub(crate) fn new(level: CompressionLevel) -> Self {
        Self {
            matcher: WindowMatcher::new(Self::params(level)),
        }
    }

    fn params(level: CompressionLevel) -> MatcherParams {
        // Uncompressed frames only use the window size (for the frame header) and the spaces
        MatcherParams::for_level(level)
            .unwrap_or_else(|| MatcherParams::for_level(CompressionLevel::Fastest).unwrap())
    }
}

impl Matcher for MatchGeneratorDriver {
    fn reset(&mut self, level: CompressionLevel) {
        self.matcher.reset(Self::params(level));
    }

    fn source_size_hint(&mut self, size: u64) {
        self.matcher.source_size_hint(size);
    }

    fn window_size(&self) -> u64 {
        self.matcher.window_size()
    }

    fn get_next_space(&mut self) -> Vec<u8> {
        self.matcher.get_next_space()
    }

    fn get_last_space(&mut self) -> &[u8] {
        self.matcher.get_last_space()
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        self.matcher.commit_space(space)
    }

    fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.matcher.start_matching(handle_sequence)
    }

    fn skip_matching(&mut self) {
        self.matcher.skip_matching()
    }

    fn load_dictionary(&mut self, content: &[u8]) {
        self.matcher.load_dictionary(content)
    }
}
