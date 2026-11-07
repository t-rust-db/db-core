//! The behaviour every [`TableStore`] must share. Each backend's tests run
//! [`run`], so a backend that drifts from `LocalFs` fails here.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicU32, Ordering};

use super::TableStore;
use crate::storage::column::VfsFile;

static SCRATCH: AtomicU32 = AtomicU32::new(0);

/// A fresh local directory for files to publish.
fn scratch() -> std::path::PathBuf {
    let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("db-core-conformance-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn run(store: &impl TableStore) {
    // Empty: missing objects are None / NotFound, and listing is empty.
    assert_eq!(store.get("a").unwrap(), None);
    assert_eq!(store.map("a").err().unwrap().kind(), ErrorKind::NotFound);
    store.delete("a").unwrap(); // idempotent
    assert!(store.list("").unwrap().is_empty());

    // Put, then replace: the last put wins, and the key is listed once.
    store.put("a", b"one").unwrap();
    store.put("a", b"two").unwrap();
    assert_eq!(store.get("a").unwrap().as_deref(), Some(&b"two"[..]));
    assert_eq!(&*store.map("a").unwrap(), b"two");
    assert_eq!(store.list("").unwrap(), ["a"]);

    // Prefixes match whole path components: `data/` must not catch `data2/`.
    store.put("data/x", b"").unwrap();
    store.put("data/sub/y", b"").unwrap();
    store.put("data2/z", b"").unwrap();
    assert_eq!(store.list("data/").unwrap(), ["data/sub/y", "data/x"]);
    assert_eq!(store.list("data").unwrap(), store.list("data/").unwrap());

    // Open reads at an offset; a missing object is NotFound.
    store.put("o", b"0123456789").unwrap();
    let f = store.open("o").unwrap();
    assert_eq!(f.size().unwrap(), 10);
    let mut buf = [0u8; 4];
    assert_eq!(f.read_at(3, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"3456");
    assert_eq!(f.read_at(8, &mut buf).unwrap(), 2);
    assert_eq!(f.read_at(10, &mut buf).unwrap(), 0);
    assert_eq!(&*f.mmap().unwrap(), b"0123456789");
    assert_eq!(
        store.open("missing").err().unwrap().kind(),
        ErrorKind::NotFound
    );
    store.delete("o").unwrap();

    // Publish moves a local file in (creating its "directory") and leaves
    // nothing behind locally; publishing the same bytes again is fine.
    let dir = scratch();
    let file = dir.join("f");
    std::fs::write(&file, b"first").unwrap();
    store.publish(&file, "pub/deep/f").unwrap();
    assert!(!file.exists());
    assert_eq!(
        store.get("pub/deep/f").unwrap().as_deref(),
        Some(&b"first"[..])
    );
    std::fs::write(&file, b"first").unwrap();
    store.publish(&file, "pub/deep/f").unwrap();
    // Different bytes under a taken name are either replaced or refused (a
    // store with conditional puts), and the object matches the outcome.
    std::fs::write(&file, b"second").unwrap();
    match store.publish(&file, "pub/deep/f") {
        Ok(()) => assert_eq!(
            store.get("pub/deep/f").unwrap().as_deref(),
            Some(&b"second"[..])
        ),
        Err(e) => {
            assert_eq!(e.kind(), ErrorKind::AlreadyExists);
            assert_eq!(
                store.get("pub/deep/f").unwrap().as_deref(),
                Some(&b"first"[..])
            );
        }
    }
    assert_eq!(
        store
            .publish(&dir.join("absent"), "pub/x")
            .err()
            .unwrap()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(store.get("pub/x").unwrap(), None);
    std::fs::remove_dir_all(&dir).unwrap();

    // Removing empty directories never removes an object.
    store.remove_empty_dirs("").unwrap();
    store.remove_empty_dirs("pub/").unwrap();
    assert_eq!(store.list("pub/").unwrap(), ["pub/deep/f"]);
    store.delete("pub/deep/f").unwrap();

    // Delete removes the object only.
    store.delete("a").unwrap();
    assert_eq!(store.get("a").unwrap(), None);
    assert_eq!(store.get("data/x").unwrap().as_deref(), Some(&b""[..]));

    // Keys stay under the root whatever the backend.
    for key in ["/etc/passwd", "../x", "a/../b", "a//b", "a/./b", ""] {
        assert_eq!(
            store.get(key).err().unwrap().kind(),
            ErrorKind::InvalidInput,
            "{key:?}"
        );
        assert_eq!(
            store.put(key, b"").err().unwrap().kind(),
            ErrorKind::InvalidInput,
            "{key:?}"
        );
    }
}
