# Ruzstd (a pure rust zstd format implementation)

> **This is the gpt-cmdr fork of `ruzstd`.** Its `main` branch is upstream `master` plus a faster and smaller
> compressor: the `Default`, `Better` and `Best` levels and compressor dictionaries from the upstream pull request
> 132 ("Implement Default, Better and Best compression levels", by AlexNitrov, MIT like this crate), repeat offset
> codes, frequency-based Huffman tables, a rewritten FSE encoder with per-block table selection, and matchers
> modelled on the reference implementation's `fast` and `dfast` strategies. See `Changelog.md` for the details.
> The `master` branch tracks upstream.

[![Released API docs](https://docs.rs/ruzstd/badge.svg)](https://docs.rs/ruzstd)
[![CI](https://github.com/killingspark/zstd-rs/workflows/CI/badge.svg)](https://github.com/killingspark/zstd-rs/actions?query=workflow%3ACI)


# What is this

A pure Rust implementation of the Zstandard compression format, as defined in [RFC8878](https://www.rfc-editor.org/rfc/rfc8878.pdf).

This crate contains a fully operational implementation of the decompression portion of the standard.
It also provides a compressor which is usable, but it does not yet reach the speed, ratio or configurability of the original zstd library.

This crate is currently actively maintained.

# Current Status

## Decompression
The `decoding` module provides a complete
implementation of a Zstandard decompressor.

In terms of speed, `ruzstd` is behind the original C implementation
which has a rust binding located [here](https://github.com/gyscos/zstd-rs).

Measuring with the 'time' utility the original zstd and my decoder both
decoding the same enwik9.zst file from a ramfs, my decoder is about 3.5
times slower. Enwik9 is highly compressible, for less compressible data
(like a ubuntu installation .iso) my decoder comes close to only being
1.4 times slower.

## Compression
On the compression side:
- Support for generating compressed blocks at any compression level
  - [x] Uncompressed
  - [x] Fastest (roughly level 1): one hash table, greedy matching, 512 KiB window
  - [x] Default (roughly level 3): two hash tables (8-byte and 5-byte prefixes), greedy matching, 2 MiB window
  - [x] Better: hash chains with lazy matching, 4 MiB window
  - [x] Best: hash chains with lazy matching, 8 MiB window
- [x] Repeat offset codes, searched by every level
- [x] Huffman tables built from the literal frequencies, FSE tables for the sequences chosen per block
  (predefined, RLE, repeated or new) by their estimated cost
- [x] Checksums
- [x] Dictionaries (raw content and the zstd dictionary format, the entropy tables of the latter are not used yet)

Measured on GeoParquet pages (18,376 pages written by a GeoParquet writer for three hydraulic models) and
512x512 float32 GeoTIFF tiles with the floating point predictor (a flood depth grid and a terrain model),
each page or tile compressed as its own frame. Sizes against the reference implementation (libzstd 1.5.7
through the `zstd` crate) on the same data; speeds single threaded on a shared machine, relative to the
reference implementation measured in the same run:

| Data                        | Fastest vs zstd -1 | speed vs zstd -1 | Default vs zstd -3 | speed vs zstd -3 |
|-----------------------------|-------------------:|-----------------:|-------------------:|-----------------:|
| GeoParquet, 29 MB, 6 kB pages   | -12.9 % | 0.56x | +0.4 % | 0.55x |
| GeoParquet, 105 MB, 10 kB pages | -12.7 % | 0.53x | +0.4 % | 0.51x |
| GeoParquet, 1 MB, 1.3 kB pages  | -3.8 %  | 0.54x | +2.2 % | 0.44x |
| GeoParquet, 3.3 MB, 3.8 kB pages| -6.5 %  | 0.44x | +0.8 % | 0.41x |
| GeoParquet, 185 MB, 119 kB pages| -4.0 %  | 0.49x | +0.6 % | 0.55x |
| float32 tiles, depth, 27 MB     | -0.7 %  | 0.45x | +0.3 % | 0.54x |
| float32 tiles, terrain, 104 MB  | +0.1 %  | 0.50x | +0.2 % | 0.49x |
| **All**                         | **-4.5 %** |   | **+0.5 %** |      |

`Better` is 3.7 % smaller than zstd -3 on the same data at about a tenth of its speed, `Best` slower still.

The convenience functions create a new compressor per call. When the whole input fits into one block they
know its size: the frame header then states it, and the matcher shrinks its window and tables to the input as
the reference implementation does, so small inputs stay cheap. Reusing a `FrameCompressor` avoids the
allocations as well.

## Dictionary Generation
When the `dict_builder` feature is enabled, the `dictionary` module
provides the ability to create new dictionaries. 

On the `github-users` sample set, our implementation benchmarks within
0.2% of the official implementation (as of commit 
`09e52d07340acdb2e13817b066e8be6e424f7258`):
```no_build
uncompressed: 100.00% (7484607 bytes)
no dict: 34.99% of original size (2618872 bytes)
reference dict: 16.16% of no dict size (2195672 bytes smaller)
our dict: 16.28% of no dict size (2192400 bytes smaller)
```

The dictionary generator only provides support for creating "raw
content" dictionaries. Tagged dictionaries are currently unsupported.

See <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#dictionary-format>
for clarification.


# How can you use it?

## Compression

The easiest is to use the provided `compress`/`compress_to_vec` functions

```rust, no_run
use ruzstd::encoding::{compress, compress_to_vec, CompressionLevel};
let data: &[u8] = todo!();
// Either
let mut compressed = Vec::new();
compress(data, &mut compressed, CompressionLevel::Fastest);
// or
let compressed = compress_to_vec(data, CompressionLevel::Fastest);
 ```

 Or you can use the `FrameCompressor` manually to compress data. This allows you to process encoded data while it is being encoded instead of collecting into a big vector.

### Compressing with a dictionary

A dictionary acts as data that precedes the input, so inputs that share structure with it compress much better.
Both dictionaries in the zstd dictionary format (as created by `zstd --train` or the `dictionary` module) and
raw content dictionaries (arbitrary bytes) are supported. The decoder needs the same dictionary.

```rust, no_run
use ruzstd::encoding::{FrameCompressor, CompressionLevel, Dictionary};
let raw_dictionary: &[u8] = todo!("read the dictionary file");
let dictionary = Dictionary::parse(raw_dictionary).unwrap();
// Reuse one compressor for many inputs, loading the dictionary is only done once
let mut compressor: FrameCompressor<&[u8], Vec<u8>, _> = FrameCompressor::new(CompressionLevel::Default);
compressor.set_dictionary(dictionary);
for input in [b"first input".as_slice(), b"second input".as_slice()] {
    compressor.set_source(input);
    compressor.set_drain(Vec::new());
    compressor.compress();
    let compressed = compressor.take_drain().unwrap();
    // store `compressed` somewhere
}
```

Frames compressed with a dictionary in the zstd format carry its ID, so `FrameDecoder::add_dict` is enough on
the decoding side. Raw content dictionaries have no ID, register one with
`ruzstd::decoding::Dictionary::from_raw_content` and select it with `FrameDecoder::force_dict` after
initializing the decoder for a frame. The specification only allows references into the dictionary while the
frame holds at most `window_size` bytes, which is 512 KiB for `Fastest` and 2 MiB for `Default`.

## Decompression

Additionally to the descriptions and the docs you can have a look at the zstd / zstd_streaming binaries. They showcase how this library can be used.

### Easy

The easiest is to wrap the io::Read into a StreamingDecoder which itself implements io::Read. It will decode blocks as necessary to fulfill the read requests

```rust, no_run
use ruzstd::decoding::StreamingDecoder;
use ruzstd::io::Read;

let mut source: &[u8] = todo!("Get a reader from a File or any other source");
let mut decoder = StreamingDecoder::new(&mut source).unwrap();

let mut result = Vec::new();
decoder.read_to_end(&mut result).unwrap();
```

This might be a problem if you are accepting user provided data. Frames can be REALLY big when decoded. If this is the case you should either check how big the frame
actually is or use the memory efficient approach described below.

### Memory efficient

If memory is a concern you can decode frames partially. There are two ways to do this:

#### Streaming decoder

Use the StreamingDecoder and use a while loop to fill your buffer (see src/bin/zstd_stream.rs for an example). This is the
recommended approach.

#### Use the lower level FrameDecoder

For an example see the src/bin/zstd.rs file. Basically you can decode the frame until either a
given block count has been decoded or the decodebuffer has reached a certain size. Then you can collect no longer needed bytes from the buffer and do something with them, discard them and resume decoding the frame in a loop until the frame has been decoded completely.

## Roadmap

1. More Performance optimizations
    1. sequence_decoding and reverse_bitreader::get_bits. Those account for about 50% of the whole time used in decoding
    2. Matching suffixes. This accounts for >60% of the whole time used in encoding
2. Implement encoder features
    1. Use the entropy tables of dictionaries

## Testing

Tests take two forms.

1. Tests using well-formed files that have to decode correctly and are checked against their originals
1. Tests using malformed input that have been generated by the fuzzer. These don't have to decode (they are garbage) but they must not make the decoder panic

## Fuzzing

Fuzzing has been done on

1. Random input with no initial corpus
2. The \*.zst in /fuzz_decodecorpus


### You want to help fuzz?

Use `cargo +nightly fuzz run decode` or some other fuzz target to run the fuzzer. It is seeded with files created with decodecorpus.

If the fuzzer finds a crash it will be saved to the artifacts dir by the fuzzer. Run `cargo test artifacts` to run the artifacts tests.
This will tell you where the decoder panics exactly. If you are able to fix the issue please feel free to do a pull request. If not please still submit the offending input and I will see how to fix it myself.

# Contributing

Contributions will be published under the same MIT license as this project. Please make an entry in the Changelog.md file when you make a PR.

## AI Contributions
Contributions may be created by using whatever tools you like. They must be read, verified and decided to be a value added to the project by a human (you! not a maintainer) before submitting a PR and the communication about the PR must be handled by a human and not an agent. This is meant to be more or less in the spirit of the [LLVM policy](https://llvm.org/docs/AIToolPolicy.html) adapted to a much smaller project.
