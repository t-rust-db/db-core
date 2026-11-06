//! The behaviour every [`TableStore`] must share. Each backend's tests run
//! [`run`], so a backend that drifts from `LocalFs` fails here.

use std::io::ErrorKind;

use super::TableStore;

pub(super) fn run(store: &impl TableStore) {
    // Empty: missing objects are None / NotFound, and listing is empty.
    assert_eq!(store.get("a").unwrap(), None);
    assert_eq!(store.map("a").err().unwrap().kind(), ErrorKind::NotFound);
    assert_eq!(store.delete("a").err().unwrap().kind(), ErrorKind::NotFound);
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
