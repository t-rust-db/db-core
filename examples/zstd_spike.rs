//! SPIKE #697 (not for merging): decode speed of every ZSTD page in the
//! given Parquet files with ruzstd, and the same frames written out
//! (concatenated, a valid multi-frame stream) for `zstd -d` to time.
//!
//!     cargo run --release --example zstd_spike -- out.zst file.parquet...

use std::time::Instant;

use db_core::storage::parquet::compression::zstd;
use db_core::storage::ParquetFile;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, files) = args.split_first().expect("usage: zstd_spike <out.zst> <file.parquet>...");
    let mut pages: Vec<(Vec<u8>, usize)> = Vec::new();
    for path in files {
        let bytes = std::fs::read(path).expect("read");
        let file = ParquetFile::open(&bytes).expect("open");
        for rg in 0..file.num_row_groups() {
            let rg = file.row_group(rg).expect("row group");
            for c in 0..rg.num_columns() {
                pages.extend(rg.spike_zstd_pages(c).expect("pages"));
            }
        }
    }
    let compressed: usize = pages.iter().map(|p| p.0.len()).sum();
    let uncompressed: usize = pages.iter().map(|p| p.1).sum();
    std::fs::write(out, pages.iter().flat_map(|p| p.0.iter().copied()).collect::<Vec<u8>>()).expect("write");
    let mut best = f64::MAX;
    for _ in 0..10 {
        let start = Instant::now();
        for (frame, size) in &pages {
            let body = zstd::decompress(frame, *size).expect("decompress");
            assert_eq!(body.len(), *size);
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    println!(
        "pages={} compressed={} uncompressed={} ruzstd_best_ms={:.2} ruzstd_mb_s={:.0}",
        pages.len(),
        compressed,
        uncompressed,
        best * 1e3,
        uncompressed as f64 / best / 1e6
    );
}
