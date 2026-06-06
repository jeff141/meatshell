//! Minimal `~/.ssh/config` parser used to import hosts as meatshell sessions.
//!
//! We only read the handful of fields a session needs — `HostName`, `User`,
//! `Port`, `IdentityFile` — grouped under each concrete `Host` alias.  Wildcard
//! patterns (`Host *`) and unsupported directives are ignored; this is a
//! convenience importer, not a full ssh_config implementation.

use std::path::{Path, PathBuf};

/// One importable host parsed from `~/.ssh/config`.
#[derive(Debug, Clone)]
pub struct ImportedHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: String,
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

pub fn parse_str(text: &str, home: &Path) -> Vec<ImportedHost> {
    let mut hosts: Vec<ImportedHost> = Vec::new();
    let mut cur: Option<ImportedHost> = None;

    let flush = |cur: &mut Option<ImportedHost>, out: &mut Vec<ImportedHost>| {
        if let Some(mut h) = cur.take() {
            if h.hostname.is_empty() {
                h.hostname = h.alias.clone();
            }
            out.push(h);
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
}
