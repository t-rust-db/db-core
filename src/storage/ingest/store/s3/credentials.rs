//! Where credentials come from: the environment, then the shared
//! credentials file (`~/.aws/credentials`), like the AWS tools.

use std::io;
use std::path::PathBuf;

use super::sigv4::Credentials;

impl Credentials {
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
    /// `AWS_SESSION_TOKEN`; `None` if the first two are not both set.
    pub fn from_env() -> Option<Credentials> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        if access_key.is_empty() || secret_key.is_empty() {
            return None;
        }
        Some(Credentials {
            access_key,
            secret_key,
            session_token: std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
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
        let path = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aws").join("credentials"))
            })?;
        let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_string());
        Credentials::from_ini(&std::fs::read_to_string(path).ok()?, &profile)
    }

    /// The environment, else the shared credentials file.
    pub fn from_chain() -> io::Result<Credentials> {
        Credentials::from_env()
            .or_else(Credentials::from_shared_file)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no AWS credentials in the environment or the shared credentials file",
                )
            })
    }
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
}
