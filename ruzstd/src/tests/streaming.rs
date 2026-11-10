//! The streaming decoder (`StreamingDecoder`, `FrameDecoder::decode_blocks` and what collects
//! from it) against the data the frames were made from and against the zstd C library: frames
//! larger than their window, read in pieces of every size, with dictionaries, and damaged.
#![cfg(test)]

use super::direct::{sample, Lcg};
use crate::decoding::{BlockDecodingStrategy, Dictionary, FrameDecoder, StreamingDecoder};
use crate::io::Read;
use alloc::vec;
use alloc::vec::Vec;
use std::io::Write as _;

/// A frame of the C library's stream compressor with a window of `1 << window_log` bytes: it
/// states no content size, and ends with a checksum if asked to.
fn windowed_frame(data: &[u8], level: i32, window_log: u32, checksum: bool) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
    encoder.window_log(window_log).unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// As [`windowed_frame`], with a dictionary.
fn windowed_dict_frame(data: &[u8], level: i32, window_log: u32, dict: &[u8]) -> Vec<u8> {
    let mut encoder =
        zstd::stream::write::Encoder::with_dictionary(Vec::new(), level, dict).unwrap();
    encoder.window_log(window_log).unwrap();
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Reads the frame `piece` bytes at a time through `decoder`, which was initialized for it.
fn read_in_pieces<R: Read>(
    mut decoder: StreamingDecoder<R, &mut FrameDecoder>,
    piece: usize,
) -> Result<Vec<u8>, crate::io::Error> {
    let mut out = Vec::new();
    let mut buf = vec![0; piece];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[cfg(feature = "hash")]
fn assert_checksum(decoder: &FrameDecoder, expected: bool) {
    let from_data = decoder.get_checksum_from_data();
    assert_eq!(from_data.is_some(), expected);
    if expected {
        assert_eq!(from_data, decoder.get_calculated_checksum());
    }
}

#[cfg(not(feature = "hash"))]
fn assert_checksum(decoder: &FrameDecoder, expected: bool) {
    assert_eq!(decoder.get_checksum_from_data().is_some(), expected);
}

/// The frames the tests below read: windows from 1 KiB (the smallest) up, so that the window
/// turns around many times, every kind of block, and frames that state their size.
fn frames(data: &[u8]) -> Vec<(Vec<u8>, bool)> {
    use crate::encoding::{compress_to_vec, CompressionLevel};
    vec![
        (windowed_frame(data, 1, 10, true), true),
        (windowed_frame(data, 3, 13, false), false),
        (windowed_frame(data, 19, 17, true), true),
        (windowed_frame(data, 3, 21, false), false),
        (zstd::bulk::compress(data, 3).unwrap(), false),
        (
            compress_to_vec(data, CompressionLevel::Fastest),
            cfg!(feature = "hash"),
        ),
        (
            compress_to_vec(data, CompressionLevel::Uncompressed),
            cfg!(feature = "hash"),
        ),
    ]
}

#[test]
fn reads_in_pieces_of_every_size() {
    let data = sample(700_000, 21);
    let mut decoder = FrameDecoder::new();
    for (f, (frame, checksum)) in frames(&data).iter().enumerate() {
        for &piece in &[1, 3, 100, 4096, 70_000, 1 << 20] {
            if piece == 1 && f > 1 {
                continue;
            }
            let mut source = &frame[..];
            let stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
            let out = read_in_pieces(stream, piece).unwrap();
            assert!(out == data, "frame {} piece {}", f, piece);
            assert!(source.is_empty(), "frame {} piece {}", f, piece);
            assert!(decoder.is_finished());
            assert_eq!(decoder.can_collect(), 0);
            assert_checksum(&decoder, *checksum);
        }
    }
}

#[test]
fn reads_to_the_end() {
    let data = sample(700_000, 22);
    let mut decoder = FrameDecoder::new();
    for (f, (frame, checksum)) in frames(&data).iter().enumerate() {
        // All at once, after what the vector holds
        let mut source = &frame[..];
        let mut stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
        let mut out = b"before".to_vec();
        stream.read_to_end(&mut out).unwrap();
        assert_eq!(&out[..6], b"before");
        assert!(out[6..] == data[..], "frame {}", f);
        // Nothing is left
        assert_eq!(stream.read(&mut [0; 8]).unwrap(), 0);
        stream.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 6 + data.len());
        assert!(source.is_empty());
        assert_checksum(&decoder, *checksum);

        // The rest, after a part was read: a few bytes, most of a window, most of the frame
        for &first in &[1usize, 5000, 300_000, 699_999, 700_000] {
            let mut source = &frame[..];
            let mut stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
            let mut out = vec![0; first];
            stream.read_exact(&mut out).unwrap();
            stream.read_to_end(&mut out).unwrap();
            assert!(out == data, "frame {} after {}", f, first);
            assert_checksum(&decoder, *checksum);
        }
    }

    // A vector with the capacity for a frame that states its size keeps its allocation
    let frame = zstd::bulk::compress(&data, 3).unwrap();
    let mut out = Vec::with_capacity(3 + data.len());
    out.extend_from_slice(b"abc");
    let at = out.as_ptr();
    let mut source = &frame[..];
    let mut stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
    stream.read_to_end(&mut out).unwrap();
    assert!(out[3..] == data[..]);
    assert_eq!(out.as_ptr(), at);
    assert_eq!(out.capacity(), 3 + data.len());
}

/// `decode_blocks` with every strategy, collected in every way, by a reader that keeps up with
/// the decoder and by ones that let the buffer grow.
#[test]
fn decodes_blocks_and_collects() {
    let data = sample(900_000, 23);
    let mut decoder = FrameDecoder::new();
    let mut rounds = 0;
    for (frame, _) in frames(&data) {
        for strategy in 0..4 {
            for collector in 0..3 {
                let mut source = &frame[..];
                decoder.reset(&mut source).unwrap();
                let mut out: Vec<u8> = Vec::new();
                let mut buf = vec![0; 50_000];
                while !decoder.is_finished() {
                    let strategy = match strategy {
                        0 => BlockDecodingStrategy::UptoBlocks(1),
                        1 => BlockDecodingStrategy::UptoBlocks(3),
                        2 => BlockDecodingStrategy::UptoBytes(300_000),
                        _ => BlockDecodingStrategy::All,
                    };
                    decoder.decode_blocks(&mut source, strategy).unwrap();
                    match collector {
                        0 => {
                            if let Some(bytes) = decoder.collect() {
                                out.extend_from_slice(&bytes);
                            }
                        }
                        1 => {
                            decoder.collect_to_writer(&mut out).unwrap();
                        }
                        // One read for all that was decoded: the buffer grows
                        _ => {
                            let n = decoder.read(&mut buf).unwrap();
                            out.extend_from_slice(&buf[..n]);
                        }
                    }
                }
                while decoder.can_collect() > 0 {
                    let n = decoder.read(&mut buf).unwrap();
                    out.extend_from_slice(&buf[..n]);
                }
                assert!(out == data, "strategy {} collector {}", strategy, collector);
                assert!(source.is_empty());
                rounds += 1;
            }
        }
    }
    assert_eq!(rounds, 7 * 12);
}

/// `decode_from_to`, from sources that hold a part of the frame into targets of every size.
#[test]
fn decodes_from_slices_to_slices() {
    let data = sample(700_000, 24);
    for (f, (frame, _)) in frames(&data).iter().enumerate() {
        for &(source_len, target_len) in &[
            (usize::MAX, 1 << 20),
            (140_000, 100),
            (140_000, 70_000),
            (300_000, 200_000),
        ] {
            let mut decoder = FrameDecoder::new();
            let mut out: Vec<u8> = Vec::new();
            let mut target = vec![0; target_len];
            let mut pos = 0usize;
            loop {
                let end = frame.len().min(pos.saturating_add(source_len));
                let (read, written) = decoder
                    .decode_from_to(&frame[pos..end], &mut target)
                    .unwrap();
                pos += read;
                out.extend_from_slice(&target[..written]);
                if read == 0 && written == 0 {
                    break;
                }
            }
            assert!(out == data, "frame {} target {}", f, target_len);
            assert_eq!(pos, frame.len());
        }
    }
}

/// Samples that share phrases, to make a dictionary from.
fn dictionary_samples() -> Vec<Vec<u8>> {
    let mut rng = Lcg(99);
    let words: Vec<Vec<u8>> = (0..300)
        .map(|_| {
            (0..3 + rng.next() % 9)
                .map(|_| b'a' + (rng.next() % 26) as u8)
                .collect()
        })
        .collect();
    (0..400)
        .map(|_| {
            let mut sample = Vec::new();
            while sample.len() < 1500 {
                sample.extend_from_slice(&words[(rng.next() % 300) as usize]);
                sample.push(b' ');
            }
            sample
        })
        .collect()
}

/// Content that repeats that of the dictionary from its start on, and is longer than the window.
fn dictionary_data(samples: &[Vec<u8>], seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    let mut data = Vec::new();
    while data.len() < 400_000 {
        data.extend_from_slice(&samples[(rng.next() % 400) as usize][..700]);
        data.extend_from_slice(&sample(300, u64::from(rng.next())));
    }
    data
}

#[test]
fn decodes_frames_with_a_dictionary() {
    let samples = dictionary_samples();
    let trained = zstd::dict::from_samples(&samples, 16_000).unwrap();
    let dict_id = Dictionary::decode_dict(&trained).unwrap().id;
    let data = dictionary_data(&samples, 7);
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict(Dictionary::decode_dict(&trained).unwrap())
        .unwrap();
    // A dictionary of content only, which frames do not name
    let raw: Vec<u8> = samples[..10].concat();
    decoder
        .add_dict(Dictionary::from_raw_content(77, raw.clone()))
        .unwrap();

    let mut frames = 0;
    for &level in &[1, 3, 19] {
        for &window_log in &[10, 14, 20] {
            for (dict, forced) in [(&trained, None), (&raw, Some(77))] {
                let frame = windowed_dict_frame(&data, level, window_log, dict);
                let expected = zstd::bulk::Decompressor::with_dictionary(dict)
                    .unwrap()
                    .decompress(&frame, data.len())
                    .unwrap();
                assert!(expected == data);

                for &piece in &[7, 3000, 1 << 20] {
                    let mut source = &frame[..];
                    let stream =
                        StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
                    if let Some(id) = forced {
                        stream.decoder.force_dict(id).unwrap();
                    }
                    let out = read_in_pieces(stream, piece).unwrap();
                    assert!(out == data, "level {} window {}", level, window_log);
                }

                let mut source = &frame[..];
                let mut stream =
                    StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
                if let Some(id) = forced {
                    stream.decoder.force_dict(id).unwrap();
                }
                let mut out = Vec::new();
                stream.read_to_end(&mut out).unwrap();
                assert!(out == data, "level {} window {}", level, window_log);

                if forced.is_some() {
                    // The frame does use the dictionary: without, it is not the data
                    let mut source = &frame[..];
                    let stream =
                        StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
                    assert!(read_in_pieces(stream, 3000).map_or(true, |out| out != data));
                } else {
                    // `decode_all` finds the dictionary a frame names
                    let mut out = vec![0; data.len()];
                    assert_eq!(decoder.decode_all(&frame, &mut out).unwrap(), data.len());
                    assert!(out == data);
                    // Without the dictionary the frame is an error
                    let mut source = &frame[..];
                    assert!(FrameDecoder::new().reset(&mut source).is_err());
                }
                frames += 1;
            }
        }
    }
    assert_eq!(frames, 18);
    assert_ne!(dict_id, 0);
}

/// A frame of one block of one sequence: no literals, a match of three bytes at the offset of
/// `offset_code` with `offset_bit` as its extra bit.
fn one_match_frame(offset_code: u8, offset_bit: Option<bool>) -> Vec<u8> {
    let stream = match offset_bit {
        None => 0b1,
        Some(bit) => 0b10 | u8::from(bit),
    };
    let block = [
        0x00,        // no literals
        0x01,        // one sequence
        0x54,        // each table is a single code
        0x00,        // literal length 0
        offset_code, // the offset
        0x00,        // match length 3
        stream,
    ];
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 16];
    // The last block, compressed, of that many bytes
    frame.extend_from_slice(&[1 | (2 << 1) | ((block.len() as u8) << 3), 0, 0]);
    frame.extend_from_slice(&block);
    frame
}

/// With no literals before it, a match at offset value 1 is at the second of the last offsets
/// and one at offset value 3 at the first less one: what a dictionary sets them to.
#[test]
fn repeated_offsets_of_a_dictionary() {
    let read = |frame: &[u8], offsets: [u32; 3]| -> Result<Vec<u8>, crate::io::Error> {
        let mut dict = Dictionary::from_raw_content(5, b"0123456789".to_vec());
        dict.offset_hist = offsets;
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict).unwrap();
        let mut source = frame;
        let stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
        stream.decoder.force_dict(5).unwrap();
        read_in_pieces(stream, 100)
    };
    // The second offset: four back, in the dictionary
    assert_eq!(read(&one_match_frame(0, None), [1, 4, 8]).unwrap(), b"678");
    // The first less one: two back, and the match runs on into its own bytes
    assert_eq!(
        read(&one_match_frame(1, Some(true)), [3, 4, 8]).unwrap(),
        b"898"
    );
    // The first less one is no offset, also for an offset of 0 from a damaged dictionary
    // (https://github.com/KillingSpark/zstd-rs/issues/115)
    assert!(read(&one_match_frame(1, Some(true)), [1, 4, 8]).is_err());
    assert!(read(&one_match_frame(1, Some(true)), [0, 4, 8]).is_err());
    // Further back than the dictionary reaches
    assert!(read(&one_match_frame(0, None), [1, 11, 8]).is_err());
    assert_eq!(read(&one_match_frame(0, None), [1, 10, 8]).unwrap(), b"012");
}

/// What the C library makes of a frame, `None` when it fails.
fn reference(frame: &[u8], dict: Option<&[u8]>, limit: usize) -> Option<Vec<u8>> {
    let mut decompressor = match dict {
        Some(dict) => zstd::bulk::Decompressor::with_dictionary(dict).unwrap(),
        None => zstd::bulk::Decompressor::new().unwrap(),
    };
    decompressor.decompress(frame, limit).ok()
}

/// Damaged frames are errors, or decode to the same bytes however they are read and to those
/// the C library decodes them to: never a panic.
#[test]
fn damaged_frames_do_not_panic() {
    let samples = dictionary_samples();
    let trained = zstd::dict::from_samples(&samples, 16_000).unwrap();
    let with_dict = dictionary_data(&samples, 8);
    let data = sample(200_000, 25);
    let frames: [(Vec<u8>, Option<&[u8]>); 5] = [
        (windowed_frame(&data, 3, 10, false), None),
        (windowed_frame(&data, 19, 14, false), None),
        (zstd::bulk::compress(&data, 3).unwrap(), None),
        (
            windowed_dict_frame(&with_dict[..150_000], 3, 10, &trained),
            Some(&trained[..]),
        ),
        (
            windowed_dict_frame(&with_dict[..150_000], 19, 17, &trained),
            Some(&trained[..]),
        ),
    ];
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict(Dictionary::decode_dict(&trained).unwrap())
        .unwrap();
    let mut rng = Lcg(4711);
    let (mut decoded, mut agreed, mut failed) = (0, 0, 0);
    for (frame, dict) in &frames {
        for round in 0..1200 {
            let mut damaged = frame.clone();
            match round % 3 {
                0 => damaged.truncate(rng.next() as usize % frame.len()),
                1 => {
                    let at = rng.next() as usize % frame.len();
                    damaged[at] ^= 1 << (rng.next() % 8);
                }
                _ => {
                    for _ in 0..3 {
                        let at = rng.next() as usize % frame.len();
                        damaged[at] = rng.next() as u8;
                    }
                }
            }
            let piece = 1 + rng.next() as usize % 40_000;
            let mut source = &damaged[..];
            let pieces = StreamingDecoder::new_with_decoder(&mut source, &mut decoder)
                .ok()
                .and_then(|stream| read_in_pieces(stream, piece).ok());
            let mut source = &damaged[..];
            let at_once = StreamingDecoder::new_with_decoder(&mut source, &mut decoder)
                .ok()
                .and_then(|mut stream| {
                    let mut out = Vec::new();
                    stream.read_to_end(&mut out).ok().map(|_| out)
                });
            match (&pieces, &at_once) {
                (Some(pieces), Some(at_once)) => {
                    assert!(pieces == at_once, "round {}", round);
                    decoded += 1;
                    // The reference checks more (the content size a frame states, the reach
                    // of its window): what it accepts too, it decodes to the same bytes
                    if let Some(expected) = reference(&damaged, *dict, 2 * data.len()) {
                        assert!(*pieces == expected, "round {}", round);
                        agreed += 1;
                    }
                }
                (None, None) => failed += 1,
                // Read at once, a frame may not decode to more than it states, and a match
                // may reach further back than the window, as in `decode_all`
                (Some(_), None) | (None, Some(_)) => {}
            }
        }
    }
    assert!(decoded > 100 && agreed > 100 && failed > 1000);
}

/// A frame with more content than its header states: read in pieces it decodes as it always
/// did, read at once into a vector it decodes or is an error, and never writes past the vector.
#[test]
fn more_content_than_stated() {
    let data = sample(500_000, 26);
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(16).unwrap();
    encoder
        .set_pledged_src_size(Some(data.len() as u64))
        .unwrap();
    encoder.write_all(&data).unwrap();
    let frame = encoder.finish().unwrap();
    // The frame header: no dictionary, a window descriptor and a content size of four bytes
    assert_eq!(frame[4], 0x80);
    assert_eq!(frame[6..10], (data.len() as u32).to_le_bytes());

    let mut decoder = FrameDecoder::new();
    for &stated in &[0u32, 1, 70_000, 131_072, 300_000, 499_999, 500_001, 900_000] {
        let mut lying = frame.clone();
        lying[6..10].copy_from_slice(&stated.to_le_bytes());
        let mut source = &lying[..];
        let stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
        assert!(read_in_pieces(stream, 10_000).unwrap() == data);

        for &capacity in &[0, 100, stated as usize, 600_000] {
            let mut source = &lying[..];
            let mut stream = StreamingDecoder::new_with_decoder(&mut source, &mut decoder).unwrap();
            let mut out = Vec::with_capacity(capacity);
            out.push(b'!');
            match stream.read_to_end(&mut out) {
                Ok(_) => assert!(out[1..] == data[..], "stated {}", stated),
                Err(_) => {
                    assert!(stated < 500_000, "stated {}", stated);
                    // What was decoded before the error is there
                    assert!(out[1..] == data[..out.len() - 1]);
                }
            }
            assert_eq!(out[0], b'!');
        }
    }
}
