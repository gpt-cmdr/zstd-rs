use alloc::vec::Vec;

use crate::encoding::{compress_to_vec, CompressionLevel};

#[test]
fn fastest_does_not_expand_incompressible_blocks_past_raw_size() {
    assert_fastest_does_not_exceed_raw(8 * 1024);
}

#[test]
fn fastest_does_not_expand_incompressible_max_size_blocks() {
    assert_fastest_does_not_exceed_raw(128 * 1024);
}

fn assert_fastest_does_not_exceed_raw(len: usize) {
    let data = xorshift(len);
    let raw = compress_to_vec(data.as_slice(), CompressionLevel::Uncompressed);
    let fastest = compress_to_vec(data.as_slice(), CompressionLevel::Fastest);

    assert!(
        fastest.len() <= raw.len(),
        "fastest output should not exceed raw frame size for {len} bytes: {} > {}",
        fastest.len(),
        raw.len()
    );
}

fn xorshift(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9ABC_DEF0u64;
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

/// Records of 16 bytes where some fields repeat, so many matches share their offset with an earlier match
fn strided_records(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut data = Vec::with_capacity(len + 16);
    let mut id = 0u32;
    while data.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        id += 1 + (state % 3) as u32;
        data.extend_from_slice(&id.to_le_bytes());
        data.extend_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        data.extend_from_slice(&((state >> 40) as u16 & 0x0F0F).to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, (state >> 60) as u8]);
    }
    data.truncate(len);
    data
}

fn assert_roundtrip(data: &[u8], level: CompressionLevel) {
    let compressed = compress_to_vec(data, level);
    let mut decoded = Vec::with_capacity(data.len());
    crate::decoding::FrameDecoder::new()
        .decode_all_to_vec(&compressed, &mut decoded)
        .unwrap();
    assert!(decoded == data, "ruzstd did not reproduce the input");
    let mut decoded = Vec::new();
    zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
    assert!(decoded == data, "zstd did not reproduce the input");
}

#[test]
fn repeated_offsets_roundtrip() {
    assert_roundtrip(
        &strided_records(3 * 128 * 1024 + 77, 3),
        CompressionLevel::Fastest,
    );
}

#[test]
fn offset_history_survives_raw_blocks() {
    // A block that does not compress is stored raw, so the sequences found in it never reach the decoder and do
    // not change its offset history. Each noise block below contains two matches at the distance that the next
    // (compressible) block repeats with. If the encoder took the offsets of the raw block into its history it would
    // encode the next block's first match as a repeat of an offset the decoder has never seen.
    let mut data = Vec::new();
    for round in 0..6usize {
        let period = 300 + 37 * round;
        let mut noise = xorshift(128 * 1024 + round);
        // Two short matches: they save less than the sequence section costs, so the block is stored raw
        for at in [40_000, 90_000] {
            let copy: Vec<u8> = noise[at - period..at - period + 6].to_vec();
            noise[at..at + 6].copy_from_slice(&copy);
        }
        data.extend_from_slice(&noise);
        let unit = xorshift(period + round + 1);
        let unit = &unit[unit.len() - period..];
        let block_start = data.len();
        while data.len() < block_start + 128 * 1024 {
            data.extend_from_slice(unit);
        }
        data.truncate(block_start + 128 * 1024);
    }
    assert_roundtrip(&data, CompressionLevel::Fastest);
}
