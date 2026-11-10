#![no_main]
#[macro_use]
extern crate libfuzzer_sys;
extern crate ruzstd;
use std::io::Read;

fuzz_target!(|data: &[u8]| {
    // In pieces, through the window of the decoder
    let mut in_pieces = None;
    if let Ok(mut decoder) = ruzstd::decoding::StreamingDecoder::new(data) {
        let mut output = Vec::new();
        let mut piece = vec![0; 1 + data.len() * 31 % 70_000];
        in_pieces = loop {
            match decoder.read(&mut piece) {
                Ok(0) => break Some(output),
                Ok(n) => output.extend_from_slice(&piece[..n]),
                Err(_) => break None,
            }
        };
    }
    // At once, straight into the vector: what decodes both ways decodes to the same bytes
    if let Ok(mut decoder) = ruzstd::decoding::StreamingDecoder::new(data) {
        let mut output = Vec::new();
        if decoder.read_to_end(&mut output).is_ok() {
            if let Some(in_pieces) = in_pieces {
                assert!(in_pieces == output);
            }
        }
    }
});
