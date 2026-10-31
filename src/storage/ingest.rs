//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! Durability (#571): the WAL assigns LSNs and group-commits framed
//! records to `<table>/wal/`, tracking the `visible`/`durable`/`flushed`
//! watermarks; a `LOCK` file keeps a table to one writer; replay on open
//! recovers everything up to a torn tail and refuses to drop data past real
//! corruption. #572 adds pages, the flush to Parquet under an atomically
//! replaced manifest (`TableWriter`), and [`Snapshot`], the read view over
//! both; #573 the time-tiered layout; #574 the public `Table` handle and
//! its line sources.
//!
//! ADR 0029 splits this module in two: the `ingest-read` feature is what a
//! reader of a table directory needs (manifest, pages, the WAL tail,
//! [`Snapshot`]), with no Parquet or WAL writer; `ingest` adds the write
//! side.

// Read side (`ingest-read`): what a reader of a table directory needs.
mod crc32c;
pub mod error;
mod fields;
mod frame;
pub mod layout;
pub mod manifest;
pub mod page;
pub mod record;
mod recover;
pub mod snapshot;
pub mod wal;

// Write side (`ingest`): the Parquet flush, tiers, the lock and the writer.
#[cfg(feature = "ingest")]
mod flush;
#[cfg(feature = "ingest")]
pub mod handle;
#[cfg(feature = "ingest")]
pub mod lock;
#[cfg(feature = "ingest")]
pub mod table;
#[cfg(feature = "ingest")]
mod tier;

pub use error::IngestError;
#[cfg(feature = "ingest")]
pub use handle::{Table, FOLLOW_DIR};
pub use layout::{Area, Layout, LayoutKind, SessionKey, TierOptions};
#[cfg(feature = "ingest")]
pub use lock::{LockError, TableLock, LOCK_FILE};
pub use manifest::{FileEntry, KeyBound, KeyRange, Manifest, ManifestError, MANIFEST_FILE};
pub use page::{Page, PageBody};
pub use record::{RecordValue, RowPage};
pub use snapshot::{ReaderStats, Snapshot, TableReader};
#[cfg(feature = "ingest")]
pub use table::{FlushStatus, TableOptions, TableWriter, DATA_DIR, TMP_DIR};
#[cfg(feature = "ingest")]
pub use wal::Wal;
pub use wal::{Ack, Record, WalError, WalOptions, Watermarks, WAL_DIR};

/// Creates `dir` and any missing parents, fsyncing the parent of each
/// directory it creates (#648): `fs::create_dir_all` leaves a new entry in
/// the parent's page cache, so a power loss could drop a directory whose
/// files a durable manifest (or a durable WAL ack) already counts on.
#[cfg(feature = "ingest")]
pub(crate) fn create_dir_all_synced(dir: &std::path::Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut at = Some(dir);
    while let Some(path) = at {
        if path.as_os_str().is_empty() || path.is_dir() {
            break;
        }
        missing.push(path);
        at = path.parent();
    }
    for path in missing.into_iter().rev() {
        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        };
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(all(test, feature = "ingest"))]
#[allow(
    non_snake_case,
    clippy::unwrap_used,
    reason = "test code fails fast (db-core#230); MC/DC tags"
)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors (`mcdc__<id>__vN`) for `create_dir_all_synced`:
    //! the walk up stops at an empty path or an existing directory.

    fn scratch(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("db-core-dir-synced-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        root
    }

    #[test]
    fn mcdc__storage_ingest_create_dir_all_synced_a0ca1343__v1_every_missing_level_is_created() {
        let root = scratch("v1");
        let deep = root.join("staging").join("date=2026-10-11").join("hour=03");
        super::create_dir_all_synced(&deep).unwrap();
        assert!(deep.is_dir());
        let file = root.join("plain");
        std::fs::write(&file, b"x").unwrap();
        assert!(super::create_dir_all_synced(&file.join("below")).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mcdc__storage_ingest_create_dir_all_synced_a0ca1343__v2_a_relative_path_stops_at_the_empty_parent(
    ) {
        // Relative and missing at the top: the walk reaches the empty
        // parent of its first component.
        let top =
            std::path::PathBuf::from(format!("db-core-dir-synced-rel-{}", std::process::id()));
        std::fs::remove_dir_all(&top).ok();
        let rel = top.join("a");
        super::create_dir_all_synced(&rel).unwrap();
        assert!(rel.is_dir());
        std::fs::remove_dir_all(&top).ok();
    }

    #[test]
    fn mcdc__storage_ingest_create_dir_all_synced_a0ca1343__v3_an_existing_directory_is_left_as_is()
    {
        let root = scratch("v3");
        std::fs::create_dir_all(&root).unwrap();
        super::create_dir_all_synced(&root).unwrap();
        assert!(root.is_dir());
        std::fs::remove_dir_all(&root).ok();
    }
}
