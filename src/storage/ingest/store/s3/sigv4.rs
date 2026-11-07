//! AWS Signature Version 4 for S3 requests (header-based authorization).

use super::sha256::{hex, hmac_sha256, sha256};

/// Static or session credentials.
#[derive(Clone)]
pub struct Credentials {
    /// The access key id.
    pub access_key: String,
    /// The secret access key.
    pub secret_key: String,
    /// The session token of temporary credentials, sent as
    /// `x-amz-security-token`.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// Never prints the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Percent-encodes `s` per SigV4: everything but `A-Z a-z 0-9 - _ . ~`, and
/// `/` too unless `keep_slash` (a path keeps its slashes).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let plain = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if plain || (keep_slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push('%');
            for nibble in [b >> 4, b & 0x0f] {
                out.push(
                    char::from_digit(u32::from(nibble), 16)
                        .unwrap_or('0')
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    out
}

/// The canonical query string: encoded, sorted by name then value.
pub fn canonical_query(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// What the signature covers.
pub struct SignInput<'a> {
    /// `GET`, `PUT`, ...
    pub method: &'a str,
    /// The request path, already percent-encoded.
    pub path: &'a str,
    /// The query parameters, unencoded.
    pub query: &'a [(String, String)],
    /// Every header to sign, lowercase names (at least `host`,
    /// `x-amz-content-sha256` and `x-amz-date`).
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the body.
    pub payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
    /// The signing region.
    pub region: &'a str,
}

/// The `Authorization` header value for `input`.
pub fn authorization(creds: &Credentials, input: &SignInput<'_>) -> String {
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.path,
        canonical_query(input.query),
        canonical_headers,
        signed_headers,
        input.payload_hash
    );
    let date = input.amz_date.get(..8).unwrap_or(input.amz_date);
    let scope = format!("{date}/{}/s3/aws4_request", input.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        input.amz_date,
        hex(&sha256(canonical.as_bytes()))
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, input.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

/// `YYYYMMDDTHHMMSSZ` for `secs` seconds after the Unix epoch (UTC).
pub fn amz_date(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant), for days since 1970-01-01.
    let z = days.wrapping_add(719_468);
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = doe
        .wrapping_sub(doe / 1_460)
        .wrapping_add(doe / 36_524)
        .wrapping_sub(doe / 146_096)
        / 365;
    let doy = doe.wrapping_sub(
        yoe.wrapping_mul(365)
            .wrapping_add(yoe / 4)
            .wrapping_sub(yoe / 100),
    );
    let mp = (5u64.wrapping_mul(doy).wrapping_add(2)) / 153;
    let day = doy
        .wrapping_sub((153u64.wrapping_mul(mp).wrapping_add(2)) / 5)
        .wrapping_add(1);
    let month = if mp < 10 {
        mp.wrapping_add(3)
    } else {
        mp.wrapping_sub(9)
    };
    let year = yoe
        .wrapping_add(era.wrapping_mul(400))
        .wrapping_add(u64::from(month <= 2));
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn creds() -> Credentials {
        Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        }
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn signature(input: &SignInput<'_>) -> String {
        let auth = authorization(&creds(), input);
        auth.rsplit("Signature=").next().unwrap().to_string()
    }

    // The examples in "Signature Calculations for the Authorization
    // Header" (AWS S3 API reference), 2013-05-24, us-east-1.

    #[test]
    fn a_ranged_get_matches_the_aws_example() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let input = SignInput {
            method: "GET",
            path: "/test.txt",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
        };
        assert_eq!(
            signature(&input),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn a_query_without_a_value_matches_the_aws_example() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let query = h(&[("lifecycle", "")]);
        let input = SignInput {
            method: "GET",
            path: "/",
            query: &query,
            headers: &headers,
            payload_hash: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
        };
        assert_eq!(
            signature(&input),
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
    }

    #[test]
    fn a_listing_with_two_parameters_matches_the_aws_example() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let query = h(&[("prefix", "J"), ("max-keys", "2")]);
        let input = SignInput {
            method: "GET",
            path: "/",
            query: &query,
            headers: &headers,
            payload_hash: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
        };
        assert_eq!(
            signature(&input),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn encoding_keeps_unreserved_and_optionally_slashes() {
        assert_eq!(uri_encode("a b/c~d.e-f_g", true), "a%20b/c~d.e-f_g");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("é=&", true), "%C3%A9%3D%26");
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        // A leap day, and the end of a century that is a leap year.
        assert_eq!(amz_date(1_709_210_096), "20240229T123456Z");
        assert_eq!(amz_date(951_782_399), "20000228T235959Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(951_868_800), "20000301T000000Z");
        assert_eq!(amz_date(4_102_444_799), "20991231T235959Z");
    }

    #[test]
    fn credentials_never_print_the_secret() {
        let text = format!("{:?}", creds());
        assert!(!text.contains("wJalr"), "{text}");
    }
}
