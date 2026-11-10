//! Per-level block compression logic
mod matched;
#[cfg(test)]
mod tests;
pub(crate) use matched::compress_committed_block;
pub use matched::compress_matched_block;
