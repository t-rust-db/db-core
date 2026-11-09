//! Where credentials come from, in the AWS SDKs' order: the environment,
//! the shared credentials file (`~/.aws/credentials`), the container
//! credentials endpoint (an ECS task role) and the EC2 instance metadata
//! service (IMDSv2, an instance role). A role's credentials are temporary:
//! a [`CredentialSource`] keeps them and fetches them again shortly before
//! they expire (#1042). Both endpoints are plain `http://` on a link-local
//! address, so [`PlainHttp`] reaches them.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::http::{Endpoint, HttpRequest, PlainHttp, Transport};
use super::sigv4::Credentials;
use crate::json_path::{parse_object, JsonValue};
use crate::storage::stream::timestamp::parse_iso8601;

/// The ECS container credentials endpoint, for a relative URI.
const CONTAINER_HOST: &str = "http://169.254.170.2";
/// The EC2 instance metadata service.
const IMDS_ENDPOINT: &str = "http://169.254.169.254";
/// Seconds before a role's credentials expire that they are fetched again.
const REFRESH_BEFORE: i64 = 300;
/// How long a role endpoint may take: it is on the host itself, and where
/// there is none (a laptop) the chain should give up quickly.
const ROLE_TIMEOUT: Duration = Duration::from_secs(2);

impl Credentials {
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
    /// `AWS_SESSION_TOKEN`; `None` if the first two are not both set.
    pub fn from_env() -> Option<Credentials> {
        Credentials::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Credentials::from_env`] over any way of looking a variable up.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Credentials> {
        let access_key = get("AWS_ACCESS_KEY_ID").filter(|v| !v.is_empty())?;
        let secret_key = get("AWS_SECRET_ACCESS_KEY").filter(|v| !v.is_empty())?;
        Some(Credentials {
            access_key,
            secret_key,
            session_token: get("AWS_SESSION_TOKEN").filter(|t| !t.is_empty()),
        })
    }

    /// `profile` in the INI text of a shared credentials file.
    pub fn from_ini(text: &str, profile: &str) -> Option<Credentials> {
        let mut in_profile = false;
        let (mut access_key, mut secret_key, mut token) = (None, None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_profile = name.trim() == profile;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !in_profile {
                continue;
            }
            let value = value.trim().to_string();
            match key.trim() {
                "aws_access_key_id" => access_key = Some(value),
                "aws_secret_access_key" => secret_key = Some(value),
                "aws_session_token" => token = Some(value),
                _ => {}
            }
        }
        Some(Credentials {
            access_key: access_key?,
            secret_key: secret_key?,
            session_token: token,
        })
    }

    /// The shared credentials file's `AWS_PROFILE` (default `default`).
    pub fn from_shared_file() -> Option<Credentials> {
        Credentials::from_shared_file_with(
            |name| std::env::var(name).ok(),
            |path| std::fs::read_to_string(path).ok(),
        )
    }

    /// [`Credentials::from_shared_file`] over any variable lookup and file
    /// read: `AWS_SHARED_CREDENTIALS_FILE`, else `$HOME/.aws/credentials`.
    pub fn from_shared_file_with(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&std::path::Path) -> Option<String>,
    ) -> Option<Credentials> {
        let path = get("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| get("HOME").map(|h| PathBuf::from(h).join(".aws").join("credentials")))?;
        let profile = get("AWS_PROFILE").unwrap_or_else(|| "default".to_string());
        Credentials::from_ini(&read(&path)?, &profile)
    }

    /// The environment, else the shared credentials file, else an ECS task
    /// role, else the EC2 instance role: see [`CredentialSource::chain_with`].
    pub fn from_chain() -> io::Result<CredentialSource> {
        CredentialSource::chain_with(
            |name| std::env::var(name).ok(),
            |path| std::fs::read_to_string(path).ok(),
            Arc::new(PlainHttp {
                timeout: ROLE_TIMEOUT,
            }),
        )
    }
}

/// What signs a store's requests: fixed credentials, or a role's, fetched
/// again shortly before they expire.
#[derive(Clone)]
pub struct CredentialSource {
    inner: Arc<Source>,
}

enum Source {
    Fixed(Credentials),
    Role(Role),
}

/// A role's credentials endpoint and what it last handed out.
struct Role {
    fetch: Fetch,
    transport: Arc<dyn Transport>,
    held: Mutex<Option<Held>>,
}

enum Fetch {
    /// The container endpoint's URL, and its `Authorization` value.
    Container { url: String, auth: Option<String> },
    /// The instance metadata service at this endpoint.
    Imds { endpoint: String },
}

struct Held {
    credentials: Credentials,
    /// Unix seconds.
    expires: i64,
}

impl fmt::Debug for CredentialSource {
    /// Never prints a secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.inner {
            Source::Fixed(c) => f.debug_tuple("Fixed").field(c).finish(),
            Source::Role(role) => match &role.fetch {
                Fetch::Container { url, .. } => f.debug_tuple("ContainerRole").field(url).finish(),
                Fetch::Imds { endpoint } => f.debug_tuple("InstanceRole").field(endpoint).finish(),
            },
        }
    }
}

impl From<Credentials> for CredentialSource {
    fn from(credentials: Credentials) -> CredentialSource {
        CredentialSource {
            inner: Arc::new(Source::Fixed(credentials)),
        }
    }
}

impl CredentialSource {
    /// The credentials to sign with now. A role's are fetched again when
    /// they expire within five minutes; if that fails, the old ones serve
    /// while they are still valid.
    pub fn current(&self) -> io::Result<Credentials> {
        match &*self.inner {
            Source::Fixed(c) => Ok(c.clone()),
            Source::Role(role) => role.current(unix_now()),
        }
    }

    /// The chain over any variable lookup, file read and transport:
    ///
    /// 1. `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`;
    /// 2. the shared credentials file ([`Credentials::from_shared_file_with`]);
    /// 3. the container endpoint: `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`
    ///    (on `169.254.170.2`) or `AWS_CONTAINER_CREDENTIALS_FULL_URI`, with
    ///    `AWS_CONTAINER_AUTHORIZATION_TOKEN` or `…_TOKEN_FILE`;
    /// 4. IMDSv2 at `AWS_EC2_METADATA_SERVICE_ENDPOINT` (else
    ///    `169.254.169.254`), unless `AWS_EC2_METADATA_DISABLED=true`.
    ///
    /// A role's first credentials are fetched here, so a missing or
    /// misconfigured role fails now rather than at the first request.
    pub fn chain_with(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&Path) -> Option<String>,
        transport: Arc<dyn Transport>,
    ) -> io::Result<CredentialSource> {
        if let Some(c) = Credentials::from_lookup(&get) {
            return Ok(c.into());
        }
        if let Some(c) = Credentials::from_shared_file_with(&get, &read) {
            return Ok(c.into());
        }
        let fetch = match container_url(&get) {
            Some(url) => Fetch::Container {
                url,
                auth: container_auth(&get, &read),
            },
            None => {
                let disabled = get("AWS_EC2_METADATA_DISABLED")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if disabled {
                    return Err(no_credentials("the instance metadata service is disabled"));
                }
                let endpoint = get("AWS_EC2_METADATA_SERVICE_ENDPOINT")
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| IMDS_ENDPOINT.to_string());
                Fetch::Imds {
                    endpoint: endpoint.trim_end_matches('/').to_string(),
                }
            }
        };
        let role = Role {
            fetch,
            transport,
            held: Mutex::new(None),
        };
        if let Err(e) = role.current(unix_now()) {
            return Err(match role.fetch {
                Fetch::Imds { .. } => no_credentials(&format!("no instance role: {e}")),
                Fetch::Container { .. } => e,
            });
        }
        Ok(CredentialSource {
            inner: Arc::new(Source::Role(role)),
        })
    }
}

impl Role {
    fn current(&self, now: i64) -> io::Result<Credentials> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = held
            .as_ref()
            .filter(|h| now < h.expires.saturating_sub(REFRESH_BEFORE));
        if let Some(h) = fresh {
            return Ok(h.credentials.clone());
        }
        match self.fetch() {
            Ok(new) => {
                let credentials = new.credentials.clone();
                *held = Some(new);
                Ok(credentials)
            }
            Err(e) => match held.as_ref().filter(|h| now < h.expires) {
                Some(h) => Ok(h.credentials.clone()),
                None => Err(e),
            },
        }
    }

    fn fetch(&self) -> io::Result<Held> {
        match &self.fetch {
            Fetch::Container { url, auth } => {
                let headers = auth
                    .iter()
                    .map(|a| ("authorization".to_string(), a.clone()))
                    .collect();
                parse_role_json(&self.get(url, "GET", headers)?)
            }
            Fetch::Imds { endpoint } => {
                let ttl = (
                    "x-aws-ec2-metadata-token-ttl-seconds".to_string(),
                    "21600".to_string(),
                );
                let token = self.get(&format!("{endpoint}/latest/api/token"), "PUT", vec![ttl])?;
                let with_token = || vec![("x-aws-ec2-metadata-token".to_string(), token.clone())];
                let base = format!("{endpoint}/latest/meta-data/iam/security-credentials/");
                let roles = self.get(&base, "GET", with_token())?;
                let name = roles.lines().map(str::trim).find(|l| !l.is_empty());
                let name = name.ok_or_else(|| invalid("the instance has no role"))?;
                parse_role_json(&self.get(&format!("{base}{name}"), "GET", with_token())?)
            }
        }
    }

    /// The body of a 200 answer to `method url`, as text.
    fn get(&self, url: &str, method: &str, headers: Vec<(String, String)>) -> io::Result<String> {
        let (endpoint, target) = split_url(url)?;
        let resp = self.transport.send(&HttpRequest {
            method: method.to_string(),
            endpoint,
            target,
            headers,
            body: Vec::new(),
        })?;
        if resp.status != 200 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{method} {url} answered {}", resp.status),
            ));
        }
        String::from_utf8(resp.body).map_err(|_| invalid("credentials are not UTF-8"))
    }
}

/// `http://host[:port]/path` into the endpoint and the path.
fn split_url(url: &str) -> io::Result<(Endpoint, String)> {
    let after_scheme = url.find("://").map_or(0, |i| i.saturating_add(3));
    let path_at = url
        .get(after_scheme..)
        .and_then(|rest| rest.find('/'))
        .map_or(url.len(), |i| i.saturating_add(after_scheme));
    let (base, path) = url.split_at(path_at);
    let path = if path.is_empty() { "/" } else { path };
    Ok((Endpoint::parse(base)?, path.to_string()))
}

fn container_url(get: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let relative = get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").filter(|v| !v.is_empty());
    match relative {
        Some(r) => Some(format!("{CONTAINER_HOST}{r}")),
        None => get("AWS_CONTAINER_CREDENTIALS_FULL_URI").filter(|v| !v.is_empty()),
    }
}

fn container_auth(
    get: &impl Fn(&str) -> Option<String>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<String> {
    let token = get("AWS_CONTAINER_AUTHORIZATION_TOKEN").filter(|v| !v.is_empty());
    match token {
        Some(t) => Some(t),
        None => get("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
            .and_then(|file| read(Path::new(&file)))
            .map(|t| t.trim().to_string()),
    }
}

/// `AccessKeyId`, `SecretAccessKey`, `Token` and `Expiration` of a role
/// endpoint's JSON answer (the container endpoint and IMDS give the same).
fn parse_role_json(text: &str) -> io::Result<Held> {
    let not_json = || invalid("credentials are not a JSON object");
    let (JsonValue::Object(fields), _) = parse_object(text).ok_or_else(not_json)? else {
        return Err(not_json());
    };
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| match v {
                // The parser leaves escapes as they are; `\/` is the one a
                // base64 secret or token can carry.
                JsonValue::Str(s) => Some(s.replace("\\/", "/")),
                _ => None,
            })
    };
    let missing = |name: &str| invalid(&format!("credentials without {name}"));
    let expiration = field("Expiration").ok_or_else(|| missing("Expiration"))?;
    let expires = parse_iso8601(&expiration).ok_or_else(|| missing("a valid Expiration"))?;
    Ok(Held {
        credentials: Credentials {
            access_key: field("AccessKeyId").ok_or_else(|| missing("AccessKeyId"))?,
            secret_key: field("SecretAccessKey").ok_or_else(|| missing("SecretAccessKey"))?,
            session_token: field("Token"),
        },
        expires: expires.div_euclid(1_000_000_000),
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

fn no_credentials(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no AWS credentials in the environment or the shared credentials file, and {why}"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const INI: &str = "# comment\n[default]\naws_access_key_id = AKDEFAULT\naws_secret_access_key=sdefault\n\n[work]\naws_access_key_id = AKWORK\naws_secret_access_key = swork\naws_session_token = tok\n";

    #[test]
    fn a_profile_is_read_from_the_ini_text() {
        let d = Credentials::from_ini(INI, "default").unwrap();
        assert_eq!(
            (d.access_key.as_str(), d.secret_key.as_str()),
            ("AKDEFAULT", "sdefault")
        );
        assert_eq!(d.session_token, None);
        let w = Credentials::from_ini(INI, "work").unwrap();
        assert_eq!(
            (w.access_key.as_str(), w.session_token.as_deref()),
            ("AKWORK", Some("tok"))
        );
        assert!(Credentials::from_ini(INI, "absent").is_none());
        assert!(Credentials::from_ini("[p]\naws_access_key_id = only\n", "p").is_none());
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn the_environment_needs_both_keys_and_may_carry_a_token() {
        let both = [("AWS_ACCESS_KEY_ID", "AK"), ("AWS_SECRET_ACCESS_KEY", "sk")];
        let c = Credentials::from_lookup(vars(&both)).unwrap();
        assert_eq!(
            (c.access_key.as_str(), c.session_token.as_deref()),
            ("AK", None)
        );
        let with_token = [
            ("AWS_ACCESS_KEY_ID", "AK"),
            ("AWS_SECRET_ACCESS_KEY", "sk"),
            ("AWS_SESSION_TOKEN", "t"),
        ];
        let c = Credentials::from_lookup(vars(&with_token)).unwrap();
        assert_eq!(c.session_token.as_deref(), Some("t"));
        assert!(Credentials::from_lookup(vars(&[("AWS_ACCESS_KEY_ID", "AK")])).is_none());
        let empty = [("AWS_ACCESS_KEY_ID", ""), ("AWS_SECRET_ACCESS_KEY", "sk")];
        assert!(Credentials::from_lookup(vars(&empty)).is_none());
    }

    #[test]
    fn the_shared_file_is_found_by_variable_or_home_and_profile() {
        let read = |path: &std::path::Path| {
            (path.ends_with(".aws/credentials") || path.ends_with("custom"))
                .then(|| INI.to_string())
        };
        // $HOME/.aws/credentials, the default profile.
        let c = Credentials::from_shared_file_with(vars(&[("HOME", "/h")]), read).unwrap();
        assert_eq!(c.access_key, "AKDEFAULT");
        // An explicit file and profile win.
        let pairs = [
            ("AWS_SHARED_CREDENTIALS_FILE", "/x/custom"),
            ("AWS_PROFILE", "work"),
            ("HOME", "/h"),
        ];
        let c = Credentials::from_shared_file_with(vars(&pairs), read).unwrap();
        assert_eq!(c.access_key, "AKWORK");
        // No home, no file, or an unreadable one: nothing.
        assert!(Credentials::from_shared_file_with(vars(&[]), read).is_none());
        assert!(Credentials::from_shared_file_with(vars(&[("HOME", "/h")]), |_| None).is_none());
    }

    #[test]
    fn the_real_environment_and_file_are_consulted_without_panicking() {
        // What they find depends on the machine; they just must not fail.
        // (`from_chain` is left out: on a cloud runner it may find a role.)
        let _ = Credentials::from_env();
        let _ = Credentials::from_shared_file();
        assert_eq!(no_credentials("x").kind(), io::ErrorKind::NotFound);
    }

    use super::super::http::HttpResponse;

    /// A status (0: no answer) and body for a request.
    type Reply = Box<dyn Fn(&HttpRequest) -> (u16, String) + Send + Sync>;

    /// A transport that answers from `reply` and keeps what it was sent.
    struct Fake {
        reply: Reply,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl Transport for Fake {
        fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse> {
            self.seen.lock().unwrap().push(request.clone());
            let (status, body) = (self.reply)(request);
            if status == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no answer"));
            }
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: body.into_bytes(),
            })
        }
    }

    fn fake(reply: impl Fn(&HttpRequest) -> (u16, String) + Send + Sync + 'static) -> Arc<Fake> {
        Arc::new(Fake {
            reply: Box::new(reply),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn role_json(key: &str, expiration: &str) -> String {
        format!(
            r#"{{"Code":"Success","AccessKeyId":"{key}","SecretAccessKey":"se\/cret","Token":"tok","Expiration":"{expiration}"}}"#
        )
    }

    fn header<'r>(r: &'r HttpRequest, name: &str) -> Option<&'r str> {
        r.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// IMDSv2 at 169.254.169.254: a token, then the role, then its keys.
    fn imds() -> Arc<Fake> {
        fake(|r| {
            let token = header(r, "x-aws-ec2-metadata-token") == Some("T0K");
            match (r.method.as_str(), r.target.as_str()) {
                ("PUT", "/latest/api/token") => (200, "T0K".to_string()),
                ("GET", "/latest/meta-data/iam/security-credentials/") if token => {
                    (200, "frank-test-instance\n".to_string())
                }
                ("GET", "/latest/meta-data/iam/security-credentials/frank-test-instance")
                    if token =>
                {
                    (200, role_json("ASIAROLE", "2099-01-01T00:00:00Z"))
                }
                _ => (401, String::new()),
            }
        })
    }

    #[test]
    fn keys_in_the_environment_or_file_come_before_any_role() {
        let t = imds();
        let env = [("AWS_ACCESS_KEY_ID", "AK"), ("AWS_SECRET_ACCESS_KEY", "sk")];
        let s = CredentialSource::chain_with(vars(&env), |_| None, t.clone()).unwrap();
        assert_eq!(s.current().unwrap().access_key, "AK");
        let s =
            CredentialSource::chain_with(vars(&[("HOME", "/h")]), |_| Some(INI.into()), t.clone())
                .unwrap();
        assert_eq!(s.current().unwrap().access_key, "AKDEFAULT");
        assert!(format!("{s:?}").starts_with("Fixed"));
        assert!(t.seen.lock().unwrap().is_empty(), "no role endpoint asked");
    }

    #[test]
    fn the_instance_role_comes_from_imdsv2() {
        let t = imds();
        let s = CredentialSource::chain_with(vars(&[]), |_| None, t.clone()).unwrap();
        let c = s.current().unwrap();
        assert_eq!(
            (
                c.access_key.as_str(),
                c.secret_key.as_str(),
                c.session_token.as_deref()
            ),
            ("ASIAROLE", "se/cret", Some("tok"))
        );
        let seen = t.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "token, role name, keys; then cached");
        assert_eq!(seen[0].endpoint.host, "169.254.169.254");
        assert_eq!(
            header(&seen[0], "x-aws-ec2-metadata-token-ttl-seconds"),
            Some("21600")
        );
        let shown = format!("{s:?}");
        assert!(shown.starts_with("InstanceRole"), "{shown}");
        assert!(
            !shown.contains("se/cret") && !shown.contains("tok"),
            "{shown}"
        );
    }

    #[test]
    fn imds_can_be_moved_or_disabled() {
        let t = imds();
        let moved = [(
            "AWS_EC2_METADATA_SERVICE_ENDPOINT",
            "http://127.0.0.1:1338/",
        )];
        CredentialSource::chain_with(vars(&moved), |_| None, t.clone()).unwrap();
        assert_eq!(t.seen.lock().unwrap()[0].endpoint.port, 1338);
        let off = [("AWS_EC2_METADATA_DISABLED", "TRUE")];
        let e = CredentialSource::chain_with(vars(&off), |_| None, imds()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("disabled"), "{e}");
    }

    #[test]
    fn no_instance_role_is_no_credentials() {
        // A laptop: nothing answers.
        let e = CredentialSource::chain_with(vars(&[]), |_| None, fake(|_| (0, String::new())))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("no instance role"), "{e}");
        // An instance without a role: an empty list.
        let none = fake(|r| match r.method.as_str() {
            "PUT" => (200, "T".to_string()),
            _ => (200, String::new()),
        });
        let e = CredentialSource::chain_with(vars(&[]), |_| None, none).unwrap_err();
        assert!(e.to_string().contains("has no role"), "{e}");
    }

    #[test]
    fn the_ecs_task_role_comes_from_the_container_endpoint() {
        let t = fake(|r| match (r.target.as_str(), header(r, "authorization")) {
            ("/v2/credentials/abc", None) => (200, role_json("ASIATASK", "2099-01-01T00:00:00Z")),
            ("/creds", Some("secret-token")) => {
                (200, role_json("ASIAFULL", "2099-01-01T00:00:00Z"))
            }
            _ => (403, String::new()),
        });
        let rel = [(
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "/v2/credentials/abc",
        )];
        let s = CredentialSource::chain_with(vars(&rel), |_| None, t.clone()).unwrap();
        assert_eq!(s.current().unwrap().access_key, "ASIATASK");
        assert_eq!(t.seen.lock().unwrap()[0].endpoint.host, "169.254.170.2");
        assert!(format!("{s:?}").starts_with("ContainerRole"));
        // A full URI, its token from a file (EKS Pod Identity's way).
        let full = [
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://127.0.0.1:9911/creds",
            ),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "/var/run/token"),
        ];
        let read = |p: &Path| (p == Path::new("/var/run/token")).then(|| "secret-token\n".into());
        let s = CredentialSource::chain_with(vars(&full), read, t.clone()).unwrap();
        assert_eq!(s.current().unwrap().access_key, "ASIAFULL");
        // The token in a variable wins over the file; a refused one is an error.
        let wrong = [
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://127.0.0.1:9911/creds",
            ),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "wrong"),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "/var/run/token"),
        ];
        let e = CredentialSource::chain_with(vars(&wrong), read, t).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
    }

    #[test]
    fn a_role_is_fetched_again_before_it_expires_and_kept_if_that_fails() {
        // Expires at 2026-11-08T12:00:00Z; the endpoint can be switched off.
        let expires = 1_794_139_200;
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (u, n) = (up.clone(), calls.clone());
        let t = fake(move |_| {
            let k = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if u.load(std::sync::atomic::Ordering::SeqCst) {
                (200, role_json(&format!("K{k}"), "2026-11-08T12:00:00Z"))
            } else {
                (0, String::new())
            }
        });
        let role = Role {
            fetch: Fetch::Container {
                url: "http://169.254.170.2/v2/credentials/x".into(),
                auth: None,
            },
            transport: t,
            held: Mutex::new(None),
        };
        assert_eq!(role.current(expires - 3600).unwrap().access_key, "K0");
        // Well before expiry: the held keys, no request.
        assert_eq!(
            role.current(expires - REFRESH_BEFORE - 1)
                .unwrap()
                .access_key,
            "K0"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Within five minutes of it: fetched again.
        assert_eq!(
            role.current(expires - REFRESH_BEFORE).unwrap().access_key,
            "K1"
        );
        // The endpoint is down: the old keys serve until they expire, then not.
        up.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(role.current(expires - 1).unwrap().access_key, "K1");
        assert!(role.current(expires).is_err());
    }

    #[test]
    fn a_role_answer_needs_keys_and_an_expiration() {
        let held = parse_role_json(&role_json("AK", "2026-11-08T12:00:00Z")).unwrap();
        assert_eq!(held.expires, 1_794_139_200);
        assert_eq!(held.credentials.secret_key, "se/cret");
        for bad in [
            "not json",
            "[1]",
            r#"{"AccessKeyId":"a","SecretAccessKey":"s"}"#,
            r#"{"AccessKeyId":"a","SecretAccessKey":"s","Expiration":"soon"}"#,
            r#"{"SecretAccessKey":"s","Expiration":"2026-11-08T12:00:00Z"}"#,
            r#"{"AccessKeyId":"a","Expiration":"2026-11-08T12:00:00Z"}"#,
            r#"{"AccessKeyId":1,"SecretAccessKey":"s","Expiration":"2026-11-08T12:00:00Z"}"#,
        ] {
            let e = parse_role_json(bad).err().unwrap();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{bad}");
        }
        // No token is fine (an ECS role always sends one, but nothing needs it).
        let held = parse_role_json(
            r#"{"AccessKeyId":"a","SecretAccessKey":"s","Expiration":"2026-11-08T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(held.credentials.session_token, None);
    }

    #[test]
    fn a_url_splits_into_endpoint_and_path() {
        let (e, p) = split_url("http://169.254.170.2/v2/credentials/x").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, p.as_str()),
            ("169.254.170.2", 80, "/v2/credentials/x")
        );
        let (e, p) = split_url("http://127.0.0.1:9911").unwrap();
        assert_eq!((e.port, p.as_str()), (9911, "/"));
        assert!(split_url("ftp://x/y").is_err());
        assert!(split_url("no-scheme").is_err());
    }

    #[test]
    fn a_non_utf8_answer_is_invalid() {
        let t = fake(|_| (200, String::new()));
        let role = Role {
            fetch: Fetch::Container {
                url: "http://h/x".into(),
                auth: None,
            },
            transport: Arc::new(Bytes(t)),
            held: Mutex::new(None),
        };
        let e = role.current(0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// Answers with bytes that are not UTF-8.
    struct Bytes(Arc<Fake>);

    impl Transport for Bytes {
        fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse> {
            let mut r = self.0.send(request)?;
            r.body = vec![0xff, 0xfe];
            Ok(r)
        }
    }
}
