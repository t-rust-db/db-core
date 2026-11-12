//! An S3-compatible object store (AWS S3, Garage, MinIO) as a table's
//! store (#1015). Std only; see the `storage-s3` feature.
//!
//! The protocol is here (SigV4, requests, listing, multipart upload,
//! conditional puts); the bytes travel over a [`Transport`]. [`PlainHttp`]
//! speaks `http://`, which is what Garage and MinIO on a private network
//! need. AWS S3 is HTTPS only, so against AWS the embedding application
//! supplies a TLS [`Transport`] to [`S3Store::with_transport`].
//!
//! Commit modes. A manifest commit is one PUT, which an object store makes
//! atomic. Whether a *second* writer can be kept out depends on conditional
//! puts (`If-None-Match`, `If-Match`): AWS S3 and MinIO have them, Garage
//! ignores them. [`CommitMode::Auto`] probes the store when it is opened.
//! With conditional puts the manifest is fenced and the writer holds a lease
//! ([`lease`](super::lease)); without them the writer's local lock is the
//! only protection, which does not reach a second machine.

mod cache;
mod credentials;
pub use credentials::CredentialSource;
mod http;
mod sha256;
mod sigv4;
#[cfg(test)]
mod tests;
mod xml;

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use self::cache::RangeCache;
use super::lease::{publish_once, ConditionalObjects, Lease, ManifestFence};
use super::{check_key, TableStore};
use crate::storage::column::{MmapRegion, VfsFile};
use crate::storage::ingest::manifest::MANIFEST_FILE;
pub use http::{exchange, Endpoint, HttpRequest, HttpResponse, PlainHttp, Transport};
use sha256::{hex, sha256};
pub use sigv4::Credentials;
use sigv4::{amz_date, authorization, canonical_query, uri_encode, SignInput};

/// Attempts for a request that is safe to repeat.
const ATTEMPTS: u64 = 3;

/// Whether the store's conditional puts are used to keep a second writer out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMode {
    /// Probe the store when it is opened: conditional puts if it honours
    /// them, else [`CommitMode::SingleWriter`].
    Auto,
    /// Trust the writer's local lock alone (Garage).
    SingleWriter,
    /// Fence the manifest and hold a lease; fail to open a store that does
    /// not honour conditional puts.
    ConditionalPut,
}

/// Where the table's objects are, and how to reach them.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// `http://host[:port]` or `https://host[:port]`.
    pub endpoint: String,
    /// The signing region (`garage`, `us-east-1`, ...).
    pub region: String,
    /// The bucket.
    pub bucket: String,
    /// The key prefix the table lives under (none if empty).
    pub prefix: String,
    /// `/bucket/key` addressing (Garage, MinIO) rather than `bucket.host`.
    pub path_style: bool,
    /// What signs the requests: fixed credentials or a role's.
    pub credentials: CredentialSource,
    /// How a second writer is kept out.
    pub commit: CommitMode,
    /// How long a writer lease lasts without renewal.
    pub lease_ttl: Duration,
    /// Files of at least this many bytes are uploaded in parts (64 MiB).
    pub multipart_threshold: u64,
    /// The size of one part (16 MiB; S3 wants at least 5 MiB, but the last).
    pub part_size: usize,
    /// The bytes of column chunks and footers a query read that the store
    /// keeps for the next query, least recently used out first (256 MiB;
    /// 0 keeps none). A table's files never change, so a kept range never
    /// goes stale (#1052, ADR 0045).
    pub read_cache_bytes: u64,
}

impl S3Config {
    /// Path-style addressing, no prefix, [`CommitMode::Auto`], a 30 s lease.
    pub fn new(
        endpoint: impl Into<String>,
        region: impl Into<String>,
        bucket: impl Into<String>,
        credentials: impl Into<CredentialSource>,
    ) -> S3Config {
        S3Config {
            endpoint: endpoint.into(),
            region: region.into(),
            bucket: bucket.into(),
            prefix: String::new(),
            path_style: true,
            credentials: credentials.into(),
            commit: CommitMode::Auto,
            lease_ttl: Duration::from_secs(30),
            multipart_threshold: 64 << 20,
            part_size: 16 << 20,
            read_cache_bytes: 256 << 20,
        }
    }
}

/// `%XX` decoded; anything that is not an escape stays.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let escaped = (b == b'%')
            .then(|| bytes.get(i.saturating_add(1)..i.saturating_add(3)))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match escaped {
            Some(v) => {
                out.push(v);
                i = i.saturating_add(3);
            }
            None => {
                out.push(b);
                i = i.saturating_add(1);
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl S3Config {
    /// Parses `s3://bucket[/prefix][?option=value&...]`, signing with
    /// `credentials`. The options are `endpoint=URL`, `region=NAME`,
    /// `path_style=true|false` and `commit=auto|single-writer|conditional-put`
    /// (a typo is an error, not ignored).
    ///
    /// What the URI leaves out comes from the environment, like the AWS
    /// tools: the endpoint from `AWS_ENDPOINT_URL`, the region from
    /// `AWS_REGION` or `AWS_DEFAULT_REGION` (else `us-east-1`). With no
    /// endpoint at all the store is AWS S3, addressed virtual-host style; a
    /// custom endpoint (Garage, MinIO) is addressed path-style.
    pub fn from_uri(uri: &str, credentials: impl Into<CredentialSource>) -> io::Result<S3Config> {
        S3Config::from_uri_with(uri, |name| std::env::var(name).ok(), credentials)
    }

    /// [`S3Config::from_uri`] over any way of looking a variable up.
    pub fn from_uri_with(
        uri: &str,
        get: impl Fn(&str) -> Option<String>,
        credentials: impl Into<CredentialSource>,
    ) -> io::Result<S3Config> {
        let bad =
            |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("{uri:?}: {why}"));
        let rest = uri
            .strip_prefix("s3://")
            .ok_or_else(|| bad("not an s3:// URI"))?;
        let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
        if bucket.is_empty() {
            return Err(bad("no bucket"));
        }
        let (mut endpoint, mut region, mut path_style, mut commit) = (None, None, None, None);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode(value);
            match name {
                "endpoint" => endpoint = Some(value),
                "region" => region = Some(value),
                "path_style" => {
                    path_style = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err(bad("path_style is true or false")),
                    });
                }
                "commit" => {
                    commit = Some(match value.as_str() {
                        "auto" => CommitMode::Auto,
                        "single-writer" => CommitMode::SingleWriter,
                        "conditional-put" => CommitMode::ConditionalPut,
                        _ => return Err(bad("commit is auto, single-writer or conditional-put")),
                    });
                }
                other => return Err(bad(&format!("unknown option {other:?}"))),
            }
        }
        let region = region
            .or_else(|| get("AWS_REGION"))
            .or_else(|| get("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".to_string());
        let custom = endpoint
            .or_else(|| get("AWS_ENDPOINT_URL"))
            .filter(|e| !e.is_empty());
        let path_style = path_style.unwrap_or(custom.is_some());
        let endpoint = custom.unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        let mut cfg = S3Config::new(endpoint, region, bucket, credentials);
        cfg.prefix = prefix.trim_matches('/').to_string();
        cfg.path_style = path_style;
        if let Some(commit) = commit {
            cfg.commit = commit;
        }
        Ok(cfg)
    }
}

/// Signs and sends requests.
struct Client {
    cfg: S3Config,
    endpoint: Endpoint,
    transport: Arc<dyn Transport>,
    fence: ManifestFence,
    /// What held files read, for the next query (#1052).
    ranges: RangeCache,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// A status a caller is ready for; anything else is an error.
fn status_error(resp: &HttpResponse) -> io::Error {
    let body = String::from_utf8_lossy(&resp.body);
    let code = xml::first(&body, "Code").unwrap_or_default();
    let message = xml::first(&body, "Message").unwrap_or_default();
    let kind = match resp.status {
        401 | 403 => io::ErrorKind::PermissionDenied,
        404 => io::ErrorKind::NotFound,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        format!("S3 answered {} {code}: {message}", resp.status),
    )
}

impl Client {
    /// The object key under the table's prefix.
    fn full_key(&self, key: &str) -> String {
        let prefix = self.cfg.prefix.trim_matches('/');
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}/{key}")
        }
    }

    /// One signed request. `key` of `None` addresses the bucket.
    fn call(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        extra: &[(String, String)],
        body: &[u8],
    ) -> io::Result<HttpResponse> {
        let object = key.map(|k| self.full_key(k));
        let (endpoint, path) = if self.cfg.path_style {
            let path = match &object {
                Some(o) => format!("/{}/{o}", self.cfg.bucket),
                None => format!("/{}", self.cfg.bucket),
            };
            (self.endpoint.clone(), path)
        } else {
            let endpoint = Endpoint {
                host: format!("{}.{}", self.cfg.bucket, self.endpoint.host),
                ..self.endpoint.clone()
            };
            (endpoint, format!("/{}", object.unwrap_or_default()))
        };
        let path = uri_encode(&path, true);
        let amz = amz_date(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        );
        let payload_hash = hex(&sha256(body));
        let mut headers = vec![
            ("host".to_string(), endpoint.host_header()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz.clone()),
        ];
        let credentials = self.cfg.credentials.current()?;
        if let Some(token) = &credentials.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        headers.extend(
            extra
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone())),
        );
        let auth = authorization(
            &credentials,
            &SignInput {
                method,
                path: &path,
                query,
                headers: &headers,
                payload_hash: &payload_hash,
                amz_date: &amz,
                region: &self.cfg.region,
            },
        );
        headers.retain(|(k, _)| k != "host");
        headers.push(("authorization".to_string(), auth));
        let cq = canonical_query(query);
        let target = if cq.is_empty() {
            path
        } else {
            format!("{path}?{cq}")
        };
        self.transport.send(&HttpRequest {
            method: method.to_string(),
            endpoint,
            target,
            headers,
            body: body.to_vec(),
        })
    }

    /// [`Client::call`] again on a connection error or a 5xx, for requests
    /// that are safe to repeat.
    fn call_retrying(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        extra: &[(String, String)],
        body: &[u8],
    ) -> io::Result<HttpResponse> {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100u64.saturating_mul(attempt)));
            }
            match self.call(method, key, query, extra, body) {
                Ok(resp) if resp.status >= 500 => last = Some(Ok(resp)),
                Ok(resp) => return Ok(resp),
                Err(e) => last = Some(Err(e)),
            }
        }
        last.unwrap_or_else(|| Err(io::Error::other("no attempt was made")))
    }

    /// `resp` if its status is 2xx or one of `also`, else the S3 error.
    fn expect(resp: HttpResponse, also: &[u16]) -> io::Result<HttpResponse> {
        if (200..300).contains(&resp.status) || also.contains(&resp.status) {
            Ok(resp)
        } else {
            Err(status_error(&resp))
        }
    }

    fn etag(resp: &HttpResponse) -> io::Result<String> {
        resp.header("etag")
            .map(str::to_string)
            .ok_or_else(|| io::Error::other("S3 answered without an ETag"))
    }

    fn put_object(
        &self,
        key: &str,
        bytes: &[u8],
        condition: Option<(&str, &str)>,
    ) -> io::Result<HttpResponse> {
        let extra: Vec<(String, String)> = condition
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into_iter()
            .collect();
        // A conditional put is not repeated: a lost answer to one that
        // landed would read as a lost race.
        if condition.is_some() {
            self.call("PUT", Some(key), &[], &extra, bytes)
        } else {
            self.call_retrying("PUT", Some(key), &[], &extra, bytes)
        }
    }

    /// Creates or replaces the object conditionally: the new ETag, or
    /// `None` if the condition failed.
    fn conditional_put(
        &self,
        key: &str,
        bytes: &[u8],
        condition: (&str, &str),
    ) -> io::Result<Option<String>> {
        let resp = self.put_object(key, bytes, Some(condition))?;
        // 412 is the condition failing; 409 is AWS's answer to two
        // conditional writes racing; a missing object under `If-Match` is 404.
        if matches!(resp.status, 404 | 409 | 412) {
            return Ok(None);
        }
        Client::etag(&Client::expect(resp, &[])?).map(Some)
    }

    fn head_size(&self, key: &str) -> io::Result<Option<u64>> {
        let resp = Client::expect(
            self.call_retrying("HEAD", Some(key), &[], &[], &[])?,
            &[404],
        )?;
        if resp.status == 404 {
            return Ok(None);
        }
        resp.header("content-length")
            .and_then(|v| v.parse().ok())
            .map(Some)
            .ok_or_else(|| io::Error::other("S3 answered a HEAD without a content-length"))
    }

    fn get_whole(&self, key: &str) -> io::Result<Option<HttpResponse>> {
        let resp = Client::expect(self.call_retrying("GET", Some(key), &[], &[], &[])?, &[404])?;
        Ok((resp.status != 404).then_some(resp))
    }

    fn put_multipart(
        &self,
        local: &std::path::Path,
        key: &str,
        create_only: bool,
    ) -> io::Result<()> {
        let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };
        let started = Client::expect(
            self.call("POST", Some(key), &q(&[("uploads", "")]), &[], &[])?,
            &[],
        )?;
        let upload_id = xml::first(&String::from_utf8_lossy(&started.body), "UploadId")
            .ok_or_else(|| io::Error::other("S3 started no multipart upload"))?;
        let uploaded = self.upload_parts(local, key, &upload_id).and_then(|etags| {
            let mut body = String::from("<CompleteMultipartUpload>");
            for (n, etag) in etags.iter().enumerate() {
                body.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>",
                    n.saturating_add(1)
                ));
            }
            body.push_str("</CompleteMultipartUpload>");
            let extra: Vec<(String, String)> = if create_only {
                vec![("if-none-match".to_string(), "*".to_string())]
            } else {
                Vec::new()
            };
            let done = self.call(
                "POST",
                Some(key),
                &q(&[("uploadId", &upload_id)]),
                &extra,
                body.as_bytes(),
            )?;
            if done.status == 412 || done.status == 409 {
                return Err(super::lease::fenced(
                    "another writer already published an object under this name",
                ));
            }
            let done = Client::expect(done, &[])?;
            // S3 can answer 200 and put the failure in the body.
            if String::from_utf8_lossy(&done.body).contains("<Error>") {
                return Err(status_error(&done));
            }
            Ok(())
        });
        if uploaded.is_err() {
            self.call(
                "DELETE",
                Some(key),
                &q(&[("uploadId", &upload_id)]),
                &[],
                &[],
            )
            .ok();
        }
        uploaded
    }

    fn upload_parts(
        &self,
        local: &std::path::Path,
        key: &str,
        upload_id: &str,
    ) -> io::Result<Vec<String>> {
        let mut file = File::open(local)?;
        let mut etags = Vec::new();
        loop {
            let size = self.cfg.part_size.max(1);
            let mut part = Vec::with_capacity(size);
            (&mut file).take(size as u64).read_to_end(&mut part)?;
            if part.is_empty() {
                return Ok(etags);
            }
            let number = etags.len().saturating_add(1).to_string();
            let query = vec![
                ("partNumber".to_string(), number),
                ("uploadId".to_string(), upload_id.to_string()),
            ];
            let resp = Client::expect(
                self.call_retrying("PUT", Some(key), &query, &[], &part)?,
                &[],
            )?;
            etags.push(Client::etag(&resp)?);
        }
    }
}

impl ConditionalObjects for Client {
    fn get_versioned(&self, key: &str) -> io::Result<Option<(Vec<u8>, String)>> {
        let found = match self.get_whole(key)? {
            Some(resp) => {
                let etag = Client::etag(&resp)?;
                Some((resp.body, etag))
            }
            None => None,
        };
        if key == MANIFEST_FILE {
            self.fence.saw(found.as_ref().map(|(_, v)| v.clone()));
        }
        Ok(found)
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> io::Result<Option<String>> {
        self.conditional_put(key, bytes, ("if-none-match", "*"))
    }

    fn put_if_match(&self, key: &str, bytes: &[u8], version: &str) -> io::Result<Option<String>> {
        self.conditional_put(key, bytes, ("if-match", version))
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        Client::expect(
            self.call_retrying("DELETE", Some(key), &[], &[], &[])?,
            &[404],
        )
        .map(|_| ())
    }
}

/// A table's objects in a bucket.
#[derive(Debug, Clone)]
pub struct S3Store {
    client: Arc<Client>,
    conditional: bool,
}

impl S3Store {
    /// Opens the store over plain HTTP (see the module docs for HTTPS).
    pub fn new(cfg: S3Config) -> io::Result<S3Store> {
        S3Store::with_transport(cfg, Arc::new(PlainHttp::default()))
    }

    /// Opens the table at an `s3://bucket/prefix` URI (see
    /// [`S3Config::from_uri`]) with credentials from the environment or the
    /// shared credentials file, over plain HTTP.
    pub fn open_uri(uri: &str) -> io::Result<S3Store> {
        S3Store::new(S3Config::from_uri(uri, Credentials::from_chain()?)?)
    }

    /// Opens the store over `transport`. In [`CommitMode::Auto`] this probes
    /// the store for conditional puts with two small writes.
    pub fn with_transport(cfg: S3Config, transport: Arc<dyn Transport>) -> io::Result<S3Store> {
        let endpoint = Endpoint::parse(&cfg.endpoint)?;
        let mode = cfg.commit;
        let ranges = RangeCache::new(cfg.read_cache_bytes);
        let client = Arc::new(Client {
            cfg,
            endpoint,
            transport,
            fence: ManifestFence::default(),
            ranges,
        });
        let conditional = match mode {
            CommitMode::SingleWriter => false,
            CommitMode::Auto => probe_conditional(&client).unwrap_or(false),
            CommitMode::ConditionalPut => {
                if !probe_conditional(&client)? {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "this store ignores If-None-Match, so conditional-put commits \
                         cannot protect the table; use CommitMode::SingleWriter",
                    ));
                }
                true
            }
        };
        Ok(S3Store {
            client,
            conditional,
        })
    }

    /// Whether manifest commits are fenced and the writer takes a lease.
    pub fn is_conditional(&self) -> bool {
        self.conditional
    }

    /// The listed object `key` of `size` bytes (the manifest's), opened
    /// for a snapshot to hold: no HEAD, and its reads go through the read
    /// cache (#1052).
    pub(crate) fn open_held(&self, key: &str, size: u64) -> io::Result<S3File> {
        check_key(key)?;
        Ok(S3File {
            client: Arc::clone(&self.client),
            key: key.to_string(),
            size,
            cached: true,
        })
    }
}

/// Whether the store refuses a second create-only put of the same key.
fn probe_conditional(client: &Client) -> io::Result<bool> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let key = format!(".db-core-probe-{}-{nanos}", std::process::id());
    let outcome = (|| {
        if client.put_if_absent(&key, b"probe")?.is_none() {
            // Could not even create it: the store is not usable like this.
            return Ok(false);
        }
        // A store that honours the header refuses; one that ignores it
        // overwrites.
        Ok(client.put_if_absent(&key, b"probe")?.is_none())
    })();
    client.remove(&key).ok();
    outcome
}

/// An object opened for positioned reads: one ranged GET per read, or none
/// when a held file's range is still in the store's read cache.
#[derive(Debug)]
pub struct S3File {
    client: Arc<Client>,
    key: String,
    size: u64,
    /// Whether reads go through the store's read cache: a snapshot's held
    /// file does, a flush or a compaction reading a file once does not.
    cached: bool,
}

/// Copies the start of `from` into `buf`; the bytes copied.
fn copy_into(buf: &mut [u8], from: &[u8]) -> usize {
    let n = from.len().min(buf.len());
    if let (Some(to), Some(from)) = (buf.get_mut(..n), from.get(..n)) {
        to.copy_from_slice(from);
    }
    n
}

impl VfsFile for S3File {
    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let want = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        let last = offset.saturating_add(want).min(self.size).saturating_sub(1);
        let len = usize::try_from(last.saturating_sub(offset).saturating_add(1)).unwrap_or(0);
        if self.cached {
            if let Some(held) = self.client.ranges.get(&self.key, offset, len) {
                return Ok(copy_into(buf, &held));
            }
        }
        let range = format!("bytes={offset}-{last}");
        let resp = Client::expect(
            self.client.call_retrying(
                "GET",
                Some(&self.key),
                &[],
                &[("range".to_string(), range)],
                &[],
            )?,
            &[],
        )?;
        // A server that ignores Range sends the whole object.
        let body = if resp.status == 206 {
            resp.body.as_slice()
        } else {
            usize::try_from(offset)
                .ok()
                .and_then(|o| resp.body.get(o..))
                .unwrap_or_default()
        };
        let body = body.get(..len).unwrap_or(body);
        if self.cached && body.len() == len {
            self.client
                .ranges
                .insert(&self.key, offset, Arc::from(body));
        }
        Ok(copy_into(buf, body))
    }

    fn mmap(&self) -> io::Result<MmapRegion> {
        let resp = self
            .client
            .get_whole(&self.key)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(MmapRegion::Owned(Arc::from(resp.body)))
    }
}

impl TableStore for S3Store {
    type File = S3File;

    fn open(&self, key: &str) -> io::Result<S3File> {
        check_key(key)?;
        let size = self
            .client
            .head_size(key)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(S3File {
            client: Arc::clone(&self.client),
            key: key.to_string(),
            size,
            cached: false,
        })
    }

    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        check_key(key)?;
        Ok(self.client.get_versioned(key)?.map(|(bytes, _)| bytes))
    }

    fn map(&self, key: &str) -> io::Result<MmapRegion> {
        check_key(key)?;
        let resp = self
            .client
            .get_whole(key)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(MmapRegion::Owned(Arc::from(resp.body)))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        check_key(key)?;
        if self.conditional && key == MANIFEST_FILE {
            return self.client.fence.commit(&*self.client, key, bytes);
        }
        Client::expect(self.client.put_object(key, bytes, None)?, &[]).map(|_| ())
    }

    fn publish(&self, local: &std::path::Path, key: &str) -> io::Result<()> {
        check_key(key)?;
        let len = std::fs::metadata(local)?.len();
        if len >= self.client.cfg.multipart_threshold {
            if self.conditional {
                // Same name as an existing object: the same bytes (a retry)
                // by size, else another writer's file.
                match self.client.head_size(key)? {
                    Some(existing) if existing == len => return std::fs::remove_file(local),
                    Some(_) => {
                        return Err(super::lease::fenced(
                            "another writer already published a different object under this name",
                        ))
                    }
                    None => {}
                }
            }
            self.client.put_multipart(local, key, self.conditional)?;
        } else {
            let bytes = std::fs::read(local)?;
            if self.conditional {
                publish_once(&*self.client, key, &bytes)?;
            } else {
                Client::expect(self.client.put_object(key, &bytes, None)?, &[])?;
            }
        }
        std::fs::remove_file(local)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        check_key(key)?;
        self.client.remove(key)
    }

    fn lease(&self, owner: &str) -> io::Result<Option<Lease>> {
        if !self.conditional {
            return Ok(None);
        }
        Lease::acquire(Arc::clone(&self.client), owner, self.client.cfg.lease_ttl).map(Some)
    }

    fn remove_empty_dirs(&self, _prefix: &str) -> io::Result<()> {
        Ok(())
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let base = prefix.trim_end_matches('/');
        if !base.is_empty() {
            check_key(base)?;
        }
        let table = self.client.cfg.prefix.trim_matches('/');
        let mut under = String::new();
        for part in [table, base] {
            if !part.is_empty() {
                under.push_str(part);
                under.push('/');
            }
        }
        let strip = if table.is_empty() {
            String::new()
        } else {
            format!("{table}/")
        };
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), under.clone()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".to_string(), t.clone()));
            }
            let resp = Client::expect(
                self.client.call_retrying("GET", None, &query, &[], &[])?,
                &[],
            )?;
            let body = String::from_utf8_lossy(&resp.body).into_owned();
            keys.extend(
                xml::all(&body, "Key")
                    .into_iter()
                    .filter_map(|k| k.strip_prefix(strip.as_str()).map(str::to_string)),
            );
            if xml::first(&body, "IsTruncated").as_deref() != Some("true") {
                break;
            }
            token = xml::first(&body, "NextContinuationToken");
            if token.is_none() {
                break;
            }
        }
        keys.sort();
        Ok(keys)
    }
}
