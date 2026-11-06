//! An in-memory [`TableStore`] for tests, with crash injection: after a
//! set number of writes, every later write fails and applies nothing, as
//! a process that died at that point would leave the store.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::{check_key, TableStore};
use crate::storage::column::MmapRegion;

#[derive(Default)]
pub(super) struct MemoryStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Writes still allowed; `None` lets every write through.
    writes_left: Mutex<Option<usize>>,
}

impl MemoryStore {
    /// Lets the next `writes` puts or deletes through, then fails all later
    /// ones with nothing applied: a crash after that many writes.
    pub(super) fn crash_after(&self, writes: usize) {
        *self.writes_left.lock().unwrap() = Some(writes);
    }

    fn write_allowed(&self) -> io::Result<()> {
        let mut left = self.writes_left.lock().unwrap();
        match left.as_mut() {
            Some(0) => Err(io::Error::other("injected crash")),
            Some(n) => {
                *n = n.saturating_sub(1);
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl TableStore for MemoryStore {
    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        check_key(key)?;
        Ok(self.objects.lock().unwrap().get(key).cloned())
    }

    fn map(&self, key: &str) -> io::Result<MmapRegion> {
        match self.get(key)? {
            Some(bytes) => Ok(MmapRegion::Owned(Arc::from(bytes))),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        check_key(key)?;
        self.write_allowed()?;
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        check_key(key)?;
        self.write_allowed()?;
        match self.objects.lock().unwrap().remove(key) {
            Some(_) => Ok(()),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let base = prefix.trim_end_matches('/');
        if !base.is_empty() {
            check_key(base)?;
        }
        let under = |key: &str| {
            base.is_empty()
                || key == base
                || key.strip_prefix(base).is_some_and(|r| r.starts_with('/'))
        };
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|key| under(key))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::conformance;
    use super::*;
    use crate::storage::ingest::layout::LayoutKind;
    use crate::storage::ingest::manifest::{FileEntry, Manifest};
    use crate::storage::stream::detect::Format;

    #[test]
    fn memory_store_meets_the_table_store_contract() {
        conformance::run(&MemoryStore::default());
    }

    fn manifest(files: u64) -> Manifest {
        (1..=files).fold(Manifest::new(Format::Jsonl, LayoutKind::Flat), |m, n| {
            m.with_file(FileEntry {
                path: format!("data/{n:020}-{n:020}.parquet"),
                area: crate::storage::ingest::layout::Area::Data,
                lsn_lo: n,
                lsn_hi: n,
                rows: 1,
                bytes: 1,
                observed_ts: None,
                timestamp: None,
                keys: Vec::new(),
            })
        })
    }

    #[test]
    fn a_crash_at_the_manifest_commit_keeps_the_previous_manifest() {
        let store = MemoryStore::default();
        manifest(1).write_to(&store).unwrap();
        // Crash before the commit lands: the new manifest is never written.
        store.crash_after(0);
        assert!(manifest(2).write_to(&store).is_err());
        assert_eq!(Manifest::read_from(&store).unwrap().unwrap(), manifest(1));
    }

    #[test]
    fn a_completed_commit_survives_a_later_crash() {
        let store = MemoryStore::default();
        store.crash_after(1);
        manifest(1).write_to(&store).unwrap();
        assert!(manifest(2).write_to(&store).is_err());
        assert_eq!(Manifest::read_from(&store).unwrap().unwrap(), manifest(1));
    }

    #[test]
    fn a_crashed_delete_removes_nothing() {
        let store = MemoryStore::default();
        store.put("x", b"1").unwrap();
        store.crash_after(0);
        assert!(store.delete("x").is_err());
        assert_eq!(store.get("x").unwrap().as_deref(), Some(&b"1"[..]));
    }
}
