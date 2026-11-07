//! The HTTP boundary: a [`Transport`] sends one request and returns one
//! response. [`PlainHttp`] is std-only HTTP/1.1 over TCP (Garage, MinIO, a
//! local gateway). HTTPS needs a TLS transport, which db-core does not
//! carry: the embedding application supplies one.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Where requests go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `https` rather than `http`.
    pub https: bool,
    /// The host name or address.
    pub host: String,
    /// The port (the scheme's default if the URL named none).
    pub port: u16,
}

impl Endpoint {
    /// Parses `http://host[:port]` or `https://host[:port]` (no path).
    pub fn parse(url: &str) -> io::Result<Endpoint> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad endpoint {url:?}"));
        let (https, rest) = match url.split_once("://") {
            Some(("http", rest)) => (false, rest),
            Some(("https", rest)) => (true, rest),
            _ => return Err(bad()),
        };
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains('/') {
            return Err(bad());
        }
        let default = if https { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => {
                (host, port.parse::<u16>().map_err(|_| bad())?)
            }
            _ => (authority, default),
        };
        if host.is_empty() {
            return Err(bad());
        }
        Ok(Endpoint {
            https,
            host: host.to_string(),
            port,
        })
    }

    /// The `Host` header value: the port only when it is not the default.
    pub fn host_header(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// One request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// `GET`, `PUT`, `HEAD`, `DELETE`, `POST`.
    pub method: String,
    /// Where it goes.
    pub endpoint: Endpoint,
    /// The path and query, already percent-encoded: `/bucket/key?x=y`.
    pub target: String,
    /// Header names and values (`Host`, `Content-Length` and
    /// `Connection` are added by the transport).
    pub headers: Vec<(String, String)>,
    /// The body, possibly empty.
    pub body: Vec<u8>,
}

/// One response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Header names (lowercased) and values.
    pub headers: Vec<(String, String)>,
    /// The body, de-chunked.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header named `name` (lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends a request, returns the response. Any status is `Ok`; only a
/// failure to talk to the server is an error.
pub trait Transport: Send + Sync {
    /// Performs `request`.
    fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse>;
}

/// HTTP/1.1 over a fresh TCP connection per request, `Connection: close`.
#[derive(Debug, Clone)]
pub struct PlainHttp {
    /// Connect, read and write timeout.
    pub timeout: Duration,
}

impl Default for PlainHttp {
    fn default() -> Self {
        PlainHttp {
            timeout: Duration::from_secs(60),
        }
    }
}

impl Transport for PlainHttp {
    fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse> {
        if request.endpoint.https {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "https needs a TLS transport; PlainHttp speaks http only",
            ));
        }
        let addr = (request.endpoint.host.as_str(), request.endpoint.port);
        let mut stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        exchange(&mut stream, request)
    }
}

/// Writes `request` to `stream` as HTTP/1.1 with `Connection: close`, reads
/// to the end and parses the response. The stream is the caller's: a
/// [`TcpStream`] for [`PlainHttp`], or a TLS stream over one, so an
/// application can add HTTPS by connecting and calling this.
pub fn exchange<S: Read + Write>(
    stream: &mut S,
    request: &HttpRequest,
) -> io::Result<HttpResponse> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        request.method,
        request.target,
        request.endpoint.host_header(),
        request.body.len()
    );
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&request.body)?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw, &request.method)
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("bad HTTP response: {why}"),
    )
}

/// The position just past the first `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .map(|at| at.saturating_add(needle.len()))
}

fn parse_response(raw: &[u8], method: &str) -> io::Result<HttpResponse> {
    let end = find(raw, b"\r\n\r\n").ok_or_else(|| invalid("no end of headers"))?;
    let head = raw.get(..end).unwrap_or_default();
    let rest = raw.get(end..).unwrap_or_default();
    let head = std::str::from_utf8(head).map_err(|_| invalid("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("no status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let find_header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let body = if method == "HEAD" || status == 204 || status == 304 {
        Vec::new()
    } else if find_header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        dechunk(rest)?
    } else if let Some(len) = find_header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if rest.len() < len {
            return Err(invalid("body shorter than content-length"));
        }
        rest.get(..len).unwrap_or_default().to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn dechunk(mut rest: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n").ok_or_else(|| invalid("chunk size line"))?;
        let size_line = rest.get(..line_end.saturating_sub(2)).unwrap_or_default();
        let size_text = std::str::from_utf8(size_line).map_err(|_| invalid("chunk size"))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("chunk size"))?;
        rest = rest.get(line_end..).unwrap_or_default();
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(..size).ok_or_else(|| invalid("short chunk"))?;
        body.extend_from_slice(chunk);
        rest = rest.get(size.saturating_add(2)..).unwrap_or_default();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Serves `reply` to one connection and returns what the client sent.
    fn serve_once(reply: &'static [u8]) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            // Read until the headers and the declared body have arrived.
            loop {
                let n = conn.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
                let Some(end) = find(&seen, b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
                let len: usize = head
                    .split("content-length: ")
                    .nth(1)
                    .and_then(|r| r.split("\r\n").next())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if seen.len() >= end + len {
                    break;
                }
            }
            conn.write_all(reply).unwrap();
            seen
        });
        (port, handle)
    }

    fn request(port: u16, method: &str, body: &[u8]) -> HttpRequest {
        HttpRequest {
            method: method.to_string(),
            endpoint: Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
            target: "/b/k?x=y".to_string(),
            headers: vec![("x-test".to_string(), "1".to_string())],
            body: body.to_vec(),
        }
    }

    #[test]
    fn a_request_goes_out_whole_and_a_sized_body_comes_back() {
        let (port, server) =
            serve_once(b"HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: 5\r\n\r\nhello");
        let resp = PlainHttp::default()
            .send(&request(port, "PUT", b"payload"))
            .unwrap();
        assert_eq!((resp.status, &resp.body[..]), (200, &b"hello"[..]));
        assert_eq!(resp.header("etag"), Some("\"abc\""));
        let seen = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(seen.starts_with("PUT /b/k?x=y HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("x-test: 1\r\n") && seen.contains("content-length: 7\r\n"));
        assert!(seen.contains(&format!("host: 127.0.0.1:{port}\r\n")));
        assert!(seen.ends_with("\r\n\r\npayload"));
    }

    /// A stream over two buffers: the server's reply in, the request out.
    struct Duplex {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sent.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exchange_speaks_http_over_any_stream() {
        let mut stream = Duplex {
            reply: std::io::Cursor::new(
                b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            ),
            sent: Vec::new(),
        };
        let mut req = request(443, "PUT", b"body");
        req.endpoint.https = true;
        let resp = exchange(&mut stream, &req).unwrap();
        assert_eq!((resp.status, resp.body.as_slice()), (200, &b"ok"[..]));
        assert_eq!(resp.header("etag"), Some("\"e\""));
        let sent = String::from_utf8(stream.sent).unwrap();
        assert!(sent.starts_with("PUT "), "{sent}");
        assert!(sent.contains("connection: close\r\n"), "{sent}");
        assert!(sent.ends_with("\r\n\r\nbody"), "{sent}");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        let (port, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\n\r\n",
        );
        let resp = PlainHttp::default()
            .send(&request(port, "GET", b""))
            .unwrap();
        assert_eq!(resp.body, b"abcdefg");
        server.join().unwrap();
    }

    #[test]
    fn head_and_error_statuses_carry_no_surprises() {
        let (port, server) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n");
        let resp = PlainHttp::default()
            .send(&request(port, "HEAD", b""))
            .unwrap();
        assert_eq!((resp.status, resp.body.len()), (200, 0));
        assert_eq!(resp.header("content-length"), Some("99"));
        server.join().unwrap();
        let (port, server) = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\ngone");
        let resp = PlainHttp::default()
            .send(&request(port, "GET", b""))
            .unwrap();
        assert_eq!((resp.status, &resp.body[..]), (404, &b"gone"[..]));
        server.join().unwrap();
    }

    #[test]
    fn a_truncated_response_is_an_error_not_a_short_body() {
        let (port, server) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort");
        let err = PlainHttp::default()
            .send(&request(port, "GET", b""))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        server.join().unwrap();
    }

    #[test]
    fn https_needs_another_transport() {
        let req = HttpRequest {
            endpoint: Endpoint::parse("https://s3.amazonaws.com").unwrap(),
            ..request(1, "GET", b"")
        };
        let err = PlainHttp::default().send(&req).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn endpoints_parse_with_default_ports() {
        let e = Endpoint::parse("http://localhost:3900").unwrap();
        assert_eq!(
            (e.https, e.host.as_str(), e.port),
            (false, "localhost", 3900)
        );
        assert_eq!(e.host_header(), "localhost:3900");
        let e = Endpoint::parse("https://s3.eu-west-1.amazonaws.com/").unwrap();
        assert_eq!((e.https, e.port), (true, 443));
        assert_eq!(e.host_header(), "s3.eu-west-1.amazonaws.com");
        for bad in [
            "localhost:3900",
            "ftp://x",
            "http://",
            "http://h/p",
            "http://h:notaport",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }
}
