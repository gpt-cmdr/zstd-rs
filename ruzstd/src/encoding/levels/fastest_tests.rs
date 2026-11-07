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

#[test]
fn skewed_literals_roundtrip() {
    // Literals with a very skewed distribution over all 256 byte values, so the Huffman codes reach the maximum
    // length, at several block sizes
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut data = Vec::new();
    while data.len() < 3 * 128 * 1024 + 99 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let byte = match state % 64 {
            0 => (state >> 32) as u8,
            1..=8 => b'a' + (state >> 40) as u8 % 26,
            _ => b' ' + (state >> 50) as u8 % 3,
        };
        data.push(byte);
    }
    for len in [1500, 20_000, data.len()] {
        let input = &data[..len];
        let compressed = compress_to_vec(input, CompressionLevel::Fastest);
        assert!(compressed.len() < len * 3 / 4);
        let mut decoded = Vec::with_capacity(len);
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&compressed, &mut decoded)
            .unwrap();
        assert!(decoded == input, "ruzstd did not reproduce the input");
        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert!(decoded == input, "zstd did not reproduce the input");
    }
}
