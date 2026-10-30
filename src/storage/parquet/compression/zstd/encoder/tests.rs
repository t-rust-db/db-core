//! The encoder (#692) against two decoders: ours and the `zstd` CLI's
//! (libzstd).

use super::*;
use crate::storage::parquet::compression::zstd::decompress;
use crate::test_rng::Rng;
use std::io::Write;

fn cli_decode(frame: &[u8]) -> Vec<u8> {
    let mut child = std::process::Command::new("zstd")
        .args(["-q", "-d", "-c"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("zstd CLI must be available for this test");
    let mut stdin = child.stdin.take().unwrap();
    let input = frame.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success(), "libzstd refused the frame");
    output.stdout
}

/// `data` must survive both decoders.
fn check(data: &[u8], what: &str) {
    let frame = compress(data);
    assert_eq!(
        decompress(&frame, data.len()).unwrap(),
        data,
        "{what}: ours"
    );
    assert_eq!(cli_decode(&frame), data, "{what}: libzstd");
}

fn text(rng: &mut Rng, len: usize) -> Vec<u8> {
    const WORDS: [&str; 12] = [
        "order",
        "shipped",
        "customer",
        "warehouse",
        "delayed",
        "refund",
        "the",
        "a",
        "of",
        "pallet",
        "invoice",
        "error",
    ];
    let mut out = Vec::new();
    while out.len() < len {
        out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
        out.push(b' ');
        if rng.below(20) == 0 {
            out.extend_from_slice(rng.below(100_000).to_string().as_bytes());
        }
    }
    out.truncate(len);
    out
}

#[test]
fn empty_tiny_and_repeated_inputs_round_trip() {
    check(&[], "empty");
    check(&[7], "one byte");
    check(&[1, 2, 3], "three bytes");
    check(&[9; 5], "short run");
    check(&vec![0; 300_000], "zeros spanning blocks");
    check(&b"ab".repeat(70_000), "two-byte period");
}

#[test]
fn random_pages_round_trip() {
    let mut rng = Rng::new(692);
    for size in [
        0usize, 1, 7, 8, 9, 31, 32, 33, 255, 256, 257, 4096, 65_791, 65_792, 131_071, 131_072,
        131_073, 262_144, 400_000,
    ] {
        let noise: Vec<u8> = (0..size).map(|_| rng.below(256) as u8).collect();
        check(&noise, &format!("incompressible {size}"));
        check(&text(&mut rng, size), &format!("text {size}"));
        let ints: Vec<u8> = (0..size / 4)
            .flat_map(|i| (i as u32 / 3 + rng.below(4) as u32).to_le_bytes())
            .collect();
        check(&ints, &format!("integers {size}"));
        let mixed: Vec<u8> = (0..size)
            .map(|i| {
                if rng.below(4) == 0 {
                    rng.below(256) as u8
                } else {
                    (i % 37) as u8
                }
            })
            .collect();
        check(&mixed, &format!("mixed {size}"));
    }
}

#[test]
fn incompressible_input_barely_grows() {
    let mut rng = Rng::new(1);
    let noise: Vec<u8> = (0..300_000).map(|_| rng.below(256) as u8).collect();
    let frame = compress(&noise);
    // Header plus three bytes per raw block.
    assert!(frame.len() <= noise.len() + 32, "{}", frame.len());
}

#[test]
fn repetitive_text_shrinks() {
    let mut rng = Rng::new(2);
    let data = text(&mut rng, 200_000);
    assert!(compress(&data).len() < data.len() / 3);
}

/// Seeded fuzzing (#692, in the style of `decoder_fuzz.rs`): structured
/// pages (repeated chunks with edits, a skewed alphabet, runs) of every
/// size up to a few blocks. The output is always a valid frame that
/// decodes to the input.
#[test]
fn seeded_fuzz_always_yields_a_frame_that_decodes_to_the_input() {
    let mut rng = Rng::new(0xF022);
    for round in 0..400 {
        let size = match rng.below(4) {
            0 => rng.below(64) as usize,
            1 => rng.below(2_000) as usize,
            2 => rng.below(40_000) as usize,
            _ => rng.below(300_000) as usize,
        };
        let alphabet = 1 + rng.below(256) as usize;
        let skew = 1 + rng.below(6);
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            match rng.below(4) {
                0 if data.len() > 8 => {
                    // Copy an earlier stretch, as a match would.
                    let from = rng.below(data.len() as u64 - 4) as usize;
                    let len = 1 + rng.below(300) as usize;
                    for i in 0..len {
                        let b = data[from + i % (data.len() - from)];
                        data.push(b);
                    }
                }
                1 => {
                    let b = rng.below(alphabet as u64) as u8;
                    data.extend(std::iter::repeat_n(b, 1 + rng.below(500) as usize));
                }
                _ => {
                    for _ in 0..1 + rng.below(40) {
                        let v = rng.below(alphabet as u64);
                        data.push((v >> rng.below(skew)) as u8);
                    }
                }
            }
        }
        data.truncate(size);
        let frame = compress(&data);
        assert_eq!(
            decompress(&frame, data.len()).unwrap(),
            data,
            "round {round}, size {size}, alphabet {alphabet}"
        );
    }
}

/// Many small random pages of every shape: the property that any input
/// round-trips, over a wider spread of sizes than the fuzz above.
#[test]
fn any_small_page_round_trips() {
    let mut rng = Rng::new(0x5EED);
    for _ in 0..2_000 {
        let size = rng.below(3_000) as usize;
        let chunks: Vec<Vec<u8>> = (0..1 + rng.below(7))
            .map(|_| {
                (0..1 + rng.below(39))
                    .map(|_| rng.below(256) as u8)
                    .collect()
            })
            .collect();
        let data: Vec<u8> = if rng.below(2) == 0 {
            (0..size).map(|_| rng.below(256) as u8).collect()
        } else {
            let mut d = Vec::new();
            while d.len() < size {
                d.extend_from_slice(&chunks[rng.below(chunks.len() as u64) as usize]);
            }
            d.truncate(size);
            d
        };
        let frame = compress(&data);
        assert_eq!(decompress(&frame, data.len()).unwrap(), data);
    }
}
