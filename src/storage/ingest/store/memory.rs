//! An in-memory [`TableStore`] for tests, with crash injection: after a
//! set number of writes, every later write fails and applies nothing, as
//! a process that died at that point would leave the store.
//!
//! [`MemoryStore::conditional`] also models a store with conditional puts
//! (AWS S3): the manifest is fenced and the writer takes a lease.
//! [`MemoryStore::client`] is a second handle on the same objects with its
//! own fence, like a second process talking to one bucket.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::lease::{publish_once, ConditionalObjects, Lease, ManifestFence};
use super::{check_key, TableStore};
use crate::storage::column::memory::MemoryFile;
use crate::storage::column::{MmapRegion, Vfs};
use crate::storage::ingest::manifest::MANIFEST_FILE;

/// The objects every client of one store shares.
#[derive(Default, Debug)]
struct Shared {
    objects: Mutex<BTreeMap<String, (Vec<u8>, u64)>>,
    /// The last object version handed out: every write gets a fresh one.
    last_version: AtomicU64,
    /// Writes still allowed; `None` lets every write through.
    writes_left: Mutex<Option<usize>>,
    /// Whether the store honours conditional puts, like AWS S3 (and not
    /// like Garage).
    conditional: bool,
    lease_ttl_ms: AtomicU64,
}

/// Test-only; see the module docs.
#[derive(Default, Debug)]
pub struct MemoryStore {
    shared: Arc<Shared>,
    fence: ManifestFence,
}

impl MemoryStore {
    /// A store with conditional puts: manifest commits are fenced and the
    /// writer takes a lease.
    pub fn conditional() -> MemoryStore {
        MemoryStore {
            shared: Arc::new(Shared {
                conditional: true,
                lease_ttl_ms: AtomicU64::new(30_000),
                ..Shared::default()
            }),
            fence: ManifestFence::default(),
        }
    }

    /// Another client of the same objects, with a fence of its own.
    pub fn client(&self) -> MemoryStore {
        MemoryStore {
            shared: Arc::clone(&self.shared),
            fence: ManifestFence::default(),
        }
    }

    fn bump(&self) -> u64 {
        self.shared
            .last_version
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    fn insert(&self, key: &str, bytes: &[u8]) {
        let version = self.bump();
        self.shared
            .objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), (bytes.to_vec(), version));
    }

    /// Lets the next `writes` puts or deletes through, then fails all later
    /// ones with nothing applied: a crash after that many writes.
    pub fn crash_after(&self, writes: usize) {
        *self.shared.writes_left.lock().unwrap() = Some(writes);
    }

    /// Ends the injected crash: writes go through again, as after a restart.
    pub fn lift_crash(&self) {
        *self.shared.writes_left.lock().unwrap() = None;
    }

    fn write_allowed(&self) -> io::Result<()> {
        let mut left = self.shared.writes_left.lock().unwrap();
        match left.as_mut() {
            Some(0) => Err(io::Error::other("injected crash")),
            Some(n) => {
                *n = n.saturating_sub(1);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// The writer lease, if this store grants one.
    pub fn lease(self: &Arc<Self>, owner: &str) -> io::Result<Option<Lease>> {
        if !self.shared.conditional {
            return Ok(None);
        }
        let ttl = Duration::from_millis(self.shared.lease_ttl_ms.load(Ordering::SeqCst));
        Lease::acquire(Arc::clone(self), owner, ttl).map(Some)
    }

    /// Sets the lease TTL (default 30 s) for leases taken from now on.
    pub fn set_lease_ttl(&self, ttl: Duration) {
        let ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        self.shared.lease_ttl_ms.store(ms, Ordering::SeqCst);
    }
}

impl TableStore for MemoryStore {
    type File = MemoryFile;

    fn open(&self, key: &str) -> io::Result<MemoryFile> {
        let bytes = self
            .get(key)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let vfs = crate::storage::column::MemoryVfs::new();
        vfs.insert("object", bytes);
        vfs.open(std::path::Path::new("object"))
    }

    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        check_key(key)?;
        if self.shared.conditional && key == MANIFEST_FILE {
            // A read of the manifest is what the fence holds a commit to.
            return Ok(self.get_versioned(key)?.map(|(bytes, _)| bytes));
        }
        Ok(self
            .shared
            .objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(b, _)| b.clone()))
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
        if self.shared.conditional && key == MANIFEST_FILE {
            return self.fence.commit(self, key, bytes);
        }
        self.insert(key, bytes);
        Ok(())
    }

    fn publish(&self, local: &std::path::Path, key: &str) -> io::Result<()> {
        check_key(key)?;
        let bytes = std::fs::read(local)?;
        if self.shared.conditional {
            self.write_allowed()?;
            publish_once(self, key, &bytes)?;
        } else {
            self.put(key, &bytes)?;
        }
        std::fs::remove_file(local)
    }

    fn lease(&self, _owner: &str) -> io::Result<Option<Lease>> {
        // Leases need the `Arc` handle: see `Store::lease`.
        Ok(None)
    }

    fn remove_empty_dirs(&self, _prefix: &str) -> io::Result<()> {
        Ok(())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        check_key(key)?;
        self.write_allowed()?;
        // Idempotent, like an object store's delete.
        self.shared.objects.lock().unwrap().remove(key);
        Ok(())
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
            .shared
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|key| under(key))
            .cloned()
            .collect())
    }
}

impl ConditionalObjects for MemoryStore {
    fn get_versioned(&self, key: &str) -> io::Result<Option<(Vec<u8>, String)>> {
        let found = self
            .shared
            .objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(bytes, v)| (bytes.clone(), v.to_string()));
        if key == MANIFEST_FILE && self.shared.conditional {
            self.fence.saw(found.as_ref().map(|(_, v)| v.clone()));
        }
        Ok(found)
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> io::Result<Option<String>> {
        check_key(key)?;
        let mut objects = self.shared.objects.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(None);
        }
        let version = self.bump();
        objects.insert(key.to_owned(), (bytes.to_vec(), version));
        Ok(Some(version.to_string()))
    }

    fn put_if_match(&self, key: &str, bytes: &[u8], version: &str) -> io::Result<Option<String>> {
        check_key(key)?;
        let mut objects = self.shared.objects.lock().unwrap();
        if objects.get(key).map(|(_, v)| v.to_string()).as_deref() != Some(version) {
            return Ok(None);
        }
        let new = self.bump();
        objects.insert(key.to_owned(), (bytes.to_vec(), new));
        Ok(Some(new.to_string()))
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        self.shared.objects.lock().unwrap().remove(key);
        Ok(())
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

    /// Whether a put by client A is refused after another client replaced
    /// `key` since A's `touch` of it: the manifest fence (#1047). Each other
    /// write is a fresh client, whose own fence never refuses it.
    fn stale_put_refused(store: MemoryStore, key: &str, touch: impl Fn(&MemoryStore)) -> bool {
        let a = store.client();
        store.client().put(key, b"1").unwrap();
        touch(&a);
        store.client().put(key, b"2").unwrap();
        a.put(key, b"3").is_err()
    }

    fn put(s: &MemoryStore) {
        s.put(MANIFEST_FILE, b"0").unwrap();
        s.put("data/x", b"0").unwrap();
    }

    fn get(s: &MemoryStore) {
        s.get(MANIFEST_FILE).unwrap();
        s.get("data/x").unwrap();
    }

    fn get_versioned(s: &MemoryStore) {
        s.get_versioned(MANIFEST_FILE).unwrap();
        s.get_versioned("data/x").unwrap();
    }

    // storage_ingest_store_memory_get_ec132490 and
    // storage_ingest_store_memory_put_ec132490 (#1047):
    // `self.shared.conditional && key == MANIFEST_FILE`. A read of the
    // manifest on a conditional store arms the fence, and a put of it
    // checks the fence.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_ec132490__v1_a_conditional_manifest_read_arms_the_fence(
    ) {
        assert!(stale_put_refused(
            MemoryStore::conditional(),
            MANIFEST_FILE,
            get
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_ec132490__v2_a_plain_store_has_no_fence() {
        assert!(!stale_put_refused(
            MemoryStore::default(),
            MANIFEST_FILE,
            get
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_ec132490__v3_another_key_has_no_fence() {
        assert!(!stale_put_refused(
            MemoryStore::conditional(),
            "data/x",
            get
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_put_ec132490__v1_a_conditional_manifest_put_is_fenced() {
        assert!(stale_put_refused(
            MemoryStore::conditional(),
            MANIFEST_FILE,
            put
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_put_ec132490__v2_a_plain_store_overwrites() {
        assert!(!stale_put_refused(
            MemoryStore::default(),
            MANIFEST_FILE,
            put
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_put_ec132490__v3_another_key_overwrites() {
        assert!(!stale_put_refused(
            MemoryStore::conditional(),
            "data/x",
            put
        ));
    }

    // storage_ingest_store_memory_get_versioned_6416aed2 (#1047):
    // `key == MANIFEST_FILE && self.shared.conditional`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_versioned_6416aed2__v1_the_manifest_on_a_conditional_store_arms_the_fence(
    ) {
        assert!(stale_put_refused(
            MemoryStore::conditional(),
            MANIFEST_FILE,
            get_versioned
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_versioned_6416aed2__v2_another_key_does_not() {
        assert!(!stale_put_refused(
            MemoryStore::conditional(),
            "data/x",
            get_versioned
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_memory_get_versioned_6416aed2__v3_a_plain_store_does_not() {
        assert!(!stale_put_refused(
            MemoryStore::default(),
            MANIFEST_FILE,
            get_versioned
        ));
    }
}
