/// Huffman coding is a method of encoding where symbols are assigned a code,
/// and more commonly used symbols get shorter codes, and less commonly
/// used symbols get longer codes. Codes are prefix free, meaning no two codes
/// will start with the same sequence of bits.
mod huff0_decoder;
pub use huff0_decoder::*;
pub mod huff0_encoder;

/// Only needed for testing.
///
/// Encodes the data with a table built from that data
/// Decodes the result again by first decoding the table and then the data
/// Asserts that the decoded data equals the input
#[cfg(any(test, feature = "fuzz_exports"))]
pub fn round_trip(data: &[u8]) {
    use crate::bit_io::BitWriter;

    if data.len() < 2 {
        return;
    }
    if data.iter().all(|x| *x == data[0]) {
        return;
    }
    let mut writer = BitWriter::new();
    let encoder_table = huff0_encoder::HuffmanTable::build_from_data(data);
    let mut encoder = huff0_encoder::HuffmanEncoder::new(&encoder_table, &mut writer);

    encoder.encode(data, true);
    let encoded = writer.dump();
    let mut decoder_table = HuffmanTable::new();
    let table_bytes = decoder_table.build_decoder(&encoded).unwrap();
    let mut decoded = alloc::vec![0; data.len()];
    decoder_table
        .decode_stream(&encoded[table_bytes as usize..], &mut decoded)
        .expect("the stream is the symbols of the data");
    assert_eq!(&decoded, data);
}

/// The four streams of a literals section, decoded side by side: a symbol and two symbols per
/// lookup, with the loops the processor allows and with those the crate is compiled with, for
/// streams of every length up to where all of the loops have run.
#[test]
fn four_streams_round_trip() {
    use crate::bit_io::BitWriter;
    use crate::common::cpu::testing::on_every_path;
    use alloc::vec::Vec;

    let mut state = 12345u32;
    let mut random = move || {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        state >> 16
    };
    // Symbols of few bits (two fit in a lookup, and enough of them make the table of pairs
    // pay) and of many.
    for (symbols, sizes) in [(4u32, 4090..4130usize), (200, 16..300), (200, 4090..4130)] {
        let longest = sizes.end;
        let all: Vec<u8> = (0..longest)
            .map(|_| (random() % symbols).min(random() % symbols) as u8)
            .collect();
        let table = huff0_encoder::HuffmanTable::build_from_data(&all);
        for size in sizes {
            let data = &all[..size];
            let mut writer = BitWriter::new();
            let mut encoder = huff0_encoder::HuffmanEncoder::new(&table, &mut writer);
            encoder.encode4x(data, true);
            let encoded = writer.dump();
            on_every_path(|| {
                let mut decoder_table = HuffmanTable::new();
                let table_bytes = decoder_table.build_decoder(&encoded).unwrap() as usize;
                let (jumps, streams) = encoded[table_bytes..].split_at(6);
                let jump = |at: usize| usize::from(u16::from_le_bytes([jumps[at], jumps[at + 1]]));
                let jumps = [jump(0), jump(0) + jump(2), jump(0) + jump(2) + jump(4)];
                let mut decoded = alloc::vec![0xAA; size];
                decoder_table
                    .decode_four_streams(streams, jumps, &mut decoded)
                    .expect("the streams are the symbols of the data");
                assert!(decoded == data, "{} symbols of {}", size, symbols);
                // One symbol fewer leaves bits of a stream unread: these are not its symbols.
                let mut short = alloc::vec![0; size - 1];
                assert!(decoder_table
                    .decode_four_streams(streams, jumps, &mut short)
                    .is_none());
            });
        }
    }
}

#[test]
fn roundtrip() {
    use alloc::vec::Vec;
    round_trip(&[1, 1, 1, 1, 2, 3]);
    round_trip(&[1, 1, 1, 1, 2, 3, 5, 45, 12, 90]);

    for size in 2..512 {
        use alloc::vec;
        let data = vec![123; size];
        round_trip(&data);
        let mut data = Vec::new();
        for x in 0..size {
            data.push(x as u8);
        }
        round_trip(&data);
    }

    #[cfg(feature = "std")]
    if std::fs::exists("fuzz/artifacts/huff0").unwrap_or(false) {
        for file in std::fs::read_dir("fuzz/artifacts/huff0").unwrap() {
            if file.as_ref().unwrap().file_type().unwrap().is_file() {
                let data = std::fs::read(file.unwrap().path()).unwrap();
                round_trip(&data);
            }
        }
    }
}
