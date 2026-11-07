//! Single-writer protection for a store with conditional puts (#1015).
//!
//! A local directory has `flock`; an object store has `If-None-Match: *`
//! and `If-Match: <etag>`. Two pieces, both over [`ConditionalObjects`]:
//!
//! - a **lease**: the object `LOCK` names its holder and when the lease
//!   runs out. A writer takes it with a create-only put, or takes over an
//!   expired one with a put that matches the stale version; a background
//!   thread renews it. A writer that dies is replaced after the lease
//!   expires.
//! - **manifest fencing**: the manifest is only ever replaced by a put that
//!   matches the version this writer last read or wrote, so a writer that
//!   lost its lease cannot overwrite the commit of the one that took over.
//!   The lease says who may start; the fence is what keeps the data safe.
//!
//! Lease times are wall-clock milliseconds, so machines sharing a prefix
//! need clocks within a fraction of the TTL.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The lease object's key.
pub const LEASE_KEY: &str = "LOCK";

/// Objects that can be written conditionally on their version (an ETag, a
/// generation: any string that changes with every write).
pub trait ConditionalObjects: Send + Sync {
    /// The object and its version, or `None` if there is none.
    fn get_versioned(&self, key: &str) -> io::Result<Option<(Vec<u8>, String)>>;

    /// Creates the object unless it exists: its new version, or `None` if it
    /// already existed.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> io::Result<Option<String>>;

    /// Replaces the object if it is still at `version`: the new version, or
    /// `None` if it has moved on (or is gone).
    fn put_if_match(&self, key: &str, bytes: &[u8], version: &str) -> io::Result<Option<String>>;

    /// Removes the object (idempotent).
    fn remove(&self, key: &str) -> io::Result<()>;
}

/// The error for a writer that lost the race: another writer holds the
/// table, or committed to it since this one read it.
pub fn fenced(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, what.to_string())
}

/// Publishes `bytes` as `key` without ever replacing a different object:
/// file names come from LSN ranges, so a writer that lost the table and one
/// that took it over can both pick the same name, and a plain put would
/// overwrite a file the other's committed manifest lists. Creating the object
/// only if it is absent, and accepting an existing one that is identical (a
/// retry of this very publish), keeps the stale writer out.
pub fn publish_once<C: ConditionalObjects + ?Sized>(
    objects: &C,
    key: &str,
    bytes: &[u8],
) -> io::Result<()> {
    if objects.put_if_absent(key, bytes)?.is_some() {
        return Ok(());
    }
    match objects.get_versioned(key)? {
        Some((existing, _)) if existing == bytes => Ok(()),
        _ => Err(fenced(
            "another writer already published a different object under this name",
        )),
    }
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn encode(owner: &str, expires_ms: u64) -> Vec<u8> {
    format!("owner={owner}\nexpires_ms={expires_ms}\n").into_bytes()
}

/// `(owner, expires_ms)`; an unreadable lease is treated as expired, so a
/// corrupt object cannot lock a table forever.
fn decode(bytes: &[u8]) -> (String, u64) {
    let text = String::from_utf8_lossy(bytes);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.strip_prefix('='))
            .map(str::to_string)
    };
    (
        field("owner").unwrap_or_default(),
        field("expires_ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    )
}

/// Held until dropped: the writer lease on a store.
#[derive(Debug)]
pub struct Lease {
    stop: Arc<(Mutex<bool>, Condvar)>,
    lost: Arc<AtomicBool>,
    renewer: Option<JoinHandle<()>>,
}

impl Lease {
    /// Takes the lease for `owner` for `ttl`, renewing it every third of the
    /// `ttl` until dropped. Fails with [`io::ErrorKind::AlreadyExists`] while
    /// another writer's lease is alive.
    pub fn acquire<C: ConditionalObjects + 'static>(
        objects: Arc<C>,
        owner: &str,
        ttl: Duration,
    ) -> io::Result<Lease> {
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        let mut version = take(&*objects, owner, ttl_ms)?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let lost = Arc::new(AtomicBool::new(false));
        let renewer = {
            let (stop, lost, owner) = (Arc::clone(&stop), Arc::clone(&lost), owner.to_string());
            std::thread::Builder::new()
                .name("db-core-lease".to_string())
                .spawn(move || {
                    let (flag, wake) = &*stop;
                    let mut expires = now_ms().saturating_add(ttl_ms);
                    loop {
                        let guard = flag.lock().unwrap_or_else(PoisonError::into_inner);
                        let (guard, _) = wake
                            .wait_timeout_while(
                                guard,
                                ttl.checked_div(3).unwrap_or(ttl),
                                |stopped| !*stopped,
                            )
                            .unwrap_or_else(PoisonError::into_inner);
                        if *guard {
                            // A clean shutdown frees the table at once, if
                            // the lease is still ours.
                            drop(guard);
                            if !lost.load(Ordering::SeqCst) {
                                let ours = objects.get_versioned(LEASE_KEY);
                                if matches!(&ours, Ok(Some((_, v))) if *v == version) {
                                    objects.remove(LEASE_KEY).ok();
                                }
                            }
                            return;
                        }
                        drop(guard);
                        let next = now_ms().saturating_add(ttl_ms);
                        match objects.put_if_match(LEASE_KEY, &encode(&owner, next), &version) {
                            Ok(Some(v)) => {
                                version = v;
                                expires = next;
                            }
                            // Someone else holds it now.
                            Ok(None) => {
                                lost.store(true, Ordering::SeqCst);
                                return;
                            }
                            // A transient error: keep trying until the
                            // lease would have run out anyway.
                            Err(_) if now_ms() < expires => {}
                            Err(_) => {
                                lost.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                })?
        };
        Ok(Lease {
            stop,
            lost,
            renewer: Some(renewer),
        })
    }

    /// Whether another writer has taken the lease over (or this one could not
    /// renew it before it ran out).
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let (flag, wake) = &*self.stop;
        *flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        wake.notify_all();
        if let Some(renewer) = self.renewer.take() {
            renewer.join().ok();
        }
    }
}

/// Creates the lease object, or takes over an expired one. Returns its
/// version.
fn take<C: ConditionalObjects + ?Sized>(
    objects: &C,
    owner: &str,
    ttl_ms: u64,
) -> io::Result<String> {
    let body = encode(owner, now_ms().saturating_add(ttl_ms));
    match objects.get_versioned(LEASE_KEY)? {
        None => objects
            .put_if_absent(LEASE_KEY, &body)?
            .ok_or_else(|| fenced("another writer took the table first")),
        Some((held, version)) => {
            let (holder, expires) = decode(&held);
            if expires > now_ms() {
                return Err(fenced(&format!(
                    "the table is leased by {holder:?} until {expires} ms"
                )));
            }
            objects
                .put_if_match(LEASE_KEY, &body, &version)?
                .ok_or_else(|| fenced("another writer took over the expired lease first"))
        }
    }
}

/// The version of the manifest this writer last read or wrote.
#[derive(Debug, Default)]
pub struct ManifestFence {
    seen: Mutex<Seen>,
}

#[derive(Debug, Default, Clone)]
enum Seen {
    /// Never read: a put is unconditional (a tool, not a writer).
    #[default]
    Unknown,
    /// Read and there was none.
    Absent,
    /// Read or written at this version.
    At(String),
}

impl ManifestFence {
    /// Records the version a read saw (`None`: no manifest).
    pub fn saw(&self, version: Option<String>) {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) =
            version.map_or(Seen::Absent, Seen::At);
    }

    /// Replaces the manifest iff it is still the version this writer saw,
    /// then remembers the new version. [`fenced`] if it moved on.
    pub fn commit<C: ConditionalObjects + ?Sized>(
        &self,
        objects: &C,
        key: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let new = match seen {
            Seen::Unknown => {
                // No fence to hold: replace whatever is there.
                let current = objects.get_versioned(key)?;
                match current {
                    Some((_, v)) => objects.put_if_match(key, bytes, &v)?,
                    None => objects.put_if_absent(key, bytes)?,
                }
            }
            Seen::Absent => objects.put_if_absent(key, bytes)?,
            Seen::At(v) => objects.put_if_match(key, bytes, &v)?,
        };
        let version = new.ok_or_else(|| {
            fenced("another writer committed to this table since this one read its manifest")
        })?;
        self.saw(Some(version));
        Ok(())
    }
}
