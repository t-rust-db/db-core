//! [`TableStore`] over a local directory.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{check_key, TableStore};
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
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        fs::remove_file(self.path(key)?)
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
    fn map_and_delete_report_a_missing_object_as_not_found() {
        let s = temp("gone");
        s.put("x", b"hello").unwrap();
        assert_eq!(&*s.map("x").unwrap(), b"hello");
        s.delete("x").unwrap();
        assert_eq!(s.map("x").err().unwrap().kind(), io::ErrorKind::NotFound);
        assert_eq!(s.delete("x").err().unwrap().kind(), io::ErrorKind::NotFound);
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
}
