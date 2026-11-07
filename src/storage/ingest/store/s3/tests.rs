//! The S3 client against an in-process fake that speaks enough S3 (object
//! GET/PUT/HEAD/DELETE, v2 listing, conditional puts) to test what a real
//! Garage cannot: the conditional-put paths. The wire format and the
//! signatures are checked against a real server by the `s3_garage` tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::super::conformance;
use super::sha256::{hex, sha256};
use super::*;

/// A tiny in-memory S3 bucket.
#[derive(Default)]
struct FakeS3 {
    objects: Mutex<BTreeMap<String, (Vec<u8>, u64)>>,
    etags: AtomicU64,
    /// Whether `If-None-Match` / `If-Match` are honoured (AWS, MinIO) or
    /// ignored (Garage).
    conditional: bool,
    /// Requests seen: method, host, target.
    seen: Mutex<Vec<(String, String, String)>>,
    /// 5xx answers still to give before behaving.
    flaky: AtomicU64,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b, hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn xml_error(code: &str) -> Vec<u8> {
    format!("<Error><Code>{code}</Code><Message>fake</Message></Error>").into_bytes()
}

fn response(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        body,
    }
}

impl FakeS3 {
    fn new(conditional: bool) -> Arc<FakeS3> {
        Arc::new(FakeS3 {
            conditional,
            ..FakeS3::default()
        })
    }

    fn etag(&self) -> u64 {
        self.etags.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn count(&self, method: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == method)
            .count()
    }
}

impl Transport for FakeS3 {
    fn send(&self, req: &HttpRequest) -> io::Result<HttpResponse> {
        let host = req.endpoint.host_header();
        self.seen
            .lock()
            .unwrap()
            .push((req.method.clone(), host, req.target.clone()));
        if self.flaky.load(Ordering::SeqCst) > 0 {
            self.flaky.fetch_sub(1, Ordering::SeqCst);
            return Ok(response(503, &[], xml_error("SlowDown")));
        }
        // Every request is signed, with the body's real hash.
        let header = |name: &str| {
            req.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let auth = header("authorization").unwrap_or_default();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKTEST/"),
            "{auth}"
        );
        assert_eq!(
            header("x-amz-content-sha256"),
            Some(hex(&sha256(&req.body)).as_str())
        );

        let (path, query) = req.target.split_once('?').unwrap_or((&req.target, ""));
        let path = percent_decode(path);
        // Path-style: /bucket/key; virtual-host: /key.
        let key = path
            .strip_prefix("/bkt/")
            .or_else(|| path.strip_prefix('/').filter(|_| !path.starts_with("/bkt")))
            .unwrap_or_default()
            .to_string();
        let mut objects = self.objects.lock().unwrap();
        match req.method.as_str() {
            "GET" if query.contains("list-type=2") => {
                let prefix = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("prefix="))
                    .map(percent_decode)
                    .unwrap_or_default();
                let mut body = String::from("<ListBucketResult>");
                for k in objects.keys().filter(|k| k.starts_with(&prefix)) {
                    body.push_str(&format!("<Contents><Key>{k}</Key></Contents>"));
                }
                body.push_str("<IsTruncated>false</IsTruncated></ListBucketResult>");
                Ok(response(200, &[], body.into_bytes()))
            }
            "GET" | "HEAD" => match objects.get(&key) {
                None => Ok(response(404, &[], xml_error("NoSuchKey"))),
                Some((bytes, etag)) => {
                    let mut headers = vec![
                        ("etag", format!("\"{etag}\"")),
                        ("content-length", bytes.len().to_string()),
                    ];
                    if req.method == "HEAD" {
                        return Ok(response(200, &headers, Vec::new()));
                    }
                    if let Some(range) = header("range").and_then(|r| r.strip_prefix("bytes=")) {
                        let (from, to) = range.split_once('-').unwrap_or_default();
                        let from: usize = from.parse().unwrap_or(0);
                        let to: usize = to.parse().unwrap_or(bytes.len() - 1);
                        headers[1] = ("content-length", (to + 1 - from).to_string());
                        let part = bytes.get(from..=to).unwrap_or_default().to_vec();
                        return Ok(response(206, &headers, part));
                    }
                    Ok(response(200, &headers, bytes.clone()))
                }
            },
            "PUT" => {
                if self.conditional {
                    let exists = objects.get(&key);
                    if header("if-none-match") == Some("*") && exists.is_some() {
                        return Ok(response(412, &[], xml_error("PreconditionFailed")));
                    }
                    if let Some(want) = header("if-match") {
                        match exists {
                            None => return Ok(response(404, &[], xml_error("NoSuchKey"))),
                            Some((_, etag)) if format!("\"{etag}\"") != want => {
                                return Ok(response(412, &[], xml_error("PreconditionFailed")))
                            }
                            Some(_) => {}
                        }
                    }
                }
                let etag = self.etag();
                objects.insert(key, (req.body.clone(), etag));
                Ok(response(
                    200,
                    &[("etag", format!("\"{etag}\""))],
                    Vec::new(),
                ))
            }
            "DELETE" => {
                objects.remove(&key);
                Ok(response(204, &[], Vec::new()))
            }
            other => panic!("the fake does not speak {other}"),
        }
    }
}

fn config(commit: CommitMode) -> S3Config {
    let mut cfg = S3Config::new(
        "http://s3.test:9000",
        "us-east-1",
        "bkt",
        Credentials {
            access_key: "AKTEST".to_string(),
            secret_key: "secret".to_string(),
            session_token: None,
        },
    );
    cfg.commit = commit;
    cfg
}

fn store(fake: &Arc<FakeS3>, commit: CommitMode) -> S3Store {
    S3Store::with_transport(config(commit), Arc::clone(fake) as Arc<dyn Transport>).unwrap()
}

#[test]
fn the_client_meets_the_table_store_contract() {
    for conditional in [false, true] {
        let fake = FakeS3::new(conditional);
        conformance::run(&store(&fake, CommitMode::Auto));
    }
}

#[test]
fn a_prefix_scopes_every_key_and_listing_strips_it() {
    let fake = FakeS3::new(false);
    let mut cfg = config(CommitMode::SingleWriter);
    cfg.prefix = "tables/events/".to_string();
    let s = S3Store::with_transport(cfg, Arc::clone(&fake) as Arc<dyn Transport>).unwrap();
    s.put("data/a", b"1").unwrap();
    let keys: Vec<String> = fake.objects.lock().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["tables/events/data/a"]);
    assert_eq!(s.list("").unwrap(), ["data/a"]);
    assert_eq!(s.list("data/").unwrap(), ["data/a"]);
}

#[test]
fn virtual_host_addressing_puts_the_bucket_in_the_host() {
    let fake = FakeS3::new(false);
    let mut cfg = config(CommitMode::SingleWriter);
    cfg.path_style = false;
    let s = S3Store::with_transport(cfg, Arc::clone(&fake) as Arc<dyn Transport>).unwrap();
    s.put("k", b"v").unwrap();
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(
        seen.last().unwrap(),
        &(
            "PUT".to_string(),
            "bkt.s3.test:9000".to_string(),
            "/k".to_string()
        )
    );
}

#[test]
fn the_probe_tells_a_store_with_conditional_puts_from_one_without() {
    let aws = FakeS3::new(true);
    assert!(store(&aws, CommitMode::Auto).is_conditional());
    assert!(store(&aws, CommitMode::ConditionalPut).is_conditional());
    assert!(!store(&aws, CommitMode::SingleWriter).is_conditional());
    // The probe cleans up after itself.
    assert!(aws.objects.lock().unwrap().is_empty());

    let garage = FakeS3::new(false);
    assert!(!store(&garage, CommitMode::Auto).is_conditional());
    let err = S3Store::with_transport(
        config(CommitMode::ConditionalPut),
        Arc::clone(&garage) as Arc<dyn Transport>,
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn the_manifest_is_fenced_against_a_writer_that_read_an_older_one() {
    let aws = FakeS3::new(true);
    let (a, b) = (store(&aws, CommitMode::Auto), store(&aws, CommitMode::Auto));
    assert_eq!(a.get(MANIFEST_FILE).unwrap(), None);
    a.put(MANIFEST_FILE, b"a1").unwrap();
    a.put(MANIFEST_FILE, b"a2").unwrap(); // its own write moves its fence on
    assert_eq!(b.get(MANIFEST_FILE).unwrap().as_deref(), Some(&b"a2"[..]));
    b.put(MANIFEST_FILE, b"b1").unwrap();
    // A last saw a2; B replaced it.
    let err = a.put(MANIFEST_FILE, b"a3").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(b.get(MANIFEST_FILE).unwrap().as_deref(), Some(&b"b1"[..]));
}

#[test]
fn two_writers_creating_the_first_manifest_cannot_both_win() {
    let aws = FakeS3::new(true);
    let (a, b) = (store(&aws, CommitMode::Auto), store(&aws, CommitMode::Auto));
    assert_eq!(a.get(MANIFEST_FILE).unwrap(), None);
    assert_eq!(b.get(MANIFEST_FILE).unwrap(), None);
    a.put(MANIFEST_FILE, b"a").unwrap();
    assert_eq!(
        b.put(MANIFEST_FILE, b"b").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn the_writer_lease_is_exclusive_until_released() {
    let aws = FakeS3::new(true);
    let (a, b) = (store(&aws, CommitMode::Auto), store(&aws, CommitMode::Auto));
    let first = a
        .lease("a")
        .unwrap()
        .expect("a store with conditional puts leases");
    let err = b.lease("b").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    drop(first);
    assert!(b.lease("b").unwrap().is_some());
    // Garage: no lease.
    let garage = FakeS3::new(false);
    assert!(store(&garage, CommitMode::Auto)
        .lease("x")
        .unwrap()
        .is_none());
}

#[test]
fn publish_never_replaces_a_different_object_but_accepts_its_own_retry() {
    let aws = FakeS3::new(true);
    let s = store(&aws, CommitMode::Auto);
    let dir = std::env::temp_dir().join(format!("db-core-s3-pub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f");
    std::fs::write(&file, b"mine").unwrap();
    s.publish(&file, "data/f").unwrap();
    // A retry of the same bytes is fine.
    std::fs::write(&file, b"mine").unwrap();
    s.publish(&file, "data/f").unwrap();
    // Another writer's different bytes under the same name are refused,
    // and the object is untouched.
    std::fs::write(&file, b"theirs").unwrap();
    let err = s.publish(&file, "data/f").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(s.get("data/f").unwrap().as_deref(), Some(&b"mine"[..]));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_5xx_is_retried_and_a_persistent_one_is_an_error() {
    let fake = FakeS3::new(false);
    let s = store(&fake, CommitMode::SingleWriter);
    fake.flaky.store(2, Ordering::SeqCst);
    s.put("k", b"v").unwrap();
    assert_eq!(s.get("k").unwrap().as_deref(), Some(&b"v"[..]));
    fake.flaky.store(10, Ordering::SeqCst);
    let err = s.put("k2", b"v").unwrap_err();
    assert!(err.to_string().contains("503"), "{err}");
    fake.flaky.store(0, Ordering::SeqCst);
}

#[test]
fn a_ranged_read_fetches_only_the_range() {
    let fake = FakeS3::new(false);
    let s = store(&fake, CommitMode::SingleWriter);
    s.put("o", b"0123456789").unwrap();
    let f = s.open("o").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(f.read_at(3, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"3456");
    assert_eq!(f.read_at(8, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"89");
    assert_eq!(f.read_at(10, &mut buf).unwrap(), 0);
    // One HEAD to open, then a GET per read that reaches the object.
    assert_eq!(fake.count("HEAD"), 1);
}
