//! The decoder (#700) against real frames from the `zstd` CLI (libzstd at
//! several levels, which between them use every literals and sequence
//! mode), and against hand-built and mutated frames for its error paths.

use super::*;
use crate::test_rng::Rng;
use std::io::Write;

/// `data` compressed by the `zstd` CLI with `args`.
fn cli(data: &[u8], args: &[&str]) -> Vec<u8> {
    let mut child = std::process::Command::new("zstd")
        .args(["-q", "-c"])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("zstd CLI must be available for this test");
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    output.stdout
}

/// Inputs that between them make libzstd use raw, RLE and compressed
/// blocks; raw, RLE, 1-stream, 4-stream and treeless literals; direct and
/// FSE-coded Huffman weights; predefined, RLE, FSE-coded and repeated
/// sequence tables; and repeat offsets.
fn inputs() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng::new(700);
    let words = [
        "request", "from", "host", "session", "login", "logout", "search", "checkout", "view",
        "user", "agent", "Mozilla", "accept", "gzip", "the", "a", "of", "and",
    ];
    let text: Vec<u8> = (0..60_000)
        .flat_map(|i| {
            let w = words[rng.below(words.len() as u64) as usize];
            format!("{w} {} ", i % 977).into_bytes()
        })
        .collect();
    let digits: Vec<u8> = (0..20_000)
        .map(|_| b"0123456789 "[rng.below(11) as usize])
        .collect();
    let ints: Vec<u8> = (0..50_000u32)
        .flat_map(|i| (i.wrapping_mul(7919) % 100_003).to_le_bytes())
        .collect();
    let random: Vec<u8> = (0..150_000).map(|_| rng.below(256) as u8).collect();
    let mixed: Vec<u8> = (0..300_000)
        .map(|i| match (i / 4096) % 4 {
            0 => 0u8,
            1 => rng.below(256) as u8,
            2 => b"abcabcabd"[i % 9],
            _ => (i % 251) as u8,
        })
        .collect();
    vec![
        ("text", text),
        ("digits", digits),
        ("ints", ints),
        ("random", random),
        ("zeros", vec![0u8; 300_000]),
        ("mixed", mixed),
        ("tiny", b"0123456789abcdef".to_vec()),
        ("one", b"x".to_vec()),
        ("empty", Vec::new()),
    ]
}

#[test]
fn every_cli_frame_decodes_to_its_input() {
    for (name, data) in inputs() {
        for args in [
            &["-1"][..],
            &["-3"],
            &["-9"],
            &["-19"],
            &["-1", "--no-check"],
            &["-19", "--no-check", "--no-content-size"],
            &["--fast=5"],
        ] {
            let frame = cli(&data, args);
            let got =
                decompress(&frame, data.len()).unwrap_or_else(|e| panic!("{name} {args:?}: {e}"));
            assert!(got == data, "{name} {args:?}: wrong bytes");
        }
    }
}

/// A frame header: magic, a single-segment descriptor with a 1-byte
/// content size (`descriptor` adds flags), then `blocks`.
fn frame(descriptor: u8, content_size: u8, blocks: &[u8]) -> Vec<u8> {
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20 | descriptor, content_size];
    f.extend_from_slice(blocks);
    f
}

/// A block header: `last`, `kind` (0 raw, 1 RLE, 2 compressed), `size`.
fn block(last: bool, kind: u32, size: u32) -> [u8; 3] {
    let h = u32::from(last) | kind << 1 | size << 3;
    [h as u8, (h >> 8) as u8, (h >> 16) as u8]
}

fn read_error(frame: &[u8], size: usize) -> String {
    match decompress(frame, size) {
        Err(ZstdError::Read(why)) => why,
        other => panic!("expected a read error, got {other:?}"),
    }
}

fn frame_error(frame: &[u8], size: usize) -> String {
    match decompress(frame, size) {
        Err(ZstdError::Frame(why)) => why,
        other => panic!("expected a frame error, got {other:?}"),
    }
}

#[test]
fn raw_and_rle_blocks_decode_as_they_are() {
    let mut blocks = block(false, 0, 3).to_vec();
    blocks.extend_from_slice(b"abc");
    blocks.extend_from_slice(&block(true, 1, 4));
    blocks.push(b'z');
    assert_eq!(decompress(&frame(0, 7, &blocks), 7).unwrap(), b"abczzzz");
    // A frame without the single-segment flag carries a window descriptor.
    let mut windowed = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x50];
    windowed.extend_from_slice(&block(true, 0, 2));
    windowed.extend_from_slice(b"hi");
    assert_eq!(decompress(&windowed, 2).unwrap(), b"hi");
}

#[test]
fn header_errors_are_frame_errors() {
    assert!(frame_error(b"\x28\xB5\x2F", 1).contains("not a zstd frame"));
    assert!(frame_error(&[0x28, 0xB5, 0x2F, 0xFD], 1).contains("short frame header"));
    assert!(frame_error(&frame(0x01, 1, &[]), 1).contains("dictionaries"));
    assert!(frame_error(&frame(0x08, 1, &[]), 1).contains("reserved"));
    // An 8-byte content size field the frame does not hold.
    assert!(frame_error(&[0x28, 0xB5, 0x2F, 0xFD, 0xC0, 0], 1).contains("short frame header"));
}

#[test]
fn block_errors_are_read_errors() {
    assert!(read_error(&frame(0, 1, &[]), 1).contains("short block header"));
    assert!(read_error(&frame(0, 1, &block(true, 3, 0)), 1).contains("reserved block type"));
    assert!(read_error(&frame(0, 4, &block(true, 0, 4)), 4).contains("short raw block"));
    assert!(read_error(&frame(0, 4, &block(true, 1, 4)), 4).contains("short RLE block"));
    assert!(read_error(&frame(0, 4, &block(true, 2, 9)), 4).contains("short compressed block"));
    // A checksum flag with no checksum after the last block.
    let mut blocks = block(true, 0, 1).to_vec();
    blocks.push(b'a');
    assert!(read_error(&frame(0x04, 1, &blocks), 1).contains("checksum"));
}

#[test]
fn sizes_that_do_not_match_are_size_mismatches() {
    let mut blocks = block(true, 1, 10).to_vec();
    blocks.push(b'a');
    assert!(matches!(
        decompress(&frame(0, 10, &blocks), 9),
        Err(ZstdError::SizeMismatch {
            expected: 9,
            actual: 10
        })
    ));
    assert!(matches!(
        decompress(&frame(0, 10, &blocks), 11),
        Err(ZstdError::SizeMismatch {
            expected: 11,
            actual: 10
        })
    ));
    // Declared far past what any frame of this length can expand to.
    assert!(read_error(&frame(0, 10, &blocks), 1 << 40).contains("expansion"));
}

/// A compressed block: raw literals `literals`, then `sequences`.
fn compressed(literals: &[u8], sequences: &[u8]) -> Vec<u8> {
    let mut body = vec![(literals.len() as u8) << 3];
    body.extend_from_slice(literals);
    body.extend_from_slice(sequences);
    let mut b = block(true, 2, body.len() as u32).to_vec();
    b.extend_from_slice(&body);
    b
}

#[test]
fn sequence_section_errors_are_read_errors() {
    // No sequences: the block is its literals.
    assert_eq!(
        decompress(&frame(0, 3, &compressed(b"abc", &[0])), 3).unwrap(),
        b"abc"
    );
    // A repeated table in the first block has nothing to repeat.
    let e = read_error(&frame(0, 3, &compressed(b"abc", &[1, 0xFC])), 3);
    assert!(e.contains("repeated sequence table"), "{e}");
    // An RLE literals-length symbol past the code table.
    let e = read_error(&frame(0, 3, &compressed(b"abc", &[1, 0x40, 200])), 3);
    assert!(e.contains("RLE symbol"), "{e}");
    // A sequences header cut short.
    let e = read_error(&frame(0, 3, &compressed(b"abc", &[0x80])), 3);
    assert!(e.contains("short sequences header"), "{e}");
    let e = read_error(&frame(0, 3, &compressed(b"abc", &[1])), 3);
    assert!(e.contains("short sequences header"), "{e}");
}

#[test]
fn repeat_offsets_follow_the_rfc() {
    let mut reps = [10, 20, 30];
    // A new offset pushes the others down.
    assert_eq!(frame::resolve_offset(&mut reps, 8, 1).unwrap(), 5);
    assert_eq!(reps, [5, 10, 20]);
    // With literals, 1 is the latest offset and changes nothing.
    assert_eq!(frame::resolve_offset(&mut reps, 1, 1).unwrap(), 5);
    assert_eq!(reps, [5, 10, 20]);
    // 2 brings the second to the front.
    assert_eq!(frame::resolve_offset(&mut reps, 2, 1).unwrap(), 10);
    assert_eq!(reps, [10, 5, 20]);
    // 3 brings the third to the front.
    assert_eq!(frame::resolve_offset(&mut reps, 3, 1).unwrap(), 20);
    assert_eq!(reps, [20, 10, 5]);
    // Without literals each shifts by one: 1 is the second, 2 the third,
    // 3 the latest minus one.
    assert_eq!(frame::resolve_offset(&mut reps, 1, 0).unwrap(), 10);
    assert_eq!(reps, [10, 20, 5]);
    assert_eq!(frame::resolve_offset(&mut reps, 2, 0).unwrap(), 5);
    assert_eq!(reps, [5, 10, 20]);
    assert_eq!(frame::resolve_offset(&mut reps, 3, 0).unwrap(), 4);
    assert_eq!(reps, [4, 5, 10]);
    let mut ones = [1, 2, 3];
    frame::resolve_offset(&mut ones, 3, 0).unwrap();
    let mut zero = [0, 2, 3];
    assert!(frame::resolve_offset(&mut zero, 3, 0).is_err());
}

#[test]
fn malformed_entropy_tables_are_read_errors() {
    // An FSE accuracy log past the field's maximum.
    assert!(fse::Table::read(&[0x0F], 6, 35).is_err());
    // A description whose counts never fill the table.
    assert!(fse::Table::read(&[0x00, 0x00, 0x00, 0x00], 5, 35).is_err());
    // Huffman: no header, a short weight block, weights that don't make a
    // tree, an FSE weight block cut short.
    assert!(huffman::Table::read(&[]).is_err());
    assert!(huffman::Table::read(&[0x80 + 10, 0x11]).is_err());
    assert!(huffman::Table::read(&[0x80 + 3, 0xBB, 0xB0]).is_err());
    assert!(huffman::Table::read(&[20, 0x01]).is_err());
    let table = fse::Table::build(&fse::OF_DEFAULT, 5).unwrap();
    assert!(table.entry(1 << 5).is_err());
    assert!(fse::Table::build(&[-1; 40], 5).is_err());
}

/// The frames the mutation tests start from.
fn seeds() -> Vec<(Vec<u8>, usize)> {
    inputs()
        .into_iter()
        .filter(|(_, d)| !d.is_empty() && d.len() < 100_000)
        .flat_map(|(_, d)| {
            [&["-1"][..], &["-19"]]
                .into_iter()
                .map(move |args| (cli(&d, args), d.len()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn every_truncation_is_an_error_never_a_panic() {
    for (frame, size) in seeds() {
        for cut in 0..frame.len() {
            assert!(decompress(&frame[..cut], size).is_err(), "cut at {cut}");
        }
    }
}

#[test]
fn flipped_bits_never_panic() {
    let mut rng = Rng::new(0x5EED);
    for (frame, size) in seeds() {
        for _ in 0..400 {
            let mut mutated = frame.clone();
            let at = rng.below(mutated.len() as u64) as usize;
            mutated[at] ^= 1 << rng.below(8);
            if rng.below(3) == 0 {
                let at = rng.below(mutated.len() as u64) as usize;
                mutated[at] = rng.below(256) as u8;
            }
            // Bytes or an error, never a panic; bytes are always `size`.
            if let Ok(out) = decompress(&mutated, size) {
                assert_eq!(out.len(), size);
            }
        }
    }
}

#[test]
fn fse_counts_that_do_not_tile_the_table_are_refused() {
    // Two states' worth of counts in a 32-state table: the spread cannot
    // come back to state 0.
    let e = fse::Table::build(&[1, 1], 5).unwrap_err();
    assert!(e.to_string().contains("do not fill"), "{e}");
    // At accuracy 3 the spread step is a multiple of the table size, so it
    // never moves: 2 counts land on state 0 and the other states belong to
    // a symbol with no count.
    let e = fse::Table::build(&[0, 2], 3).unwrap_err();
    assert!(e.to_string().contains("zero-count symbol"), "{e}");
}

#[test]
fn fse_tables_refuse_impossible_counts() {
    let error = |norm: &[i16], log: u32| fse::Table::build(norm, log).unwrap_err().to_string();
    assert!(error(&[1], 70).contains("accuracy log too large"));
    // No symbols at all: the states are left to a symbol with no count.
    assert!(error(&[], 5).contains("symbol without a count"));
    // 64 states' worth of one symbol in a 32-state table.
    assert!(error(&[64], 5).contains("count too large"));
}

#[test]
fn huffman_descriptions_and_streams_refuse_corrupt_input() {
    let error = |data: &[u8]| huffman::Table::read(data).unwrap_err().to_string();
    // Two direct weights (header 127 + 2), both 0: no symbol has a code.
    assert!(error(&[127 + 2, 0x00]).contains("bad Huffman weights"));
    // Three weights need two bytes; one is there.
    assert!(error(&[127 + 3, 0x00]).contains("short Huffman weights"));
    // FSE-coded weights whose table description runs past the 2-byte
    // weight block.
    let e = error(&[2, 0x30, 0x10]);
    assert!(e.contains("FSE table description past its block"), "{e}");
    // A valid one-symbol-plus-implied tree, then streams that end early,
    // have no end marker, or are empty.
    let (table, used) = huffman::Table::read(&[127 + 2, 0x10]).unwrap();
    assert_eq!(used, 2);
    let mut out = [0u8; 4];
    assert!(table.decode(&[], &mut out).is_err());
    assert!(table.decode(&[0x00], &mut out).is_err());
    assert!(table.decode(&[0x01], &mut out).is_err());
}

#[test]
fn an_fse_description_with_too_many_symbols_is_refused() {
    // Accuracy log 5 (low nibble 0), then a first count of 1 (6-bit value
    // 2): with `max_symbol` 0 a second symbol is one too many.
    let e = fse::Table::read(&[0x20, 0x00, 0x00], 5, 0).unwrap_err();
    assert!(e.to_string().contains("too many symbols"), "{e}");
    // A zero count followed by a repeat run that passes `max_symbol`:
    // a 5-bit count of 1 (-> 0) ending at bit 8, then repeats of 3.
    let e = fse::Table::read(&[0x10, 0xFE, 0xFF, 0xFF], 5, 4).unwrap_err();
    assert!(e.to_string().contains("too many symbols"), "{e}");
}

#[test]
fn a_corrupted_checksum_is_refused() {
    let data = b"checksummed content checksummed content".to_vec();
    let mut frame = cli(&data, &["-3"]);
    assert_eq!(decompress(&frame, data.len()).unwrap(), data);
    // The checksum is the frame's last 4 bytes.
    let last = frame.len() - 1;
    frame[last] ^= 0x01;
    assert!(read_error(&frame, data.len()).contains("checksum mismatch"));
}

#[test]
fn a_frame_whose_own_size_differs_from_the_page_is_a_size_mismatch() {
    // A 1-byte content size of 7 for a page declared as 5.
    let mut blocks = block(true, 1, 7).to_vec();
    blocks.push(b'a');
    assert!(matches!(
        decompress(&frame(0, 7, &blocks), 5),
        Err(ZstdError::SizeMismatch {
            expected: 5,
            actual: 7
        })
    ));
    // A 2-byte content size counts from 256.
    let mut two = vec![0x28, 0xB5, 0x2F, 0xFD, 0x60, 0x00, 0x00];
    two.extend_from_slice(&block(true, 1, 256));
    two.push(b'b');
    assert_eq!(decompress(&two, 256).unwrap(), vec![b'b'; 256]);
    assert!(matches!(
        decompress(&two, 255),
        Err(ZstdError::SizeMismatch {
            expected: 255,
            actual: 256
        })
    ));
}

#[test]
fn oversized_blocks_literals_and_sequence_counts_are_refused() {
    // A raw block over 128 KiB.
    let big = 128 * 1024 + 1;
    // Single segment with a 4-byte content size: no window byte.
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0xA0, 0x00, 0x00, 0x00, 0x00];
    f[5..9].copy_from_slice(&(big as u32).to_le_bytes());
    f.extend_from_slice(&block(true, 0, big as u32));
    f.extend(std::iter::repeat_n(b'x', big));
    assert!(read_error(&f, big).contains("block larger than 128 KiB"));
    // RLE literals claiming more than a block regenerates: 20-bit size.
    let regenerated = 128 * 1024 + 1;
    let header = [
        0b0000_1101 | ((regenerated & 0xF) << 4) as u8,
        (regenerated >> 4) as u8,
        (regenerated >> 12) as u8,
        b'r',
        0,
    ];
    let mut blocks = block(true, 2, header.len() as u32).to_vec();
    blocks.extend_from_slice(&header);
    assert!(read_error(&frame(0, 10, &blocks), 10).contains("literals larger than a block"));
    // 100 sequences need at least 300 output bytes; the page holds 10.
    assert!(matches!(
        decompress(&frame(0, 10, &compressed(b"", &[100, 0])), 10),
        Err(ZstdError::SizeMismatch {
            expected: 10,
            actual: 11
        })
    ));
}
