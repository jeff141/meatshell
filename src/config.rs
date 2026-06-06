//! Session / application configuration.
//!
//! Persists a simple JSON file under the platform's standard config dir
//! (e.g. `%APPDATA%/meatshell/sessions.json` on Windows).
//!
//! The password field is stored in plain text for v0.1; a proper OS keychain
//! integration is tracked for a later iteration.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

/// A secret string (e.g. a session password) whose heap buffer is zeroed when
/// it is dropped, so plaintext credentials don't survive in freed memory and
/// turn up in core dumps, a debugger, or `/proc/<pid>/mem`.  `Clone` makes an
/// independent copy that is likewise zeroed on its own drop, and `Debug` is
/// redacted so a password can never be logged by accident.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never reveal the contents in logs / debug output.
        f.write_str(if self.0.is_empty() { "Secret(\"\")" } else { "Secret(***)" })
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Secret(String::deserialize(d)?))
    }
}

/// How a session authenticates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    Key,
}

impl AuthMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::Key => "key",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "key" => AuthMethod::Key,
            _ => AuthMethod::Password,
        }
    }
}

/// A single saved SSH target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    #[serde(default)]
    pub password: Secret,
    #[serde(default)]
    pub private_key_path: String,
    /// Optional outbound proxy, e.g. "socks5://127.0.0.1:1080" or
    /// "http://user:pass@host:8080". Empty = use $ALL_PROXY, else direct.
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub last_used: Option<String>,
}

impl Session {
    pub fn new_empty() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: String::new(),
            host: String::new(),
            port: 22,
            user: "root".into(),
            auth: AuthMethod::Password,
            password: Secret::default(),
            private_key_path: String::new(),
            proxy: String::new(),
            last_used: None,
        }
    }
}

/// On-disk layout. Keep additive to ease forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Preset SFTP download directory. Empty = ask each time.
    #[serde(default)]
    pub download_dir: String,
    /// UI language code: "zh" (default) or "en".
    #[serde(default)]
    pub language: String,
}

pub struct ConfigStore {
    path: PathBuf,
    cache: ConfigFile,
}

impl ConfigStore {
    /// Load (or initialise) the config file. On any parse error we back up the
    /// broken file and start fresh — losing saved sessions is better than
    /// crashing at launch.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config dir {}", parent.display())
            })?;
        }

        let cache = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            match serde_json::from_str::<ConfigFile>(&raw) {
                Ok(cfg) => cfg,
                Err(err) => {
                    let backup = path.with_extension("json.broken");
                    let _ = fs::rename(&path, &backup);
                    tracing::warn!(
                        "config file was corrupt ({err}); backed up to {}",
                        backup.display()
                    );
                    ConfigFile::default()
                }
            }
        } else {
            ConfigFile::default()
        };

        Ok(Self { path, cache })
    }

    fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "meatshell", "meatshell")
            .context("could not determine user config directory")?;
        Ok(dirs.config_dir().join("sessions.json"))
    }

    pub fn sessions(&self) -> &[Session] {
        &self.cache.sessions
    }

    #[allow(dead_code)] // reserved for an upcoming reorder/drag-drop feature
    pub fn sessions_mut(&mut self) -> &mut Vec<Session> {
        &mut self.cache.sessions
    }

    pub fn upsert(&mut self, session: Session) {
        if let Some(existing) = self
            .cache
            .sessions
            .iter_mut()
            .find(|s| s.id == session.id)
        {
            *existing = session;
        } else {
            self.cache.sessions.push(session);
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.cache.sessions.retain(|s| s.id != id);
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.cache.sessions.iter().find(|s| s.id == id)
    }

    pub fn download_dir(&self) -> &str {
        &self.cache.download_dir
    }

    pub fn set_download_dir(&mut self, dir: String) {
        self.cache.download_dir = dir;
    }

    /// UI language code ("zh" default / "en").
    pub fn language(&self) -> &str {
        if self.cache.language.is_empty() {
            "zh"
        } else {
            &self.cache.language
        }
    }

    pub fn set_language(&mut self, lang: String) {
        self.cache.language = lang;
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.cache)?;
        // Write to a sibling temp file then rename — cheap atomicity on most
        // platforms. Good enough for a config file.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, raw)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to finalise {}", self.path.display()))?;
        Ok(())
    }
}
