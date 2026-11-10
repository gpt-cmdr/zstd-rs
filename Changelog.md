# Changelog

This document records the changes made between versions, starting with version 0.5.0

# Unreleased

Speed, measured against the zstd C library (1.5.7, through the `zstd` crate) on 1 MiB chunks of rasters (USGS
elevation as Float32, SRTM as Int16, Landsat as 4 x UInt8, each with and without the TIFF predictor), a frame per
chunk, on an x86-64 desktop. `cargo run --release --example rasterbench` reproduces the comparison on any files.

* Decoding whole frames (`FrameDecoder::decode_all`) took 1.5 to 5.2 times as long as the C library and now takes
  0.99 to 1.25 times as long. Frames without a dictionary are decoded straight into the output slice, which is its
  own window, instead of into the window ring buffer and out of it: blocks are read in place, sequences are
  executed as they are decoded, from tables that hold the base value and extra bits of each state, and the
  bitstream is refilled at most twice per sequence. The bytes of the output after those of the frames may be
  written to.
* Huffman-coded literals are decoded four streams side by side through an 11-bit table, five lookups per refill,
  and two symbols per lookup where the literals compress well. The streaming decoder uses the same decoding (it
  still takes 1.5 to 4.6 times as long as the C library, for its window buffer)
* `decode_all` with the `hash` feature compares the checksum a frame ends with to that of the content decoded:
  `FrameDecoderError::ChecksumMismatch` when they differ. The streaming decoder reports both and leaves the
  comparison to the caller, as before
* New `decoding::content_size`: the size the headers of whole frames record, to size the output of `decode_all`
* The lazy strategy (`Better`, `Best`) without a dictionary keeps the positions of a hash in rows of 16 to 64 with
  a byte of the hash each, as the reference implementation's row match finder does, instead of hash chains. Rows
  are fetched into the cache eight positions ahead and candidates before they are compared. `Better` took 2.8 to
  7.4 times as long as level 9 of the C library, to the same sizes, and now takes 1.2 to 1.7 times as long. It
  looks at 16 candidates per search, as level 9 does (it looked at 32). `Best` compresses 3 to 8 times faster than
  before; frames with a dictionary keep the hash chains
* `Fastest` hashes 7-byte prefixes for data over 256 KiB and 6-byte ones below, as level 1 of the reference
  implementation: its frames were up to 3% smaller than level 1's and are now within 1%, and it takes 1.3 to 1.5
  times as long as level 1 (it took 1.5 to 2.3 times as long). `Default` takes 1.25 to 1.5 times as long as level 3
  (1.05 to 1.8 before)
* New `encoding::Compressor`: compresses whole buffers to frames that state their content size and carry no
  checksum (as `ZSTD_compress`), with tables allocated once instead of for every call
* A content size of 4 GiB or more was written with the wrong field size flag
* `Fastest` and `Default` search with the loops of the reference implementation: two positions per turn with the
  hash and table entry of each ready a step ahead (`Fastest`), the long entry of the next position loaded a step
  ahead (`Default`), one multiplication and one shift per hash. Sequences are collected inside those loops and
  their codes worked out afterwards; Huffman codes are gathered five to a word by multiplication. With a
  `Compressor`, on the desktop, `Fastest` took 1.31 to 1.49 times as long as level 1 of the C library and now
  takes 1.01 to 1.16 times as long; `Default` took 1.20 to 1.59 times as long as level 3 and now takes 1.01 to
  1.37 times as long (1.01 to 1.27 on five of the six files). Sizes are within 0.5% of the C library's, as before.
  On the hosted runners (geometric mean of the six files, levels 1 and 3): Windows 1.29 and 1.43 before, 1.17 and
  1.21 now; Linux x86-64 1.70 and 1.55 before, 1.34 and 1.47 now; Linux AArch64 1.67 and 1.44 before, 1.37 and
  1.35 now
* `Compressor::compress` returns the frame in a buffer with room for incompressible data, which never has to be
  moved while it grows (`shrink_to_fit` frames that are kept, or use `compress_into`), and copies each block of
  the data once instead of twice
* The lazy strategy with rows (`Better`, `Best`) no longer inserts every position it steps over: where nothing
  was found for about 2 KiB only the positions that are searched are inserted, and of more than 384 positions to
  insert at once only the first 96 and the last 32, as the reference implementation does. `Better` on nearly
  incompressible data (Float32 elevation) took 1.7 times as long as level 9 on the desktop, 2.1 to 3.2 times on
  the x86-64 runners, and now takes 0.6 to 0.9 times and 1.1 to 1.7 times as long, to frames 0.35% larger (as
  large as level 9's). Other files do not change by more than 0.01% in size
* Rows and candidates are prefetched on AArch64 too (`prfm`). They were read instead, which the processor waits
  for: `Better` took 2.1 to 2.6 times as long as level 9 there (5.5 times on Float32 elevation) and, with the
  change above, now takes 1.2 to 1.6 times as long. Other targets than x86-64 and AArch64 neither prefetch nor read

# zstd-rust 0.1.0

First release under the name `zstd-rust` (library name `zstd_rust`), an independently maintained fork of `ruzstd`,
not affiliated with the `ruzstd` project. It is `ruzstd`'s `master` branch at commit
`91a3a6eb4cb8a0252ef73f5a561e30c790635774` (after the `ruzstd` 0.9.1 release; its manifest said 0.9.2, unreleased) plus
the changes below. Everything under "After 0.9.1" is part of this release too.

* Package renamed to `zstd-rust`, library to `zstd_rust`; `ruzstd = { package = "zstd-rust", version = "0.1" }` keeps
  `ruzstd::` paths in dependent code. Repository, homepage and documentation point to the fork
* `LICENSE` keeps the `ruzstd` copyright and adds the fork's; the crate directory carries real copies of `LICENSE`
  and `Readme.md` instead of symbolic links, so the package is the same when built on a host without symlinks
* The command line tool (`ruzstd-cli`) is not published

The first commits ("Implement Default, Better and Best compression levels with a lazy hash-chain matcher" and "Add
dictionary support to the compressor") are by Alex Nitrov, taken unchanged with their authorship (MIT, like this
crate). On top of them:

* `Fastest` matches with one hash table of 6-byte prefixes over a 512 KiB window, `Default` with a table of 8-byte and
  one of 5-byte prefixes over 2 MiB (both greedy, like the reference implementation's `fast` and `dfast`
  strategies). `Better` and `Best` keep the hash chains with lazy matching. All levels share one matcher over a
  contiguous window (`MatchGeneratorDriver` keeps its public interface).
* All levels try the most recent offsets first, the greedy ones also take an immediate match at the second most
  recent offset after each match
* Table entries are absolute positions, so a reused compressor never clears its tables between frames
* New `Matcher::source_size_hint` (with a default implementation). The frame compressor reads the first block
  before writing the frame header; if it is the whole input the header states the content size (single segment)
  and the matcher shrinks its window and tables to the input
* `Matcher::get_next_space` may return an empty vector, which the compressor fills up to the block size
* The FSE encoder uses the usual tANS tables (constant time per symbol), normalizes counts proportionally, and
  chooses the predefined, RLE, repeated or a new table per field and block by estimated cost
* Entropy tables and the offset history are only committed when a block is emitted as a compressed block; a block
  that falls back to raw no longer leaves a Huffman table behind that the decoder never saw
* Entropy-coded bits are flushed once per sequence or per four Huffman symbols instead of being checked per write
* Compressed blocks are written straight into the output buffer

# After 0.9.1 (Current)

* Apply the per-block decoded size limit to a block's literals, including blocks with no sequences
* Implement the `Default`, `Better` and `Best` compression levels using a hash chain matcher with lazy
  match evaluation over a window spanning multiple blocks (2/4/8 MiB)
* Fix a panic in the FSE table builder when a block's sequences only use a single symbol
  (e.g. every sequence has zero literals)
* Dictionary support for the compressor: `encoding::Dictionary` (zstd dictionary format or raw content),
  `FrameCompressor::set_dictionary`, `compress_with_dictionary`/`compress_to_vec_with_dictionary`, a default
  `Matcher::load_dictionary` method, `decoding::Dictionary::from_raw_content` and a `--dictionary` option
  for the cli
* Encode offsets that repeat one of the three most recent offsets as repeat codes when compressing
* The matcher of the `Default`, `Better` and `Best` levels tries the three most recent offsets before the hash chain and
  values matches at them as nearly free
* Build the Huffman tables for literals from the symbol frequencies (an optimal code limited to 11 bits) instead of from
  the rank of each symbol only

# After 0.9.0

* Reduce memory footprint while decoding
* Match zstd default maximum window size of 128MiB
* Avoid decoding more bytes than allowed per block to protect agains malformed blocks triggering an OOM panic

# After 0.8.3

* Avoid emitting compressed blocks when the compressed payload is not smaller
  than the raw block.
* Fix Dictionary decoding. It should not panic on invalid inputs.
* Make the decode window size limit configurable via `FrameDecoder::set_max_window_size`/`max_window_size`, `StreamingDecoder::new_with_max_window_size`, and the `DEFAULT_MAX_WINDOW_SIZE` constant. The default stays 100mb.
* Apply the window size limit to the first frame of a stream, not just later frames.
* **Breaking** `FrameDecoderError::WindowSizeTooBig` gained a `max` field and now reports the effective limit.

# After 0.8.2
* Introduce the `rust-version` field
* Fix checksum generation when repeatedly using the encoder
* Expose decoding::Dictionary as public
* Add Debug derive to CompressionLevel enum
* Make RLE and Raw block decoding more efficient and not use intermediary buffer on the stack

# After 0.8.1
* The CLI has been refactored to use `clap`
* The MatchDriverGenerator has been made public so users can name it as `M` in `FrameCompressor<R,W,M>`

# After 0.8.0
* The compressor now includes a `content_checksum` when the `hash` feature is enabled
* Dictionary generation has been added

# After 0.7.3
* Add initial compression support
* **Breaking** Refactor modules to reflect that this is now also a compression library

# After 0.7.2
* Soundness fix in decoding::RingBuffer. The lengths of the diferent regions where sometimes calculated wrongly, resulting in reads of heap memory not belonging to that ringbuffer
    * Fixed by https://github.com/paolobarbolini
    * Affected versions: 0.7.0 up to and including 0.7.2

* Added convenience functions to FrameDecoder to decode multiple frames from a buffer (https://github.com/philipc)

# After 0.7.1

* Remove byteorder dependency (https://github.com/workingjubilee)
* Preparations to become a std dependency (https://github.com/workingjubilee)

# After 0.7.0
* Fix for drain_to functions into limited targets (https://github.com/michaelkirk)

# After 0.6.0
* Small fix in the zstd binary, progress tracking was slighty off for skippable frames resulting in an error only when the last frame in a file was skippable
* Small performance improvement by reorganizing code with `#[cold]` annotations
* Documentation for `StreamDecoder` mentioning the limitations around multiple frames (https://github.com/Sorseg)
* Documentation around skippable frames (https://github.com/Sorseg)
* **Breaking** `StreamDecoder` API changes to get access to the inner parts (https://github.com/ifd3f)
* Big internal documentation contribution (https://github.com/zleyyij)
* Dropped derive_more as a dependency (https://github.com/xd009642)
* Small improvement by removing the error cases from the reverse bitreader (and making sure invalid requests can't even happen)

# After 0.5.0
* Make the hashing checksum optional (thanks to [@tamird](https://github.com/tamird))
    * breaking change as the public API changes based on features
* The FrameDecoder is now Send + Sync (RingBuffer impls these traits now)
