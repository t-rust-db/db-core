//! The S3 client against an in-process fake that speaks enough S3 (object
//! GET/PUT/HEAD/DELETE, v2 listing, conditional puts) to test what a real
//! Garage cannot: the conditional-put paths. The wire format and the
//! signatures are checked against a real server by the `s3_garage` tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::super::conformance;
use super::super::Store;
use super::sha256::{hex, sha256};
use super::*;

/// The parts of one upload by part number.
type Parts = BTreeMap<usize, Vec<u8>>;
/// Uploads in progress: id -> (key, parts).
type Uploads = BTreeMap<String, (String, Parts)>;

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
    /// Multipart uploads in progress: id -> (key, parts by number).
    uploads: Mutex<Uploads>,
    next_upload: AtomicU64,
    /// Keys per listing page; 0 is no limit.
    page_size: AtomicUsize,
    /// Whether uploading a part is refused (403).
    deny_parts: AtomicBool,
}

/// The value of query parameter `name`, decoded.
fn param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name))
        .map(|(_, v)| percent_decode(v))
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
        if key == "forbidden" {
            return Ok(response(403, &[], xml_error("AccessDenied")));
        }
        let mut objects = self.objects.lock().unwrap();
        match req.method.as_str() {
            "POST" if query.starts_with("uploads") => {
                let id = format!("u{}", self.next_upload.fetch_add(1, Ordering::SeqCst));
                self.uploads
                    .lock()
                    .unwrap()
                    .insert(id.clone(), (key, BTreeMap::new()));
                let body = format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>");
                Ok(response(200, &[], body.into_bytes()))
            }
            "POST" if param(query, "uploadId").is_some() => {
                let id = param(query, "uploadId").unwrap();
                if self.conditional
                    && header("if-none-match") == Some("*")
                    && objects.contains_key(&key)
                {
                    return Ok(response(412, &[], xml_error("PreconditionFailed")));
                }
                let (_, parts) = self.uploads.lock().unwrap().remove(&id).unwrap();
                let whole: Vec<u8> = parts.into_values().flatten().collect();
                let etag = self.etag();
                objects.insert(key, (whole, etag));
                Ok(response(
                    200,
                    &[],
                    b"<CompleteMultipartUploadResult/>".to_vec(),
                ))
            }
            "PUT" if param(query, "partNumber").is_some() => {
                if self.deny_parts.load(Ordering::SeqCst) {
                    return Ok(response(403, &[], xml_error("AccessDenied")));
                }
                let number: usize = param(query, "partNumber").unwrap().parse().unwrap();
                let id = param(query, "uploadId").unwrap();
                self.uploads
                    .lock()
                    .unwrap()
                    .get_mut(&id)
                    .unwrap()
                    .1
                    .insert(number, req.body.clone());
                Ok(response(
                    200,
                    &[("etag", format!("\"p{number}\""))],
                    Vec::new(),
                ))
            }
            "DELETE" if param(query, "uploadId").is_some() => {
                self.uploads
                    .lock()
                    .unwrap()
                    .remove(&param(query, "uploadId").unwrap());
                Ok(response(204, &[], Vec::new()))
            }
            "GET" if query.contains("list-type=2") => {
                let prefix = param(query, "prefix").unwrap_or_default();
                let after = param(query, "continuation-token");
                let page = self.page_size.load(Ordering::SeqCst);
                let mut keys: Vec<&String> = objects
                    .keys()
                    .filter(|k| k.starts_with(&prefix) && after.as_ref().is_none_or(|a| *k > a))
                    .collect();
                let truncated = page > 0 && keys.len() > page;
                if truncated {
                    keys.truncate(page);
                }
                let mut body = String::from("<ListBucketResult>");
                for k in &keys {
                    body.push_str(&format!("<Contents><Key>{k}</Key></Contents>"));
                }
                if let (true, Some(last)) = (truncated, keys.last()) {
                    body.push_str(&format!("<IsTruncated>true</IsTruncated><NextContinuationToken>{last}</NextContinuationToken>"));
                } else {
                    body.push_str("<IsTruncated>false</IsTruncated>");
                }
                body.push_str("</ListBucketResult>");
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

/// Passes requests on to a [`FakeS3`] and keeps their headers.
struct Headers {
    s3: Arc<FakeS3>,
    seen: Mutex<Vec<Vec<(String, String)>>>,
}

impl Transport for Headers {
    fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse> {
        self.seen.lock().unwrap().push(request.headers.clone());
        self.s3.send(request)
    }
}

#[test]
fn every_request_is_signed_with_the_sources_current_credentials() {
    let env = |name: &str| match name {
        "AWS_ACCESS_KEY_ID" => Some("AKTEST".to_string()),
        "AWS_SECRET_ACCESS_KEY" => Some("sk".to_string()),
        "AWS_SESSION_TOKEN" => Some("session".to_string()),
        _ => None,
    };
    let source =
        CredentialSource::chain_with(env, |_| None, Arc::new(PlainHttp::default())).unwrap();
    let mut cfg = config(CommitMode::SingleWriter);
    cfg.credentials = source;
    let t = Arc::new(Headers {
        s3: FakeS3::new(false),
        seen: Mutex::new(Vec::new()),
    });
    let s = S3Store::with_transport(cfg, Arc::clone(&t) as Arc<dyn Transport>).unwrap();
    s.put("k", b"v").unwrap();
    let seen = t.seen.lock().unwrap();
    let headers = seen.last().unwrap();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("x-amz-security-token"), Some("session"));
    assert!(get("authorization").unwrap().contains("Credential=AKTEST/"));
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

#[test]
fn the_contract_holds_through_the_store_enum() {
    // The `Store::S3` and `StoreFile::S3` delegations, not just `S3Store`.
    let fake = FakeS3::new(false);
    conformance::run(&Store::S3(store(&fake, CommitMode::Auto)));
}

fn multipart_store(fake: &Arc<FakeS3>) -> S3Store {
    let mut cfg = config(CommitMode::Auto);
    cfg.multipart_threshold = 10;
    cfg.part_size = 5;
    S3Store::with_transport(cfg, Arc::clone(fake) as Arc<dyn Transport>).unwrap()
}

fn local_file(tag: &str, bytes: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("db-core-s3-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f");
    std::fs::write(&file, bytes).unwrap();
    (dir, file)
}

#[test]
fn a_large_file_is_uploaded_in_parts_and_assembled_in_order() {
    for conditional in [false, true] {
        let fake = FakeS3::new(conditional);
        let s = multipart_store(&fake);
        let bytes: Vec<u8> = (0u8..23).collect();
        let (dir, file) = local_file("mp", &bytes);
        // Opening the store probed it with two PUTs.
        let before = fake.count("PUT");
        s.publish(&file, "data/big").unwrap();
        assert!(!file.exists());
        assert_eq!(s.get("data/big").unwrap().as_deref(), Some(&bytes[..]));
        assert!(
            fake.uploads.lock().unwrap().is_empty(),
            "no upload left open"
        );
        // 23 bytes in 5-byte parts: five of them, and no other PUT.
        assert_eq!(fake.count("PUT") - before, 5);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn a_large_file_under_a_taken_name_is_a_retry_or_another_writers() {
    let fake = FakeS3::new(true);
    let s = multipart_store(&fake);
    let bytes: Vec<u8> = (0u8..23).collect();
    let (dir, file) = local_file("mp-taken", &bytes);
    s.publish(&file, "data/big").unwrap();
    // The same size is a retry of this very publish.
    std::fs::write(&file, &bytes).unwrap();
    s.publish(&file, "data/big").unwrap();
    assert!(!file.exists());
    // A different size is someone else's file.
    std::fs::write(&file, vec![9u8; 30]).unwrap();
    let err = s.publish(&file, "data/big").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(s.get("data/big").unwrap().as_deref(), Some(&bytes[..]));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_refused_part_aborts_the_upload() {
    let fake = FakeS3::new(false);
    let s = multipart_store(&fake);
    fake.deny_parts.store(true, Ordering::SeqCst);
    let (dir, file) = local_file("mp-deny", &[7u8; 23]);
    let err = s.publish(&file, "data/big").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert!(
        fake.uploads.lock().unwrap().is_empty(),
        "the upload was aborted"
    );
    assert_eq!(s.get("data/big").unwrap(), None);
    assert!(file.exists(), "the local file is kept for a retry");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_listing_follows_its_continuation_tokens() {
    let fake = FakeS3::new(false);
    let s = store(&fake, CommitMode::SingleWriter);
    for k in ["data/e", "data/a", "data/d", "data/b", "data/c"] {
        s.put(k, b"x").unwrap();
    }
    fake.page_size.store(2, Ordering::SeqCst);
    let keys = s.list("data/").unwrap();
    assert_eq!(keys, ["data/a", "data/b", "data/c", "data/d", "data/e"]);
    // Three pages of at most two keys.
    assert_eq!(fake.count("GET"), 3);
}

#[test]
fn a_forbidden_request_is_a_permission_error_with_the_s3_code() {
    let fake = FakeS3::new(false);
    let s = store(&fake, CommitMode::SingleWriter);
    let err = s.get("forbidden").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("AccessDenied"), "{err}");
}

#[test]
fn an_unusable_endpoint_is_refused_up_front_and_an_unreachable_one_fails_requests() {
    let mut cfg = config(CommitMode::SingleWriter);
    cfg.endpoint = "ftp://nowhere".to_string();
    assert_eq!(
        S3Store::new(cfg).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );

    // Nothing listens on port 1: the request fails, after its retries.
    let mut cfg = config(CommitMode::SingleWriter);
    cfg.endpoint = "http://127.0.0.1:1".to_string();
    let s = S3Store::new(cfg).unwrap();
    assert!(s.get("k").is_err());
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn creds() -> CredentialSource {
    config(CommitMode::Auto).credentials
}

#[test]
fn a_uri_names_bucket_prefix_and_options() {
    let cfg = S3Config::from_uri_with(
        "s3://bkt/tables/events/?endpoint=http://127.0.0.1:3900&region=garage&commit=single-writer",
        no_env,
        creds(),
    )
    .unwrap();
    assert_eq!(cfg.bucket, "bkt");
    assert_eq!(cfg.prefix, "tables/events");
    assert_eq!(cfg.endpoint, "http://127.0.0.1:3900");
    assert_eq!(cfg.region, "garage");
    assert!(cfg.path_style, "a custom endpoint is addressed path-style");
    assert_eq!(cfg.commit, CommitMode::SingleWriter);

    // Percent-escapes in a value, and no prefix.
    let cfg = S3Config::from_uri_with(
        "s3://bkt?endpoint=http%3A%2F%2Fminio%3A9000&path_style=false&commit=conditional-put",
        no_env,
        creds(),
    )
    .unwrap();
    assert_eq!(
        (cfg.prefix.as_str(), cfg.endpoint.as_str()),
        ("", "http://minio:9000")
    );
    assert!(!cfg.path_style);
    assert_eq!(cfg.commit, CommitMode::ConditionalPut);
}

#[test]
fn what_a_uri_leaves_out_comes_from_the_environment_or_is_aws() {
    // Nothing at all: AWS S3 in us-east-1, virtual-host style.
    let cfg = S3Config::from_uri_with("s3://bkt/p", no_env, creds()).unwrap();
    assert_eq!(cfg.endpoint, "https://s3.us-east-1.amazonaws.com");
    assert_eq!(cfg.region, "us-east-1");
    assert!(!cfg.path_style);
    assert_eq!(cfg.commit, CommitMode::Auto);

    let env = |name: &str| match name {
        "AWS_DEFAULT_REGION" => Some("eu-west-1".to_string()),
        _ => None,
    };
    let cfg = S3Config::from_uri_with("s3://bkt", env, creds()).unwrap();
    assert_eq!(cfg.endpoint, "https://s3.eu-west-1.amazonaws.com");

    // AWS_REGION beats AWS_DEFAULT_REGION; AWS_ENDPOINT_URL is a custom endpoint.
    let env = |name: &str| match name {
        "AWS_REGION" => Some("garage".to_string()),
        "AWS_DEFAULT_REGION" => Some("eu-west-1".to_string()),
        "AWS_ENDPOINT_URL" => Some("http://localhost:3900".to_string()),
        _ => None,
    };
    let cfg = S3Config::from_uri_with("s3://bkt", env, creds()).unwrap();
    assert_eq!(
        (cfg.region.as_str(), cfg.endpoint.as_str()),
        ("garage", "http://localhost:3900")
    );
    assert!(cfg.path_style);
    // The URI beats the environment.
    let cfg = S3Config::from_uri_with(
        "s3://bkt?region=us-west-2&endpoint=http://x:1",
        env,
        creds(),
    )
    .unwrap();
    assert_eq!(
        (cfg.region.as_str(), cfg.endpoint.as_str()),
        ("us-west-2", "http://x:1")
    );
}

#[test]
fn a_bad_uri_is_refused_with_the_reason() {
    for (uri, why) in [
        ("https://bkt/p", "not an s3:// URI"),
        ("s3://", "no bucket"),
        ("s3:///prefix", "no bucket"),
        ("s3://bkt?region=a&typo=1", "unknown option"),
        ("s3://bkt?path_style=yes", "path_style"),
        ("s3://bkt?commit=fast", "commit"),
    ] {
        let err = S3Config::from_uri_with(uri, no_env, creds()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{uri}");
        assert!(err.to_string().contains(why), "{uri}: {err}");
    }
}
