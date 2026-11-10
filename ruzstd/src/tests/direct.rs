//! `FrameDecoder::decode_all` (frames decoded straight into the output) against the streaming
//! decoder, which decodes through a window buffer.
#![cfg(test)]

use crate::decoding::{FrameDecoder, StreamingDecoder};
use crate::io::Read;
use alloc::vec;
use alloc::vec::Vec;

/// What the streaming decoder makes of `data`, `None` when it fails.
fn streamed(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let mut frame = StreamingDecoder::new(&mut data).ok()?;
        frame.read_to_end(&mut out).ok()?;
    }
    Some(out)
}

/// A generator of reproducible bytes.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

/// Data of several kinds of redundancy: runs, repeats at short and long distances, text-like
/// symbols and noise.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    let mut out: Vec<u8> = Vec::with_capacity(len);
    while out.len() < len {
        let n = (rng.next() % 600) as usize + 1;
        match rng.next() % 6 {
            0 => out.extend(core::iter::repeat_n(rng.next() as u8, n)),
            1 if !out.is_empty() => {
                let offset = (rng.next() as usize % out.len().min(70_000)) + 1;
                for _ in 0..n {
                    out.push(out[out.len() - offset]);
                }
            }
            2 if !out.is_empty() => {
                let offset = (rng.next() as usize % out.len().min(9)) + 1;
                for _ in 0..n {
                    out.push(out[out.len() - offset]);
                }
            }
            3 => out.extend((0..n).map(|_| b"etaoin shrdlu"[(rng.next() % 13) as usize])),
            4 => out.extend((0..n).map(|_| (rng.next() % 7) as u8 * 31)),
            _ => out.extend((0..n).map(|_| rng.next() as u8)),
        }
    }
    out.truncate(len);
    out
}

#[test]
fn decodes_the_corpus_as_the_streaming_decoder_does() {
    let mut decoder = FrameDecoder::new();
    let mut files = 0;
    for entry in std::fs::read_dir("./decodecorpus_files").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "zst") {
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let expected = streamed(&data).unwrap_or_else(|| panic!("{:?}", path));
        // Exactly the size, and with room to spare.
        for &spare in &[0, 1, 40] {
            let mut out = vec![0xAA; expected.len() + spare];
            let n = decoder
                .decode_all(&data, &mut out)
                .unwrap_or_else(|e| panic!("{:?}: {:?}", path, e));
            assert_eq!(n, expected.len(), "{:?}", path);
            assert!(out[..n] == expected[..], "{:?}", path);
        }
        if !expected.is_empty() {
            let mut out = vec![0; expected.len() - 1];
            assert!(decoder.decode_all(&data, &mut out).is_err(), "{:?}", path);
        }
        files += 1;
    }
    assert!(files > 50);
}

#[test]
fn decodes_frames_of_every_encoder() {
    use crate::encoding::{compress_to_vec, CompressionLevel};
    let mut decoder = FrameDecoder::new();
    for (i, len) in [
        0usize, 1, 7, 8, 9, 100, 1000, 70_000, 131_072, 131_073, 400_000, 1_100_000,
    ]
    .iter()
    .copied()
    .enumerate()
    {
        let data = sample(len, 77 + i as u64);
        let mut frames: Vec<Vec<u8>> = [
            CompressionLevel::Uncompressed,
            CompressionLevel::Fastest,
            CompressionLevel::Default,
            CompressionLevel::Better,
            CompressionLevel::Best,
        ]
        .iter()
        .map(|&level| compress_to_vec(&data[..], level))
        .collect();
        for &level in &[-5, 1, 3, 9, 19] {
            frames.push(zstd::bulk::compress(&data, level).unwrap());
            // A stream encoder's frames record no size and use a window.
            frames.push(zstd::stream::encode_all(&data[..], level).unwrap());
        }
        for (f, frame) in frames.iter().enumerate() {
            let mut out = vec![0x55; len + 16];
            let n = decoder
                .decode_all(frame, &mut out)
                .unwrap_or_else(|e| panic!("len {} frame {}: {:?}", len, f, e));
            assert_eq!(n, len, "len {} frame {}", len, f);
            assert!(out[..n] == data[..], "len {} frame {}", len, f);
        }
        // Frames one after another, each its own window.
        let both = [&frames[1][..], &frames[6][..], &frames[3][..]].concat();
        let mut out = vec![0; len * 3];
        assert_eq!(decoder.decode_all(&both, &mut out).unwrap(), len * 3);
        for part in out.chunks(len.max(1)).take(if len > 0 { 3 } else { 0 }) {
            assert!(part == &data[..]);
        }
    }
}

/// Damaged frames are errors or decode as the streaming decoder decodes them, without a panic.
#[test]
fn damaged_frames_do_not_panic() {
    let mut decoder = FrameDecoder::new();
    let mut rng = Lcg(2024);
    let data = sample(300_000, 5);
    let frames = [
        zstd::bulk::compress(&data, 3).unwrap(),
        zstd::bulk::compress(&data, 19).unwrap(),
        crate::encoding::compress_to_vec(&data[..], crate::encoding::CompressionLevel::Better),
        zstd::bulk::compress(&data[..3000], 3).unwrap(),
    ];
    let mut out = vec![0; data.len() + 64];
    let mut agreed = 0;
    for frame in &frames {
        for round in 0..1500 {
            let mut damaged = frame.clone();
            match round % 3 {
                0 => damaged.truncate(rng.next() as usize % frame.len()),
                1 => {
                    let at = rng.next() as usize % frame.len();
                    damaged[at] ^= 1 << (rng.next() % 8);
                }
                _ => {
                    for _ in 0..4 {
                        let at = rng.next() as usize % frame.len();
                        damaged[at] = rng.next() as u8;
                    }
                }
            }
            if let Ok(n) = decoder.decode_all(&damaged, &mut out) {
                // What decodes at all decodes to the same bytes either way.
                if let Some(expected) = streamed(&damaged) {
                    assert!(out[..n] == expected[..], "round {}", round);
                    agreed += 1;
                }
            }
        }
    }
    assert!(agreed > 0);
}

/// Frames that end with a checksum: the checksum is that of the content, the decoder reports
/// both, and content that does not have the checksum of its frame is an error.
#[cfg(feature = "hash")]
#[test]
fn checks_the_checksum_of_a_frame() {
    use crate::decoding::errors::FrameDecoderError;
    let data = sample(300_000, 3);
    // The frames of the stream compressor end with a checksum (with the `hash` feature)
    let frame =
        crate::encoding::compress_to_vec(&data[..], crate::encoding::CompressionLevel::Default);
    let mut decoder = FrameDecoder::new();
    let mut out = vec![0; data.len()];
    assert_eq!(decoder.decode_all(&frame, &mut out).unwrap(), data.len());
    assert!(out == data);
    let from_data = decoder.get_checksum_from_data().unwrap();
    assert_eq!(from_data.to_le_bytes(), frame[frame.len() - 4..]);
    assert_eq!(decoder.get_calculated_checksum(), Some(from_data));

    let mut wrong = frame.clone();
    let last = wrong.len() - 1;
    wrong[last] ^= 0x40;
    assert!(matches!(
        decoder.decode_all(&wrong, &mut out),
        Err(FrameDecoderError::ChecksumMismatch { .. })
    ));
    // Without its last byte the frame has no whole checksum
    assert!(decoder.decode_all(&frame[..last], &mut out).is_err());
}
