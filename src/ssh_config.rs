//! Minimal `~/.ssh/config` parser used to import hosts as meatshell sessions.
//!
//! We only read the handful of fields a session needs — `HostName`, `User`,
//! `Port`, `IdentityFile` — grouped under each concrete `Host` alias.  Wildcard
//! patterns (`Host *`) and unsupported directives are ignored; this is a
//! convenience importer, not a full ssh_config implementation.

use std::path::{Path, PathBuf};

use crate::config::{AuthMethod, Session};

/// One importable host parsed from `~/.ssh/config`.
#[derive(Debug, Clone)]
pub struct ImportedHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: String,
}

impl ImportedHost {
    pub fn auth_method(&self) -> AuthMethod {
        if self.identity_file.is_empty() {
            AuthMethod::Password
        } else {
            AuthMethod::Key
        }
    }

    /// Convert an imported OpenSSH host block into a saved meatshell session.
    ///
    /// The existing bulk importer historically defaulted missing users to
    /// `root`; keep that behavior for compatibility while the new-session
    /// picker can still use the raw parsed fields and leave user blank.
    pub fn into_session(self) -> Session {
        let auth = self.auth_method();
        Session {
            name: self.alias,
            host: self.hostname,
            port: self.port,
            user: if self.user.is_empty() {
                "root".into()
            } else {
                self.user
            },
            auth,
            private_key_path: self.identity_file,
            ..Session::new_empty()
        }
    }
}

/// Parse the user's `~/.ssh/config` (returns empty if it doesn't exist).
pub fn parse_default() -> Vec<ImportedHost> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let path = home.join(".ssh").join("config");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_str(&text, &home),
        Err(_) => Vec::new(),
    }
}

fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

/// Split a config line into its keyword and value, supporting both
/// `Keyword value` and `Keyword=value`, and stripping surrounding quotes.
fn split_kv(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    // Separator is the first '=' or run of whitespace.
    let (k, v) = if let Some(eq) = line.find('=') {
        // Only treat '=' as the separator if it comes before any space.
        let sp = line.find(char::is_whitespace).unwrap_or(usize::MAX);
        if eq < sp {
            (&line[..eq], &line[eq + 1..])
        } else {
            line.split_once(char::is_whitespace)?
        }
    } else {
        line.split_once(char::is_whitespace)?
    };
    let v = v.trim().trim_matches('"').trim();
    if v.is_empty() {
        return None;
    }
    Some((k.trim().to_ascii_lowercase(), v.to_string()))
}

fn expand_tilde(path: &str, home: &Path) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest).to_string_lossy().replace('\\', "/")
    } else if path == "~" {
        home.to_string_lossy().replace('\\', "/")
    } else {
        path.replace('\\', "/")
    }
}

fn is_concrete(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.contains(['*', '?', '!'])
}

/// Validate a `HostName` before importing it (issue #33).
///
/// Accepts IPv4 / IPv6 literals and DNS-style hostnames; rejects anything with
/// shell metacharacters, whitespace, control characters, scheme prefixes
/// (`http://…`), etc.  This stops a malformed or hostile `~/.ssh/config` entry
/// from flowing unchecked into a saved session.  Internal IPs are intentionally
/// allowed — they are a legitimate SSH target and can't be told apart by format.
fn is_valid_hostname(s: &str) -> bool {
    // IP literals (including private/loopback addresses) are always fine.
    if s.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    // DNS-style: dot-separated labels of [A-Za-z0-9_-], no empty or
    // hyphen-edged labels.
    s.split('.').all(|label| {
        let b = label.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b.iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
    })
}

pub fn parse_str(text: &str, home: &Path) -> Vec<ImportedHost> {
    let mut hosts: Vec<ImportedHost> = Vec::new();
    let mut cur: Option<ImportedHost> = None;

    let flush = |cur: &mut Option<ImportedHost>, out: &mut Vec<ImportedHost>| {
        if let Some(mut h) = cur.take() {
            if h.hostname.is_empty() {
                h.hostname = h.alias.clone();
            }
            // Skip entries whose HostName isn't a valid IP / hostname (issue #33).
            if is_valid_hostname(&h.hostname) {
                out.push(h);
            } else {
                tracing::warn!(
                    alias = %h.alias,
                    "skipping ~/.ssh/config host with invalid HostName"
                );
            }
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = split_kv(line) else {
            continue;
        };
        match key.as_str() {
            "host" => {
                flush(&mut cur, &mut hosts);
                // Take the first concrete (non-wildcard) alias of the block.
                if let Some(alias) = val.split_whitespace().find(|p| is_concrete(p)) {
                    cur = Some(ImportedHost {
                        alias: alias.to_string(),
                        hostname: String::new(),
                        user: String::new(),
                        port: 22,
                        identity_file: String::new(),
                    });
                }
            }
            "hostname" => {
                if let Some(h) = cur.as_mut() {
                    h.hostname = val;
                }
            }
            "user" => {
                if let Some(h) = cur.as_mut() {
                    h.user = val;
                }
            }
            "port" => {
                if let Some(h) = cur.as_mut() {
                    if let Ok(p) = val.parse::<u16>() {
                        h.port = p;
                    }
                }
            }
            "identityfile" => {
                if let Some(h) = cur.as_mut() {
                    if h.identity_file.is_empty() {
                        h.identity_file = expand_tilde(&val, home);
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut hosts);
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_basic_blocks() {
        let cfg = "\
# comment
Host prod web-prod
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519

Host *
    User nobody

Host alias-only
";
        let home = Path::new("/home/me");
        let hosts = parse_str(cfg, home);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "prod");
        assert_eq!(hosts[0].hostname, "10.0.0.5");
        assert_eq!(hosts[0].user, "deploy");
        assert_eq!(hosts[0].port, 2222);
        assert!(hosts[0].identity_file.ends_with("/.ssh/id_ed25519"));
        // alias-only: hostname falls back to the alias
        assert_eq!(hosts[1].alias, "alias-only");
        assert_eq!(hosts[1].hostname, "alias-only");
    }

    #[test]
    fn imported_host_converts_to_session() {
        let cfg = "\
Host prod
    HostName prod.example.com
    Port 2200
    IdentityFile ~/.ssh/prod
";
        let home = Path::new("/home/me");
        let mut hosts = parse_str(cfg, home);
        let session = hosts.remove(0).into_session();

        assert_eq!(session.name, "prod");
        assert_eq!(session.host, "prod.example.com");
        assert_eq!(session.port, 2200);
        assert_eq!(session.user, "root");
        assert_eq!(session.auth, AuthMethod::Key);
        assert!(session.private_key_path.ends_with("/.ssh/prod"));
    }

    #[test]
    fn rejects_invalid_hostnames() {
        let cfg = "\
Host good
    HostName 10.0.0.5
Host ipv6
    HostName ::1
Host evil
    HostName a;rm -rf /
Host spaced
    HostName foo bar
Host scheme
    HostName http://x.com
";
        let home = Path::new("/home/me");
        let aliases: Vec<String> = parse_str(cfg, home).into_iter().map(|h| h.alias).collect();
        assert!(aliases.contains(&"good".to_string()));
        assert!(aliases.contains(&"ipv6".to_string()));
        assert!(!aliases.contains(&"evil".to_string()));
        assert!(!aliases.contains(&"spaced".to_string()));
        assert!(!aliases.contains(&"scheme".to_string()));
    }

    #[test]
    fn validates_hostname_formats() {
        assert!(is_valid_hostname("192.168.1.1"));
        assert!(is_valid_hostname("::1"));
        assert!(is_valid_hostname("example.com"));
        assert!(is_valid_hostname("my-server_01.internal"));
        assert!(!is_valid_hostname("a;rm -rf /"));
        assert!(!is_valid_hostname("foo bar"));
        assert!(!is_valid_hostname("http://x.com"));
        assert!(!is_valid_hostname("-bad.com"));
        assert!(!is_valid_hostname(""));
    }
}
