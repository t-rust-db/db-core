//! Spike for db-core#697: a zstd decoder of our own. (It was timed against
//! the `ruzstd` crate, since removed; see the README.) No production code changes: the prototype lives in
//! `prototype.rs` next to this file, and #700 shipped the decoder that grew
//! out of it (`src/storage/parquet/compression/zstd.rs`).
//!
//! `frames_decode_to_their_input` checks the prototype against frames from
//! the `zstd` CLI (libzstd) at several levels.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]

mod prototype;

use std::io::Write;

const PAGE: usize = 64 * 1024;

/// Deterministic page-like data: runs, repeats and a skewed alphabet, so
/// libzstd picks RLE, raw and Huffman/FSE-coded blocks.
fn corpus(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        match next() % 4 {
            0 => out.extend(std::iter::repeat_n((next() % 251) as u8, 8 + next() % 200)),
            1 if out.len() > 64 => {
                let from = next() % (out.len() - 32);
                let n = 4 + next() % 120;
                for i in 0..n {
                    out.push(out[from + i % 32]);
                }
            }
            _ => out.extend((0..16 + next() % 64).map(|_| b"etaoin shrdlu"[next() % 13])),
        }
    }
    out.truncate(len);
    out
}

fn cli(data: &[u8], level: &str) -> Vec<u8> {
    let mut child = std::process::Command::new("zstd")
        .args(["-q", "-c", level])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("zstd CLI must be available for this spike");
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    output.stdout
}

fn frames() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut all = Vec::new();
    for level in ["-1", "-3", "-9", "-19"] {
        for seed in 0..4 {
            let data = corpus(PAGE, seed);
            all.push((cli(&data, level), data));
        }
    }
    all
}

#[test]
fn frames_decode_to_their_input() {
    for (frame, data) in frames() {
        assert_eq!(prototype::decompress(&frame, data.len()).unwrap(), data);
    }
}
