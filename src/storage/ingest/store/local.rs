//! [`TableStore`] over a local directory.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{check_key, TableStore};
use crate::storage::column::posix::PosixFile;
use crate::storage::column::{MmapRegion, PosixVfs, Vfs, VfsFile};

/// A table stored as files under one directory.
#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    /// A store rooted at `root`; the directory need not exist yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalFs { root: root.into() }
    }

    /// The table directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> io::Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(key))
    }
}

impl TableStore for LocalFs {
    type File = PosixFile;

    fn open(&self, key: &str) -> io::Result<PosixFile> {
        PosixVfs.open(&self.path(key)?)
    }

    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(key)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn map(&self, key: &str) -> io::Result<MmapRegion> {
        PosixVfs.open(&self.path(key)?)?.mmap()
    }

    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        super::super::create_dir_all_synced(parent)?;
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        File::open(parent)?.sync_all()
    }

    fn publish(&self, local: &Path, key: &str) -> io::Result<()> {
        let target = self.path(key)?;
        let parent = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        super::super::create_dir_all_synced(parent)?;
        fs::rename(local, &target)?;
        File::open(parent)?.sync_all()
    }

    fn remove_empty_dirs(&self, prefix: &str) -> io::Result<()> {
        let base = prefix.trim_end_matches('/');
        let dir = if base.is_empty() {
            self.root.clone()
        } else {
            self.path(base)?
        };
        prune(&dir, false)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match fs::remove_file(self.path(key)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let base = prefix.trim_end_matches('/');
        let dir = if base.is_empty() {
            self.root.clone()
        } else {
            self.path(base)?
        };
        let mut keys = Vec::new();
        collect(&dir, base, &mut keys)?;
        keys.sort();
        Ok(keys)
    }
}

/// Removes the empty directories under `dir` (and `dir` itself when
/// `remove_self`); returns whether `dir` is left empty.
fn prune(dir: &Path, remove_self: bool) -> io::Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut empty = true;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && prune(&entry.path(), true)? {
            continue;
        }
        empty = false;
    }
    if empty && remove_self {
        fs::remove_dir(dir)?;
    }
    Ok(empty)
}

/// Appends the key of every file under `dir` (whose own key is `base`).
fn collect(dir: &Path, base: &str, keys: &mut Vec<String>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = if base.is_empty() {
            name
        } else {
            format!("{base}/{name}")
        };
        if entry.file_type()?.is_dir() {
            collect(&entry.path(), &key, keys)?;
        } else {
            keys.push(key);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp(name: &str) -> LocalFs {
        let dir = std::env::temp_dir().join(format!("db-core-store-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        LocalFs::new(dir)
    }

    #[test]
    fn local_fs_meets_the_table_store_contract() {
        let s = temp("contract");
        super::super::conformance::run(&s);
        fs::remove_dir_all(s.root()).unwrap();
    }

    #[test]
    fn remove_empty_dirs_prunes_only_empty_ones() {
        let s = temp("prune");
        fs::create_dir_all(s.root().join("live/a/b")).unwrap();
        fs::create_dir_all(s.root().join("live/keep")).unwrap();
        s.put("live/keep/f", b"").unwrap();
        s.remove_empty_dirs("live/").unwrap();
        assert!(!s.root().join("live/a").exists());
        assert!(s.root().join("live/keep/f").exists());
        assert!(s.root().join("live").exists());
        fs::remove_dir_all(s.root()).unwrap();
    }

    #[test]
    fn put_replaces_atomically_and_leaves_no_temp_file() {
        let s = temp("put");
        assert_eq!(s.get("m.json").unwrap(), None);
        s.put("m.json", b"one").unwrap();
        s.put("m.json", b"two").unwrap();
        assert_eq!(s.get("m.json").unwrap().as_deref(), Some(&b"two"[..]));
        assert_eq!(s.list("").unwrap(), ["m.json"]);
        fs::remove_dir_all(s.root()).unwrap();
    }

    #[test]
    fn list_walks_prefixes_and_a_missing_one_is_empty() {
        let s = temp("list");
        fs::create_dir_all(s.root().join("data/sub")).unwrap();
        s.put("data/b", b"").unwrap();
        s.put("data/a", b"").unwrap();
        s.put("data/sub/c", b"").unwrap();
        s.put("top", b"").unwrap();
        assert_eq!(s.list("data/").unwrap(), ["data/a", "data/b", "data/sub/c"]);
        assert_eq!(s.list("").unwrap().len(), 4);
        assert!(s.list("nope/").unwrap().is_empty());
        fs::remove_dir_all(s.root()).unwrap();
    }

    #[test]
    fn map_reports_a_missing_object_as_not_found_and_delete_is_idempotent() {
        let s = temp("gone");
        s.put("x", b"hello").unwrap();
        assert_eq!(&*s.map("x").unwrap(), b"hello");
        s.delete("x").unwrap();
        assert_eq!(s.map("x").err().unwrap().kind(), io::ErrorKind::NotFound);
        s.delete("x").unwrap();
        fs::remove_dir_all(s.root()).unwrap();
    }

    #[test]
    fn keys_cannot_escape_the_root() {
        let s = temp("keys");
        for key in ["../x", "/etc/passwd", "a/../b"] {
            assert_eq!(
                s.get(key).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        fs::remove_dir_all(s.root()).unwrap();
    }

    // storage_ingest_store_local_prune_d25cb504 (#1047):
    // `entry.file_type()?.is_dir() && prune(&entry.path(), true)?`.
    fn pruned(name: &str, setup: impl Fn(&Path)) -> (bool, std::path::PathBuf) {
        let s = temp(name);
        let dir = s.root().join("top");
        fs::create_dir_all(&dir).unwrap();
        setup(&dir);
        (prune(&dir, false).unwrap(), dir)
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_d25cb504__v1_an_empty_subdirectory_is_removed() {
        let (empty, dir) = pruned("p1", |d| fs::create_dir(d.join("sub")).unwrap());
        assert!(empty);
        assert!(!dir.join("sub").exists());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_d25cb504__v2_a_file_keeps_the_directory() {
        let (empty, dir) = pruned("p2", |d| fs::write(d.join("f"), b"").unwrap());
        assert!(!empty);
        assert!(dir.join("f").exists());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_d25cb504__v3_a_subdirectory_with_a_file_is_kept() {
        let (empty, dir) = pruned("p3", |d| {
            fs::create_dir(d.join("sub")).unwrap();
            fs::write(d.join("sub/f"), b"").unwrap();
        });
        assert!(!empty);
        assert!(dir.join("sub/f").exists());
    }

    // storage_ingest_store_local_prune_9d0a6352 (#1047): `empty && remove_self`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_9d0a6352__v1_an_empty_directory_removes_itself() {
        let s = temp("p4");
        let dir = s.root().join("gone");
        fs::create_dir_all(&dir).unwrap();
        assert!(prune(&dir, true).unwrap());
        assert!(!dir.exists());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_9d0a6352__v2_a_directory_with_a_file_stays() {
        let s = temp("p5");
        let dir = s.root().join("kept");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f"), b"").unwrap();
        assert!(!prune(&dir, true).unwrap());
        assert!(dir.exists());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_store_local_prune_9d0a6352__v3_the_top_directory_is_not_removed() {
        let s = temp("p6");
        let dir = s.root().join("top");
        fs::create_dir_all(&dir).unwrap();
        assert!(prune(&dir, false).unwrap());
        assert!(dir.exists());
    }
}
