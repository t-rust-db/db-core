//! Where credentials come from: the environment, then the shared
//! credentials file (`~/.aws/credentials`), like the AWS tools.

use std::io;
use std::path::PathBuf;

use super::sigv4::Credentials;

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

    /// The environment, else the shared credentials file.
    pub fn from_chain() -> io::Result<Credentials> {
        Credentials::from_env()
            .or_else(Credentials::from_shared_file)
            .ok_or_else(no_credentials)
    }
}

fn no_credentials() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "no AWS credentials in the environment or the shared credentials file",
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
        let _ = Credentials::from_env();
        let _ = Credentials::from_shared_file();
        let chained = Credentials::from_chain();
        assert!(chained.is_ok() || chained.unwrap_err().kind() == io::ErrorKind::NotFound);
        assert_eq!(no_credentials().kind(), io::ErrorKind::NotFound);
    }
}
