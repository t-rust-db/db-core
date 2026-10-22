//! Seeded byte-mutation fuzzing of the page decoders (#657): Snappy and
//! ZSTD page bodies, RLE/bit-packed level sections and dictionary-index
//! streams, and DELTA_BINARY_PACKED streams. Seeds are real pages, cut
//! from the fixture files under `tests/fixtures/parquet/` and from files
//! the crate's own writer produces; each is mutated by truncation, bit
//! flips, byte overwrites, and inserted `0xff` runs that inflate whatever
//! varint or length they land in. Every mutant must decode to `Ok` or
//! `Err`: a panic fails the test and prints the input.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::storage::parquet::compression::decompress;
use crate::storage::parquet::encoding::{decode_delta_binary_packed, decode_hybrid_rle_bitpacked};
use crate::storage::parquet::footer::{Codec, Repetition};
use crate::storage::parquet::page::{decode_page_header, Encoding, PageType};
use crate::storage::parquet::ParquetFile;

/// Mutants per seed. Each seed set below has tens of seeds, so this keeps
/// every test well under a second in a debug build.
const MUTANTS: usize = 300;

/// xorshift64*: deterministic, so a failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One mutant of `seed`.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    match rng.below(5) {
        0 => v.truncate(rng.below(v.len() + 1)),
        1 => {
            for _ in 0..=rng.below(4) {
                if !v.is_empty() {
                    let i = rng.below(v.len());
                    v[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..=rng.below(4) {
                if !v.is_empty() {
                    let i = rng.below(v.len());
                    v[i] = rng.next() as u8;
                }
            }
        }
        3 => {
            let i = rng.below(v.len() + 1);
            let n = 1 + rng.below(8);
            v.splice(i..i, std::iter::repeat_n(0xff, n));
        }
        _ => {
            // A maximal 5-byte varint: a ~34-bit length or count.
            let i = rng.below(v.len() + 1);
            v.splice(i..i, [0xff, 0xff, 0xff, 0xff, 0x0f]);
        }
    }
    v
}

/// Run `decode` on `MUTANTS` mutants of every seed; any panic fails the
/// test, naming the input's length and first bytes.
fn fuzz(what: &str, seeds: &[Vec<u8>], mut decode: impl FnMut(&[u8])) {
    assert!(!seeds.is_empty(), "{what}: no seeds");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for seed in seeds {
        // The unmutated seed must decode too.
        decode(seed);
        for _ in 0..MUTANTS {
            let input = mutate(&mut rng, seed);
            if catch_unwind(AssertUnwindSafe(|| decode(&input))).is_err() {
                let head = &input[..input.len().min(64)];
                panic!(
                    "{what} panicked on a {}-byte input starting {head:02x?}",
                    input.len()
                );
            }
        }
    }
}

/// Every page of every column chunk in a Parquet file.
struct Page {
    codec: Codec,
    header: PageType,
    uncompressed_size: usize,
    body: Vec<u8>,
    /// Whether the column is optional (its data pages start with a
    /// definition-level section).
    optional: bool,
}

fn pages_of(bytes: &[u8]) -> Vec<Page> {
    let file = ParquetFile::open(bytes).unwrap();
    let mut out = Vec::new();
    for rg in &file.metadata().row_groups {
        for (column, chunk) in rg.columns.iter().enumerate() {
            let meta = chunk.meta_data.as_ref().unwrap();
            let optional = file
                .leaf_element(column)
                .is_some_and(|e| e.repetition == Some(Repetition::Optional));
            let start = meta.dictionary_page_offset.unwrap_or(meta.data_page_offset) as usize;
            let end = start + meta.total_compressed_size as usize;
            let mut pos = start;
            while pos < end {
                let (header, consumed) = decode_page_header(&bytes[pos..]).unwrap();
                let body_start = pos + consumed;
                let body_end = body_start + header.compressed_page_size as usize;
                out.push(Page {
                    codec: meta.codec,
                    header: header.page_type,
                    uncompressed_size: header.uncompressed_page_size as usize,
                    body: bytes[body_start..body_end].to_vec(),
                    optional,
                });
                pos = body_end;
            }
        }
    }
    out
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/parquet/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

/// Files the crate's writer produces: Snappy, with and without
/// dictionaries, nullable columns so data pages carry level sections.
#[cfg(feature = "storage-column-write")]
fn written_files() -> Vec<Vec<u8>> {
    use crate::storage::parquet::writer::{
        ColumnSpec, ColumnType, ColumnValues, Compression, ParquetWriter, WriterOptions,
    };
    let rows = 3_000i64;
    [true, false]
        .into_iter()
        .map(|dictionary| {
            let schema = vec![
                ColumnSpec::new("k", ColumnType::Int64, true),
                ColumnSpec::new("d", ColumnType::Double, true),
                ColumnSpec::new("s", ColumnType::Utf8, true),
                ColumnSpec::new("b", ColumnType::Boolean, false),
            ];
            let options = WriterOptions {
                compression: Compression::Snappy,
                dictionary,
                row_group_rows: 1_000,
                ..WriterOptions::default()
            };
            let mut w = ParquetWriter::new(Vec::new(), schema, options).unwrap();
            let names = ["alpha", "beta", "gamma", "delta", "epsilon"];
            w.write_columns(vec![
                ColumnValues::Int64((0..rows).map(|i| (i % 7 != 0).then_some(i % 300)).collect()),
                ColumnValues::Double(
                    (0..rows)
                        .map(|i| (i % 5 != 0).then_some(i as f64 * 0.25))
                        .collect(),
                ),
                ColumnValues::Utf8(
                    (0..rows)
                        .map(|i| (i % 3 != 0).then(|| names[(i % 5) as usize].to_string()))
                        .collect(),
                ),
                ColumnValues::Boolean((0..rows).map(|i| Some(i % 2 == 0)).collect()),
            ])
            .unwrap();
            w.finish().unwrap().0
        })
        .collect()
}

#[cfg(not(feature = "storage-column-write"))]
fn written_files() -> Vec<Vec<u8>> {
    Vec::new()
}

fn all_pages() -> Vec<Page> {
    let mut files = vec![
        fixture("production.parquet"),
        fixture("production_snappy.parquet"),
        fixture("nested_shapes.parquet"),
        fixture("nested_map.parquet"),
    ];
    files.extend(written_files());
    files.iter().flat_map(|f| pages_of(f)).collect()
}

/// Each optional flat data page's definition-level section and value
/// count, and, for dictionary-encoded pages, the index stream after it
/// with its bit width and index count. Decompressed, so the seeds are the
/// bytes the level and index decoders really see.
struct LevelSeeds {
    levels: Vec<(Vec<u8>, usize)>,
    indices: Vec<(Vec<u8>, u32, usize)>,
}

fn level_seeds(pages: &[Page]) -> LevelSeeds {
    let mut seeds = LevelSeeds {
        levels: Vec::new(),
        indices: Vec::new(),
    };
    for page in pages {
        let PageType::Data(header) = &page.header else {
            continue;
        };
        if !page.optional {
            continue;
        }
        let body = decompress(page.codec, &page.body, page.uncompressed_size).unwrap();
        let len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
        let level_bytes = body[4..4 + len].to_vec();
        let num_values = header.num_values as usize;
        let levels = decode_hybrid_rle_bitpacked(&level_bytes, 1, num_values).unwrap();
        seeds.levels.push((level_bytes, num_values));
        if matches!(
            header.encoding,
            Encoding::PlainDictionary | Encoding::RleDictionary
        ) {
            let values = &body[4 + len..];
            let present = levels.iter().filter(|&&l| l == 1).count();
            seeds
                .indices
                .push((values[1..].to_vec(), u32::from(values[0]), present));
        }
    }
    seeds
}

#[test]
fn mutated_compressed_pages_never_panic() {
    let pages = all_pages();
    let compressed: Vec<&Page> = pages
        .iter()
        .filter(|p| matches!(p.codec, Codec::Snappy | Codec::Zstd))
        .collect();
    assert!(
        compressed.iter().any(|p| p.codec == Codec::Snappy),
        "no Snappy seeds"
    );
    for page in compressed {
        let seeds = vec![page.body.clone()];
        fuzz("decompress", &seeds, |input| {
            let _outcome = decompress(page.codec, input, page.uncompressed_size);
        });
    }
}

#[test]
fn mutated_level_sections_never_panic() {
    let seeds = level_seeds(&all_pages());
    let inputs: Vec<Vec<u8>> = seeds.levels.iter().map(|(b, _)| b.clone()).collect();
    for (bytes, num_values) in &seeds.levels {
        fuzz("levels", std::slice::from_ref(bytes), |input| {
            let _outcome = decode_hybrid_rle_bitpacked(input, 1, *num_values);
        });
    }
    // The same streams read at every bit width a level or index can have.
    fuzz("levels at other widths", &inputs, |input| {
        for bit_width in [0, 2, 3, 7, 8, 9, 17, 31, 32, 33] {
            let _outcome = decode_hybrid_rle_bitpacked(input, bit_width, 64);
        }
    });
}

#[test]
fn mutated_dictionary_indices_never_panic() {
    let seeds = level_seeds(&all_pages());
    assert!(!seeds.indices.is_empty(), "no dictionary-encoded seeds");
    for (bytes, bit_width, count) in &seeds.indices {
        // Mutate the bit-width byte along with the stream it governs.
        let mut seed = vec![*bit_width as u8];
        seed.extend_from_slice(bytes);
        fuzz("dictionary indices", &[seed], |input| {
            if let Some((&width, stream)) = input.split_first() {
                let _outcome = decode_hybrid_rle_bitpacked(stream, u32::from(width), *count);
            }
        });
    }
}

/// A DELTA_BINARY_PACKED stream for `values`: block size 128, 4
/// miniblocks of 32 (the layout parquet-mr and Arrow write).
fn delta_encode(values: &[i64]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn zigzag(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }
    let mut out = Vec::new();
    varint(&mut out, 128);
    varint(&mut out, 4);
    varint(&mut out, values.len() as u64);
    varint(&mut out, zigzag(values[0]));
    let deltas: Vec<i64> = values.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
    for block in deltas.chunks(128) {
        let min = *block.iter().min().unwrap();
        varint(&mut out, zigzag(min));
        let mut minis: Vec<Vec<u64>> = block
            .chunks(32)
            .map(|m| m.iter().map(|&d| d.wrapping_sub(min) as u64).collect())
            .collect();
        minis.resize(4, Vec::new());
        let widths: Vec<u32> = minis
            .iter()
            .map(|m| 64 - m.iter().max().copied().unwrap_or(0).leading_zeros())
            .collect();
        out.extend(widths.iter().map(|&w| w as u8));
        for (mini, &width) in minis.iter_mut().zip(&widths) {
            mini.resize(32, 0);
            let mut acc: u128 = 0;
            let mut bits = 0u32;
            for &v in mini.iter() {
                acc |= u128::from(v) << bits;
                bits += width;
                while bits >= 8 {
                    out.push(acc as u8);
                    acc >>= 8;
                    bits -= 8;
                }
            }
            if bits > 0 {
                out.push(acc as u8);
            }
        }
    }
    out
}

#[test]
fn the_delta_seed_encoder_round_trips() {
    let values: Vec<i64> = (0..300).map(|i| i * i - 7 * i).collect();
    assert_eq!(
        decode_delta_binary_packed(&delta_encode(&values), values.len()).unwrap(),
        values
    );
}

#[test]
fn mutated_delta_streams_never_panic() {
    let seeds: Vec<Vec<u8>> = [
        (0..300).map(|i| i * 1013 % 977).collect::<Vec<i64>>(),
        (0..200).map(|i| 1_000_000 + i).collect(),
        vec![i64::MIN, i64::MAX, 0, -1, 1, i64::MAX, i64::MIN],
        vec![42],
    ]
    .iter()
    .map(|v| delta_encode(v))
    .collect();
    fuzz("delta", &seeds, |input| {
        let _outcome = decode_delta_binary_packed(input, 1 << 16);
    });
}

/// `data` compressed by the `zstd` CLI with `args` (libzstd).
fn zstd_cli(data: &[u8], args: &[&str]) -> Vec<u8> {
    use std::io::Write;
    let mut child = std::process::Command::new("zstd")
        .args(["-q", "-c"])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("zstd CLI must be available for this test");
    child.stdin.take().unwrap().write_all(data).unwrap();
    child.wait_with_output().unwrap().stdout
}

/// #700: the zstd decoder on mutants of libzstd frames, which between them
/// use every literals and sequence mode, so a mutation can land in any
/// table description. Half the mutants change only the first 64 bytes:
/// the frame and first block headers, the literals header and the
/// Huffman/FSE table descriptions, where a corrupt value redirects
/// everything read after it. A decode that succeeds must be exactly the
/// declared size. `ZSTD_FUZZ_MUTANTS` sets the mutants per seed (2,000
/// by default) for a longer local run.
#[test]
fn mutated_libzstd_frames_never_panic() {
    let mutants: usize = std::env::var("ZSTD_FUZZ_MUTANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let text: Vec<u8> = (0..4_000u32)
        .flat_map(|i| format!("event {} from host {} ", i % 97, i % 13).into_bytes())
        .collect();
    let digits: Vec<u8> = (0..3_000u32)
        .map(|i| b'0' + (i.wrapping_mul(7) % 10) as u8)
        .collect();
    let ints: Vec<u8> = (0..2_000u32).flat_map(|i| (i * i).to_le_bytes()).collect();
    let mut seeds = Vec::new();
    for data in [&text, &digits, &ints] {
        for args in [&["-1"][..], &["-3"], &["-19"], &["-19", "--no-check"]] {
            seeds.push((zstd_cli(data, args), data.len()));
        }
    }
    let mut rng = Rng(0x0700_7a5d_0000_0001);
    for (seed, size) in &seeds {
        assert_eq!(&decompress(Codec::Zstd, seed, *size).unwrap().len(), size);
        for i in 0..mutants {
            let input = if i % 2 == 0 {
                mutate(&mut rng, seed)
            } else {
                let head = seed.len().min(64);
                let mut v = mutate(&mut rng, &seed[..head]);
                v.extend_from_slice(&seed[head..]);
                v
            };
            let outcome = catch_unwind(AssertUnwindSafe(|| decompress(Codec::Zstd, &input, *size)));
            match outcome {
                Err(_) => panic!(
                    "zstd panicked on a {}-byte mutant starting {:02x?}",
                    input.len(),
                    &input[..input.len().min(64)]
                ),
                Ok(Ok(out)) => assert_eq!(out.len(), *size),
                Ok(Err(_)) => {}
            }
        }
    }
}
