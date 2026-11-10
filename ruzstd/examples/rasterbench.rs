//! Times zstd-rust against the zstd C library on chunks of raw data, such as the tiles of rasters.
//!
//! `cargo run --release --example rasterbench -- <directory> [dec|enc|all] [filter]`
//!
//! The directory holds `name.raw` files of chunks one after another and, for each, a `name.len` file with
//! the length of a chunk in decimal (`name_p2.raw` and `name_p3.raw` share the `.len` of `name`). Each chunk
//! is a frame. `RUNS` sets the number of timed runs (the best counts), `LEVEL` restricts the levels to one,
//! `NOREUSE` compresses with `compress_to_vec` instead of a `Compressor`.
use std::io::Read;
use std::time::Instant;
use zstd_rust::decoding::{FrameDecoder, StreamingDecoder};
use zstd_rust::encoding::{compress_to_vec, CompressionLevel, Compressor};

fn best<T>(n: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..n {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed().as_secs_f64());
        out = Some(r);
    }
    (best, out.unwrap())
}

fn level(l: i32) -> CompressionLevel {
    match l {
        0 | 3..=5 => CompressionLevel::Default,
        ..=2 => CompressionLevel::Fastest,
        6..=9 => CompressionLevel::Better,
        _ => CompressionLevel::Best,
    }
}

/// A sampling profiler for this process (Windows): `PROFILE=file` writes the instruction addresses of the
/// main thread, relative to the module, sampled a few thousand times a second.
#[cfg(windows)]
mod sampler {
    use std::convert::TryInto;
    use std::io::Write;
    #[repr(C, align(16))]
    struct Context([u8; 0x4D0]);
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenThread(access: u32, inherit: i32, id: u32) -> isize;
        fn SuspendThread(thread: isize) -> u32;
        fn ResumeThread(thread: isize) -> u32;
        fn GetThreadContext(thread: isize, context: *mut Context) -> i32;
        fn GetModuleHandleW(name: *const u16) -> isize;
    }
    pub fn start() {
        let Ok(path) = std::env::var("PROFILE") else {
            return;
        };
        let id = unsafe { GetCurrentThreadId() };
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
        std::thread::spawn(move || {
            let thread = unsafe { OpenThread(0x0002 | 0x0008 | 0x0040, 0, id) };
            let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
            let mut context = Context([0; 0x4D0]);
            loop {
                let until = std::time::Instant::now() + std::time::Duration::from_micros(250);
                while std::time::Instant::now() < until {
                    std::hint::spin_loop();
                }
                unsafe {
                    context.0[0x30..0x34].copy_from_slice(&0x0010_0001u32.to_le_bytes());
                    SuspendThread(thread);
                    let ok = GetThreadContext(thread, &mut context);
                    ResumeThread(thread);
                    if ok != 0 {
                        let rip = u64::from_le_bytes(context.0[0xF8..0x100].try_into().unwrap());
                        writeln!(out, "{:x}", rip.wrapping_sub(base)).unwrap();
                    }
                }
            }
        });
    }
}

fn main() {
    #[cfg(windows)]
    sampler::start();
    let args: Vec<String> = std::env::args().collect();
    let dir = &args[1];
    let mode = args.get(2).map(String::as_str).unwrap_or("all");
    let filter = args.get(3).cloned().unwrap_or_default();
    let runs: usize = std::env::var("RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.ends_with(".raw") && n.contains(&filter))
        .collect();
    names.sort();
    let only: Option<i32> = std::env::var("LEVEL").ok().and_then(|v| v.parse().ok());
    for name in names {
        let raw = std::fs::read(format!("{dir}/{name}")).unwrap();
        let stem = name
            .trim_end_matches(".raw")
            .trim_end_matches("_p2")
            .trim_end_matches("_p3");
        let len: usize = std::fs::read_to_string(format!("{dir}/{stem}.len"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let chunks: Vec<&[u8]> = raw.chunks(len).collect();
        let mb = raw.len() as f64 / 1e6;
        for l in [1, 3, 9, 19] {
            if only.is_some_and(|o| o != l) {
                continue;
            }
            let (tc, c): (f64, Vec<Vec<u8>>) = best(if l >= 19 { 1 } else { runs }, || {
                chunks
                    .iter()
                    .map(|c| zstd::bulk::compress(c, l).unwrap())
                    .collect()
            });
            let csize: usize = c.iter().map(Vec::len).sum();
            if mode != "dec" {
                let mut compressor = Compressor::new(level(l));
                let reuse = std::env::var("NOREUSE").is_err();
                let (tr, r): (f64, Vec<Vec<u8>>) = best(if l >= 19 { 1 } else { runs }, || {
                    chunks
                        .iter()
                        .map(|c| {
                            if reuse {
                                compressor.compress(c)
                            } else {
                                compress_to_vec(*c, level(l))
                            }
                        })
                        .collect()
                });
                let rsize: usize = r.iter().map(Vec::len).sum();
                for (a, b) in r.iter().zip(&chunks) {
                    assert_eq!(&zstd::bulk::decompress(a, len).unwrap()[..], *b);
                }
                println!(
                    "ENC {name:18} L{l:<2} C {:7.1} MB/s ratio {:.3} | rust {:7.1} MB/s ratio {:.3} | time x{:.2} size x{:.3}",
                    mb / tc,
                    raw.len() as f64 / csize as f64,
                    mb / tr,
                    raw.len() as f64 / rsize as f64,
                    tr / tc,
                    rsize as f64 / csize as f64
                );
            }
            if mode != "enc" {
                let (dc, _) = best(runs, || {
                    c.iter()
                        .map(|f| zstd::bulk::decompress(f, len).unwrap().len())
                        .sum::<usize>()
                });
                let (ds, _) = best(runs, || {
                    c.iter()
                        .map(|f| {
                            let mut out = Vec::new();
                            let mut src = &f[..];
                            StreamingDecoder::new(&mut src)
                                .unwrap()
                                .take(len as u64)
                                .read_to_end(&mut out)
                                .unwrap();
                            assert_eq!(out.len(), len);
                            out.len()
                        })
                        .sum::<usize>()
                });
                let mut fd = FrameDecoder::new();
                if let Ok(seconds) = std::env::var("DECODE_ONLY") {
                    // For profiling: only the decoding of whole frames, for that many seconds
                    let until = Instant::now()
                        + std::time::Duration::from_secs_f64(seconds.parse().unwrap());
                    let mut out = vec![0u8; len];
                    while Instant::now() < until {
                        for f in &c {
                            fd.decode_all(f, &mut out).unwrap();
                        }
                    }
                }
                let (da, _) = best(runs, || {
                    c.iter()
                        .map(|f| {
                            let mut out = vec![0u8; len];
                            let n = fd.decode_all(f, &mut out).unwrap();
                            assert_eq!(n, len);
                            n
                        })
                        .sum::<usize>()
                });
                for (f, b) in c.iter().zip(&chunks) {
                    let mut out = vec![0u8; len];
                    fd.decode_all(f, &mut out).unwrap();
                    assert_eq!(&out[..], *b);
                }
                println!(
                    "DEC {name:18} L{l:<2} C {:7.1} MB/s | rust stream {:7.1} (x{:.2}) | rust all {:7.1} (x{:.2})",
                    mb / dc,
                    mb / ds,
                    ds / dc,
                    mb / da,
                    da / dc
                );
            }
        }
    }
}
