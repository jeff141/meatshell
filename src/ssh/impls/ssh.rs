//! SSH session manager.
//!
//! Each open terminal tab maps to exactly one `SshSession`. The session runs
//! on the shared Tokio runtime; commands come in via an MPSC channel and
//! output lines are pushed back via an `UnboundedSender<SessionEvent>`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle, Handler, Msg};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{decode_secret_key, load_secret_key, PrivateKey};
use russh::{Channel, ChannelId, ChannelMsg, Disconnect};
use ssh_key::{HashAlg, PublicKey};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{AuthMethod, PortForward, Secret, Session, SessionTrigger};
use crate::i18n::t;

use super::structs::*;

struct RuntimeTrigger {
    rule: SessionTrigger,
    buffer: String,
    active: bool,
}

struct TriggerEngine(Vec<RuntimeTrigger>);

impl TriggerEngine {
    fn new(rules: &[SessionTrigger]) -> Self {
        Self(
            rules
                .iter()
                .filter(|rule| !rule.expect.is_empty() && !rule.response.is_empty())
                .cloned()
                .map(|rule| RuntimeTrigger {
                    rule,
                    buffer: String::new(),
                    active: true,
                })
                .collect(),
        )
    }

    fn feed(&mut self, text: &str) -> Vec<(Secret, bool)> {
        let mut replies = Vec::new();
        for trigger in &mut self.0 {
            if !trigger.active {
                continue;
            }
            trigger.buffer.push_str(text);
            if let Some(end) = trigger
                .buffer
                .find(&trigger.rule.expect)
                .map(|start| start + trigger.rule.expect.len())
            {
                replies.push((trigger.rule.response.clone(), trigger.rule.append_enter));
                trigger.buffer.drain(..end);
                trigger.active = trigger.rule.repeat;
            }
            // Preserve enough trailing text for a match split across chunks.
            let keep = trigger.rule.expect.len().saturating_sub(1).max(256);
            if trigger.buffer.len() > keep {
                let split = trigger.buffer.len() - keep;
                let split = trigger.buffer.ceil_char_boundary(split);
                trigger.buffer.drain(..split);
            }
        }
        replies
    }
}

// ---------------------------------------------------------------------------
// SFTP-related shared types
// ---------------------------------------------------------------------------

pub(crate) fn load_session_private_key(session: &Session, pass: &str) -> Result<PrivateKey> {
    let pass = if pass.is_empty() { None } else { Some(pass) };
    let inline = session.private_key_inline.as_str().trim();
    if !inline.is_empty() {
        if crate::ssh::ppk::is_ppk(inline.as_bytes()) {
            return crate::ssh::ppk::decode_ppk(inline.as_bytes(), pass.unwrap_or_default())
                .context("failed to parse pasted PuTTY private key");
        }
        return decode_secret_key(inline, pass).context("failed to parse pasted private key");
    }

    let raw = session.private_key_path.trim();
    if raw.is_empty() {
        return Err(anyhow!(t(
            "私钥路径或私钥内容为空",
            "private key path or private key content is empty"
        )));
    }

    let normalised = raw.replace('\\', "/");
    let key_path = normalised
        .strip_suffix(".pub")
        .map(str::to_string)
        .unwrap_or(normalised);
    if Path::new(&key_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ppk"))
    {
        let raw = std::fs::read(&key_path)
            .with_context(|| format!("failed to read PuTTY key {key_path}"))?;
        return crate::ssh::ppk::decode_ppk(&raw, pass.unwrap_or_default())
            .with_context(|| format!("failed to load PuTTY key {key_path}"));
    }
    load_secret_key(Path::new(&key_path), pass)
        .with_context(|| format!("failed to load key {key_path}"))
}

/// Format a byte count as a human-readable string.
pub fn format_size(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= UNIT && idx < UNITS.len() - 1 {
        value /= UNIT;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else if idx >= 4 {
        format!("{:.2} {}", value, UNITS[idx])
    } else {
        format!("{:.1} {}", value, UNITS[idx])
    }
}

#[cfg(test)]
mod size_format_tests {
    use super::{format_size, TriggerEngine};
    use crate::config::{Secret, SessionTrigger};

    #[test]
    fn formats_large_storage_units_through_exabytes() {
        const GIB: u64 = 1024_u64.pow(3);
        const TIB: u64 = 1024_u64.pow(4);
        const PIB: u64 = 1024_u64.pow(5);
        const EIB: u64 = 1024_u64.pow(6);

        assert_eq!(format_size(GIB), "1.0 GB");
        assert_eq!(format_size(TIB), "1.00 TB");
        assert_eq!(format_size(PIB), "1.00 PB");
        assert_eq!(format_size(EIB), "1.00 EB");
        assert_eq!(format_size(u64::MAX), "16.00 EB");
    }

    fn trigger(expect: &str, response: &str, repeat: bool) -> SessionTrigger {
        SessionTrigger {
            expect: expect.to_string(),
            response: Secret::new(response),
            append_enter: true,
            repeat,
        }
    }

    #[test]
    fn session_trigger_matches_across_output_chunks_once() {
        let mut engine = TriggerEngine::new(&[trigger("Password:", "secret", false)]);
        assert!(engine.feed("Pass").is_empty());
        let replies = engine.feed("word:");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0.as_str(), "secret");
        assert!(replies[0].1);
        assert!(engine.feed("Password:").is_empty());
    }

    #[test]
    fn repeating_session_trigger_consumes_each_match() {
        let mut engine = TriggerEngine::new(&[trigger("continue?", "y", true)]);
        assert_eq!(engine.feed("continue?").len(), 1);
        assert!(engine.feed("unrelated output").is_empty());
        assert_eq!(engine.feed("continue?").len(), 1);
    }
}

/// Format a Unix timestamp as `YYYY-MM-DD HH:MM`.
pub fn format_mtime(ts: u32) -> String {
    // SFTP mtime is a Unix timestamp (UTC seconds). Render it in the machine's
    // *local* timezone so the displayed time matches the user's wall clock
    // (e.g. UTC+8) instead of showing UTC — which read 8 h early (#168).
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(Local::now);
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// The canonical ZMODEM abort sequence: eight CAN (0x18) then eight BS (0x08).
/// Sending this makes the remote `sz`/`rz` give up so the session recovers (#76).
const ZMODEM_CANCEL: [u8; 16] = [
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
];

const PROMPT_SETUP_PREFIX: &str = "test -z \"$FISH_VERSION\"";
const PROMPT_SETUP_SUFFIX: &str = "__ms7'";
const PROMPT_SETUP_HISTORY_MARKER: &str = "__MEATSHELL_INTERNAL_SETUP_1";
const PROMPT_SETUP_DONE: &str = "\u{1b}]699;ready\u{07}";
const PROMPT_BODY: &str = "test -z \"$FISH_VERSION\" && eval '__msc(){ __c=\"$(fc -ln -1 2>/dev/null)\"; [ -n \"$__c\" ] && [ \"$__c\" != \"$__cl\" ] && { __cl=\"$__c\"; printf \"\\033]697;%s\\007\" \"$__c\"; }; }; __ms7(){ printf \"\\033]7;file://%s%s\\007\" \"$HOSTNAME\" \"$PWD\"; __msc; }; if [ -n \"$ZSH_VERSION\" ]; then autoload -Uz add-zsh-hook 2>/dev/null; add-zsh-hook precmd __ms7; else PROMPT_COMMAND=\"__ms7${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; fi; : __MEATSHELL_INTERNAL_SETUP_1; if [ -n \"$BASH_VERSION\" ]; then __md=\"$(history 2>/dev/null | { __md=\"\"; while read -r __mn __mr; do case \"$__mr\" in *\"__ms7()\"*\"PROMPT_COMMAND=\"*) __mn=\"${__mn%\\*}\"; __md=\"$__mn $__md\";; esac; done; printf \"%s\" \"$__md\"; })\"; for __mn in $__md; do history -d \"$__mn\" 2>/dev/null; done; unset __md __mn __mr; fi; __cl=\"$(fc -ln -1 2>/dev/null)\"; printf \"\\033]699;ready\\007\"; __ms7'";
const PROMPT_SHELL_PROBE: &[u8] = b"if [ -n \"$BASH_VERSION\" ]; then printf '__MEATSHELL_SHELL__:bash\\n'; elif [ -n \"$ZSH_VERSION\" ]; then printf '__MEATSHELL_SHELL__:zsh\\n'; else printf '__MEATSHELL_SHELL__:other\\n'; fi";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuxiliaryChannelKind {
    Resources,
    Processes,
    SystemInfo,
}

impl AuxiliaryChannelKind {
    fn delay(self) -> std::time::Duration {
        match self {
            Self::Resources => std::time::Duration::from_millis(750),
            Self::Processes => std::time::Duration::from_millis(1500),
            Self::SystemInfo => std::time::Duration::from_millis(2500),
        }
    }

    fn next(self) -> Option<Self> {
        match self {
            Self::Resources => Some(Self::Processes),
            Self::Processes => Some(Self::SystemInfo),
            Self::SystemInfo => None,
        }
    }
}

struct AuxiliaryStartup {
    pending: Option<AuxiliaryChannelKind>,
    in_progress: bool,
}

impl AuxiliaryStartup {
    fn new(enabled: bool) -> Self {
        Self {
            pending: enabled.then_some(AuxiliaryChannelKind::Resources),
            in_progress: false,
        }
    }

    fn pending(&self) -> Option<AuxiliaryChannelKind> {
        if self.in_progress {
            None
        } else {
            self.pending
        }
    }

    fn begin(&mut self) -> Option<AuxiliaryChannelKind> {
        let kind = self.pending()?;
        if self.in_progress {
            return None;
        }
        self.in_progress = true;
        Some(kind)
    }

    fn complete(&mut self) {
        if !self.in_progress {
            return;
        }
        self.pending = self.pending.and_then(AuxiliaryChannelKind::next);
        self.in_progress = false;
    }
}

const AUXILIARY_CHANNEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

async fn auxiliary_timeout<F, T>(
    timeout: std::time::Duration,
    future: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(timeout, future).await
}

async fn open_auxiliary_channel(
    handle: &Handle<ClientHandler>,
    command: &'static [u8],
    label: &'static str,
) -> Option<Channel<Msg>> {
    let operation = async {
        let channel = handle
            .channel_open_session()
            .await
            .with_context(|| format!("{label} channel open"))?;
        channel
            .exec(true, command)
            .await
            .with_context(|| format!("{label} exec"))?;
        Ok::<Channel<Msg>, anyhow::Error>(channel)
    };

    match auxiliary_timeout(AUXILIARY_CHANNEL_TIMEOUT, operation).await {
        Ok(Ok(channel)) => Some(channel),
        Ok(Err(error)) => {
            tracing::warn!("{label} startup failed: {error:#}");
            None
        }
        Err(_) => {
            tracing::warn!(
                "{label} startup timed out after {} ms",
                AUXILIARY_CHANNEL_TIMEOUT.as_millis()
            );
            None
        }
    }
}

fn prompt_setup_supported(probe_output: &str) -> Option<bool> {
    if probe_output.contains("__MEATSHELL_SHELL__:bash")
        || probe_output.contains("__MEATSHELL_SHELL__:zsh")
    {
        Some(true)
    } else if probe_output.contains("__MEATSHELL_SHELL__:other") {
        Some(false)
    } else {
        None
    }
}

/// Probe the login shell through a separate exec channel so unsupported shells
/// never see the long interactive prompt-integration command. In particular,
/// BusyBox ash (used by OpenWrt) ignores `PROMPT_COMMAND`; injecting into its
/// line editor only risks a visible partial command or continuation prompt.
async fn remote_supports_prompt_setup(handle: &Handle<ClientHandler>) -> bool {
    let probe = async {
        let mut channel = handle.channel_open_session().await.ok()?;
        channel.exec(true, PROMPT_SHELL_PROBE).await.ok()?;
        let _ = channel.eof().await;

        let mut output = String::new();
        let mut supported: Option<bool> = None;
        // Drain the channel fully, including the server's CHANNEL_CLOSE. Do not
        // return as soon as the marker is seen (the old code dropped the Channel
        // mid-flight) and do not just send `channel.close()` without awaiting the
        // peer's confirmation: some servers reuse channel IDs aggressively and
        // will tear down the *next* channel (the interactive shell) if it is
        // opened while this one is still being torn down. Draining to Close
        // serializes the teardown and avoids that race.
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    if supported.is_none() {
                        output.push_str(&String::from_utf8_lossy(&data));
                        if let Some(s) = prompt_setup_supported(&output) {
                            supported = Some(s);
                        } else if output.len() > 256 {
                            supported = Some(false);
                        }
                    }
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        supported
    };

    tokio::time::timeout(std::time::Duration::from_millis(1000), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZmodemDirection {
    /// Remote `sz` sends files; MeatShell receives them.
    Download,
    /// Remote `rz` receives files; MeatShell sends selected local files.
    Upload,
}

/// Identify the first hex ZMODEM handshake by its actual frame type. Remote
/// `sz` starts with ZRQINIT (`00`), while remote `rz` starts with ZRINIT (`01`).
/// Treating both as a receive operation made `rz` deadlock because each side
/// waited for the other to start sending (#308).
fn zmodem_direction(data: &[u8]) -> Option<ZmodemDirection> {
    data.windows(4).find_map(|window| {
        if window[0] != 0x18 || window[1] != b'B' {
            return None;
        }
        let high = zmodem_hex_nibble(window[2])?;
        let low = zmodem_hex_nibble(window[3])?;
        match (high << 4) | low {
            0 => Some(ZmodemDirection::Download),
            1 => Some(ZmodemDirection::Upload),
            _ => None,
        }
    })
}

fn zmodem_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn line_start_before(text: &str, pos: usize) -> usize {
    text[..pos].rfind(['\r', '\n']).map(|i| i + 1).unwrap_or(0)
}

fn include_following_line_break(text: &str, mut pos: usize) -> usize {
    let bytes = text.as_bytes();
    if pos < bytes.len() && bytes[pos] == b'\r' {
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }
    } else if pos < bytes.len() && bytes[pos] == b'\n' {
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b'\r' {
            pos += 1;
        }
    }
    pos
}

#[cfg(test)]
fn prompt_setup_echo_end(text: &str, prefix_pos: usize) -> usize {
    if let Some(rel) = text[prefix_pos..].find(PROMPT_SETUP_SUFFIX) {
        return include_following_line_break(text, prefix_pos + rel + PROMPT_SETUP_SUFFIX.len());
    }
    let line_end = text[prefix_pos..]
        .find(['\r', '\n'])
        .map(|i| prefix_pos + i)
        .unwrap_or(text.len());
    include_following_line_break(text, line_end)
}

fn strip_prompt_setup_echo(text: &mut String, prefix_pos: usize, end_pos: usize) {
    let start = line_start_before(text, prefix_pos);
    let end = include_following_line_break(text, end_pos.min(text.len()));
    // The remote PTY has already echoed the hidden setup command and advanced
    // its cursor through that line. Removing the bytes outright leaves our
    // local vt100 parser at the old prompt column, so readline's later relative
    // backspaces repaint history commands beside one another (#289). Reset and
    // clear the current local row before feeding the final prompt that follows.
    text.replace_range(start..end, "\r\x1b[2K");
}

/// Remove a late-echoed prompt setup command when it arrives after the initial
/// suppression window. Some shells echo a long injected command only after the
/// first prompt has already been delivered, so the normal buffered path cannot
/// catch it (#266).
fn strip_late_prompt_setup_echo(text: &mut String) -> bool {
    let Some(prefix_pos) = text.find(PROMPT_SETUP_PREFIX) else {
        return false;
    };
    let Some(rel_end) = text[prefix_pos..].find(PROMPT_SETUP_SUFFIX) else {
        return false;
    };
    let end = prefix_pos + rel_end + PROMPT_SETUP_SUFFIX.len();
    strip_prompt_setup_echo(text, prefix_pos, end);
    true
}

fn strip_pending_prompt_setup_echo(text: &mut String, pending: &mut bool) -> bool {
    if !*pending || !strip_late_prompt_setup_echo(text) {
        return false;
    }
    *pending = false;
    true
}

/// Consume all buffered setup echo through the private completion marker.
/// The marker is emitted by the executed command, unlike its printable escaped
/// representation in the echoed input, so it remains reliable across zsh/ZLE
/// redraws, wrapping, and arbitrary chunk boundaries (#344).
fn take_after_prompt_setup_done(text: &mut String) -> Option<String> {
    let marker = text.find(PROMPT_SETUP_DONE)?;
    let tail = text.split_off(marker + PROMPT_SETUP_DONE.len());
    text.clear();
    Some(tail)
}

fn bound_prompt_setup_echo(text: &mut String) {
    const KEEP_CHARS: usize = 64;
    const MAX_BUFFER: usize = 64 * 1024;
    if text.len() <= MAX_BUFFER {
        return;
    }
    // Everything before the completion marker is private setup echo. Retain a
    // short suffix only so a marker split across channel chunks still matches.
    let mut tail: String = text.chars().rev().take(KEEP_CHARS).collect();
    tail = tail.chars().rev().collect();
    *text = tail;
}

/// Extract the remote path from an OSC 7 sequence embedded in `text`.
///
/// Format: `ESC ] 7 ; file://hostname/path BEL`
/// Returns the decoded absolute path component (without hostname).
pub fn extract_osc7_path(text: &str) -> Option<String> {
    extract_osc7_end(text).map(|(path, _)| path)
}

/// Like [`extract_osc7_path`] but also returns the byte index just past the OSC
/// sequence's terminator, so the caller can cut everything up to and including
/// it — used to discard the echoed setup line (which may wrap) at connect (#98).
fn extract_osc7_end(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let osc_start = i + 2;
        i += 2;
        // Scan for BEL (0x07) or ST (ESC \)
        let mut end = i;
        let mut term_len = 0;
        while end < bytes.len() {
            if bytes[end] == 0x07 {
                term_len = 1;
                break;
            } else if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                term_len = 2;
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        if let Ok(content) = std::str::from_utf8(&bytes[osc_start..end]) {
            if let Some(rest) = content.strip_prefix("7;file://") {
                // rest = "hostname/path" or "/path" (empty hostname)
                let path = if rest.starts_with('/') {
                    rest.to_string()
                } else if let Some(slash) = rest.find('/') {
                    rest[slash..].to_string()
                } else {
                    "/".to_string()
                };
                return Some((url_decode(&path), end + term_len));
            }
        }
        i = end + term_len.max(1);
    }
    None
}

/// Find a meatshell command-capture sequence (`ESC ] 697 ; <command> BEL|ST`)
/// emitted by the shell hook (#113). Returns the command text and the byte
/// range of the whole escape sequence, so the caller can strip it before the
/// text is rendered. An incomplete sequence (terminator not yet received)
/// yields `None` — vt100 buffers it and the next chunk completes it.
pub fn extract_osc_command(text: &str) -> Option<(String, std::ops::Range<usize>)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let seq_start = i;
        let osc_start = i + 2;
        i += 2;
        // Scan for BEL (0x07) or ST (ESC \).
        let mut end = i;
        let mut term_len = 0;
        while end < bytes.len() {
            if bytes[end] == 0x07 {
                term_len = 1;
                break;
            } else if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                term_len = 2;
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            break; // incomplete — leave it for the next chunk
        }
        if let Ok(content) = std::str::from_utf8(&bytes[osc_start..end]) {
            if let Some(cmd) = content.strip_prefix("697;") {
                return Some((cmd.to_string(), seq_start..end + term_len));
            }
        }
        i = end + term_len;
    }
    None
}

/// Percent-decode a URL path segment (e.g. `%20` → space).
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            match (h1, h2) {
                (Some(a), Some(b)) => {
                    let hex = format!("{a}{b}");
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        result.push(byte as char);
                    } else {
                        result.push('%');
                        result.push(a);
                        result.push(b);
                    }
                }
                (Some(a), None) => {
                    result.push('%');
                    result.push(a);
                }
                _ => result.push('%'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

async fn kill_remote_process(
    handle: Arc<Handle<ClientHandler>>,
    pid: u32,
    root_password: Option<crate::config::Secret>,
) -> ProcessKillResult {
    use zeroize::Zeroize as _;

    let privileged = root_password.is_some();
    let stage = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let operation_stage = stage.clone();
    let operation = async move {
        let started = std::time::Instant::now();
        tracing::warn!("[PROC_KILL] pid={pid} privileged={privileged} stage=open-channel begin");
        let mut channel = handle
            .channel_open_session()
            .await
            .context("open process-control channel")?;
        operation_stage.store(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "[PROC_KILL] pid={pid} stage=open-channel ok elapsed_ms={}",
            started.elapsed().as_millis()
        );
        if privileged {
            // `sudo` authentication is commonly configured by PAM to require a
            // controlling terminal. Disable echo at the SSH PTY level so the
            // password can never be reflected into channel output or logs.
            channel
                .request_pty(true, "xterm", 80, 24, 0, 0, &[(russh::Pty::ECHO, 0)])
                .await
                .context("request process-control terminal")?;
            operation_stage.store(2, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                "[PROC_KILL] pid={pid} stage=request-pty ok echo=off elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        let command = process_kill_command(pid, privileged);
        channel
            .exec(true, command.as_bytes())
            .await
            .context("execute process-control command")?;
        operation_stage.store(3, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "[PROC_KILL] pid={pid} stage=exec-sudo ok elapsed_ms={} waiting_for_password_prompt={privileged}",
            started.elapsed().as_millis()
        );
        if !privileged {
            channel
                .eof()
                .await
                .context("finish process-control input")?;
        }

        let mut response = String::new();
        let mut password_sent = !privileged;
        let prompt_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let exit_status = loop {
            let msg = if !password_sent {
                match tokio::time::timeout_at(prompt_deadline, channel.wait()).await {
                    Ok(msg) => msg,
                    Err(_) => {
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=wait-password-prompt timeout; sending password fallback"
                        );
                        if let Some(password) = root_password.as_ref() {
                            let mut input = password.as_str().as_bytes().to_vec();
                            input.push(b'\r');
                            let sent = channel.data(&input[..]).await;
                            input.zeroize();
                            sent.context("write root password after prompt timeout")?;
                        }
                        password_sent = true;
                        operation_stage.store(5, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                }
            } else {
                channel.wait().await
            };
            let Some(msg) = msg else { break 1 };
            match msg {
                // ExitStatus is the authoritative completion result. Some SSH
                // servers keep a PTY channel open and never promptly follow it
                // with Close, so waiting beyond this point causes a false timeout.
                ChannelMsg::ExitStatus {
                    exit_status: status,
                } => {
                    operation_stage.store(6, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        "[PROC_KILL] pid={pid} stage=exit-status status={status} elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    break status;
                }
                ChannelMsg::Close => {
                    tracing::warn!(
                        "[PROC_KILL] pid={pid} stage=channel-close without-exit-status elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    break 1;
                }
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    let text = String::from_utf8_lossy(&data);
                    let safe = process_control_log_text(
                        &text,
                        root_password.as_ref().map(|secret| secret.as_str()),
                    );
                    if !safe.is_empty() {
                        tracing::warn!("[PROC_KILL] pid={pid} stage=remote-output text={safe:?}");
                    }
                    if response.len() < 1024 {
                        response.push_str(&text);
                        response.truncate(response.len().min(1024));
                    }
                    if !password_sent && looks_like_sudo_password_prompt(&text) {
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=password-prompt detected; submitting secret"
                        );
                        if let Some(password) = root_password.as_ref() {
                            let mut input = password.as_str().as_bytes().to_vec();
                            input.push(b'\r');
                            let sent = channel.data(&input[..]).await;
                            input.zeroize();
                            sent.context("write root password after prompt")?;
                        }
                        password_sent = true;
                        operation_stage.store(5, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=password-submitted elapsed_ms={}",
                            started.elapsed().as_millis()
                        );
                    }
                }
                _ => {}
            }
        };
        anyhow::Ok((exit_status, response))
    };

    let result = match tokio::time::timeout(std::time::Duration::from_secs(15), operation).await {
        Ok(Ok((0, _))) => ProcessKillResult {
            success: true,
            message: format!("{} PID {pid}", t("已发送 SIGTERM：", "SIGTERM sent to")),
        },
        Ok(Ok((_, response))) if privileged => ProcessKillResult {
            success: false,
            message: process_kill_failure_message(&response, true),
        },
        Ok(Ok((_, response))) => ProcessKillResult {
            success: false,
            message: process_kill_failure_message(&response, false),
        },
        Ok(Err(err)) => ProcessKillResult {
            success: false,
            message: format!(
                "{}: {err}",
                t("结束进程失败", "Failed to terminate process")
            ),
        },
        Err(_) => {
            let stage =
                process_control_stage_name(stage.load(std::sync::atomic::Ordering::Relaxed));
            tracing::warn!("[PROC_KILL] pid={pid} result=timeout stage={stage}");
            ProcessKillResult {
                success: false,
                message: format!(
                    "{} ({stage})",
                    t(
                        "结束进程超时，诊断已写入 error.log",
                        "Timed out; diagnostics were written to error.log"
                    )
                ),
            }
        }
    };
    tracing::warn!(
        "[PROC_KILL] pid={pid} result={} message={:?}",
        if result.success { "success" } else { "failure" },
        result.message
    );
    result
}

fn process_control_stage_name(stage: u8) -> &'static str {
    match stage {
        0 => "open-channel",
        1 => "request-pty",
        2 => "exec-sudo",
        3 => "wait-password-prompt",
        5 => "wait-exit-status",
        6 => "completed",
        _ => "unknown",
    }
}

fn looks_like_sudo_password_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("password") || lower.contains("密码")
}

fn process_control_log_text(text: &str, password: Option<&str>) -> String {
    let mut safe = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    if let Some(password) = password.filter(|value| !value.is_empty()) {
        safe = safe.replace(password, "[REDACTED]");
    }
    safe.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}

fn process_kill_failure_message(response: &str, privileged: bool) -> String {
    let detail = response
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !detail.is_empty() {
        return format!("{}: {detail}", t("结束失败", "Failed to terminate process"));
    }
    if privileged {
        t(
            "结束失败：服务器未返回具体的 sudo/PAM 错误",
            "Failed: the server returned no specific sudo/PAM error",
        )
        .to_string()
    } else {
        t(
            "结束失败：进程已退出或无权操作",
            "Failed: the process exited or permission was denied",
        )
        .to_string()
    }
}

fn process_kill_command(pid: u32, privileged: bool) -> String {
    if privileged {
        // `sudo` authenticates the connected account, matching what users run
        // manually. `su root` instead asks for the root account password, which
        // is commonly locked even when the user is an authorised sudoer.
        format!("LC_ALL=C sudo -S -p 'Password:' -- kill -TERM {pid}")
    } else {
        format!("kill -TERM {pid}")
    }
}

/// Entry point: spawn a session on the shared tokio runtime.
///
/// `initial_cols` / `initial_rows` are the PTY dimensions to request when
/// opening the channel. Slint fires a `terminal-resize` callback very shortly
/// after the tab becomes active; passing the best-known size here avoids the
/// remote shell starting at a stale 80×24 and sending an extra SIGWINCH.
///
/// Returns a [`SessionHandle`] for the UI + an [`UnboundedReceiver`] the UI
/// should drain on the Slint event loop.
pub fn spawn_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    jump: Option<Session>,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_tx_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_session(
            session,
            jump,
            cmd_rx,
            evt_tx_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
        {
            tracing::warn!("ssh session ended with error: {err:#}");
            let _ = evt_tx_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle {
            tab_id,
            commands: cmd_tx,
            join,
        },
        evt_rx,
    )
}

struct RuntimeForward {
    info: RuntimeTunnelInfo,
    task: Option<JoinHandle<()>>,
}

fn normalized_bind_addr(f: &PortForward) -> String {
    let bind = f.bind_addr.trim();
    if bind.is_empty() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    }
}

fn tunnel_label(f: &PortForward) -> String {
    if !f.name.trim().is_empty() {
        return f.name.trim().to_string();
    }
    match f.kind.as_str() {
        "local" => format!("-L {}:{}", normalized_bind_addr(f), f.bind_port),
        "remote" => format!("-R {}:{}", normalized_bind_addr(f), f.bind_port),
        "dynamic" => format!("-D {}:{}", normalized_bind_addr(f), f.bind_port),
        _ => format!("{} {}:{}", f.kind, normalized_bind_addr(f), f.bind_port),
    }
}

fn tunnel_info(id: String, f: &PortForward, active: bool, status: &str) -> RuntimeTunnelInfo {
    RuntimeTunnelInfo {
        id,
        name: tunnel_label(f),
        kind: f.kind.clone(),
        bind_addr: normalized_bind_addr(f),
        bind_port: f.bind_port,
        host: f.host.trim().to_string(),
        host_port: f.host_port,
        active,
        status: status.to_string(),
    }
}

fn emit_tunnel_update(
    forwards: &std::collections::HashMap<String, RuntimeForward>,
    events: &UnboundedSender<SessionEvent>,
) {
    let mut rows: Vec<RuntimeTunnelInfo> = forwards.values().map(|f| f.info.clone()).collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let _ = events.send(SessionEvent::TunnelUpdate(rows));
}

fn start_runtime_forward(
    handle: Arc<Handle<ClientHandler>>,
    id: String,
    forward: PortForward,
    events: &UnboundedSender<SessionEvent>,
) -> RuntimeForward {
    let info = tunnel_info(id, &forward, true, t("运行中", "running"));
    let task = match forward.kind.as_str() {
        "local" => Some(crate::tunnel::spawn_local(
            handle,
            info.bind_addr.clone(),
            info.bind_port,
            info.host.clone(),
            info.host_port,
            events.clone(),
        )),
        "dynamic" => Some(crate::tunnel::spawn_dynamic(
            handle,
            info.bind_addr.clone(),
            info.bind_port,
            events.clone(),
        )),
        _ => None,
    };
    RuntimeForward { info, task }
}

/// Open an SSH transport to the session's host (directly or via a SOCKS5 / HTTP
/// proxy) and return the russh handle, ready for authentication. Factored out so
/// the keyboard-interactive fallback can reconnect on a *fresh* handle — russh
/// hangs if a second auth method is attempted on a handle whose first attempt
/// already failed (#86).
async fn connect_ssh(
    session: &Session,
    jump: Option<&Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(Handle<ClientHandler>, Option<Handle<ClientHandler>>)> {
    // Remote (-R) forwards are serviced inside the handler when the server opens
    // channels back, so it needs the bind-port → local-target map up front (the
    // handler is moved into `connect`) (#56).
    let remote_forwards: std::collections::HashMap<u32, (String, u16)> = session
        .forwards
        .iter()
        .filter(|f| f.kind == "remote")
        .map(|f| (f.bind_port as u32, (f.host.clone(), f.host_port)))
        .collect();
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
        remote_forwards,
        events: events.clone(),
    };
    let addr = format!("{}:{}", session.host, session.port);

    // SSH jump host (bastion): connect + authenticate the jump session, then open
    // a direct-tcpip channel through it to this host and run the SSH handshake
    // over that tunnel. The returned jump handle must be kept alive for the whole
    // session (the tunnel lives on it) (#211).
    if let Some(j) = jump {
        let _ = events.send(SessionEvent::Status(format!(
            "{} {}@{} → {}",
            t("经跳板机连接", "via jump host"),
            j.user,
            j.host,
            addr
        )));
        let (handle, jump_handle) =
            connect_target_via_jump(j, &session.host, session.port, config, handler, events)
                .await
                .with_context(|| format!("connect {} via jump failed", addr))?;
        return Ok((handle, Some(jump_handle)));
    }

    // Connect directly, or tunnel through a SOCKS5 / HTTP proxy (issue #7).
    let handle = match crate::ssh::proxy::resolve(&session.proxy) {
        Some(p) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} → {}",
                t("经代理连接", "via proxy"),
                crate::ssh::proxy::describe(&p),
                addr
            )));
            let stream = crate::ssh::proxy::connect(&p, &session.host, session.port)
                .await
                .with_context(|| format!("proxy connect to {} failed", addr))?;
            client::connect_stream(config, stream, handler)
                .await
                .with_context(|| format!("connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), handler)
            .await
            .with_context(|| format!("connect {} failed", addr))?,
    };
    Ok((handle, None))
}

/// Outcome of authenticating an SSH session, so callers can distinguish a user
/// cancel from a credential rejection and word the status line accordingly.
pub(crate) enum AuthResult {
    Success,
    Cancelled,
    Failed,
}

/// Authenticate an already-connected SSH handle using the session's method,
/// prompting for missing credentials and supporting explicit / fallback
/// `keyboard-interactive` auth (#86, #249). Shared by the shell, SFTP and
/// jump-host paths. On the keyboard-interactive fallback it reconnects, updating
/// both `handle` and `jump_handle` in place so the caller keeps the live tunnel.
pub(crate) async fn authenticate_session(
    handle: &mut Handle<ClientHandler>,
    jump_handle: &mut Option<Handle<ClientHandler>>,
    session: &Session,
    jump: Option<&Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
) -> Result<AuthResult> {
    let (user, password) = match resolve_credentials(session, events).await {
        Some(c) => c,
        None => return Ok(AuthResult::Cancelled),
    };

    let authed = match session.auth {
        AuthMethod::Password => {
            let mut ok = handle
                .authenticate_password(&user, password.as_str())
                .await
                .context("password auth failed")?;
            if !ok {
                // russh can't switch auth methods on a handle whose first attempt
                // already failed (it hangs), so reconnect on a fresh handle before
                // trying keyboard-interactive (#86).
                let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
                let (h, jh) = Box::pin(connect_ssh(session, jump, config.clone(), events)).await?;
                *handle = h;
                *jump_handle = jh;
                ok = keyboard_interactive_auth(
                    handle,
                    &user,
                    password.as_str(),
                    &session.id,
                    &session.host,
                    events,
                )
                .await
                .context("keyboard-interactive auth failed")?;
            }
            ok
        }
        AuthMethod::KeyboardInteractive => keyboard_interactive_auth(
            handle,
            &user,
            password.as_str(),
            &session.id,
            &session.host,
            events,
        )
        .await
        .context("keyboard-interactive auth failed")?,
        AuthMethod::Key => {
            // An encrypted private key needs its passphrase; we reuse the
            // session's password field for it (empty = unencrypted key) (#90).
            let pass = password.as_str();
            let keypair = load_session_private_key(session, pass)?;
            // RSA keys must be signed with an explicit SHA-2 hash; every other
            // key type carries its own algorithm, so no override is needed.
            let hash = keypair.algorithm().is_rsa().then_some(HashAlg::Sha256);
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash)
                .context("invalid private key / hash algorithm combination")?;
            handle
                .authenticate_publickey(&user, key_with_hash)
                .await
                .context("publickey auth failed")?
        }
    };

    if authed {
        Ok(AuthResult::Success)
    } else {
        Ok(AuthResult::Failed)
    }
}

/// Connect + authenticate a jump/bastion session, open a `direct-tcpip` channel
/// to `target_host:target_port`, and run the target's SSH handshake over it.
/// Returns the target handle plus the jump handle, which the caller MUST keep
/// alive for as long as the target session lives (the tunnel rides on it) (#211).
pub(crate) async fn connect_target_via_jump<H>(
    jump: &Session,
    target_host: &str,
    target_port: u16,
    config: Arc<client::Config>,
    handler: H,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(Handle<H>, Handle<ClientHandler>)>
where
    H: client::Handler + 'static,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    // Single hop: the jump session itself never goes through another jump.
    // `Box::pin` breaks the async recursion (connect_ssh → jump → connect_ssh).
    let (mut jhandle, mut no_nested) = Box::pin(connect_ssh(jump, None, config.clone(), events))
        .await
        .with_context(|| format!("connect jump host {}:{} failed", jump.host, jump.port))?;
    match authenticate_session(
        &mut jhandle,
        &mut no_nested,
        jump,
        None,
        config.clone(),
        events,
    )
    .await?
    {
        AuthResult::Success => {}
        AuthResult::Cancelled => {
            return Err(anyhow!(t("跳板机登录已取消", "jump host login cancelled")))
        }
        AuthResult::Failed => {
            return Err(anyhow!(t(
                "跳板机认证失败",
                "jump host authentication failed"
            )))
        }
    }
    let channel = jhandle
        .channel_open_direct_tcpip(
            target_host.to_string(),
            target_port as u32,
            "127.0.0.1".to_string(),
            0,
        )
        .await
        .with_context(|| format!("open jump tunnel to {target_host}:{target_port}"))?;
    let handle = client::connect_stream(config, channel.into_stream(), handler)
        .await
        .with_context(|| format!("SSH handshake to {target_host}:{target_port} via jump"))?;
    Ok((handle, jhandle))
}

// Key-exchange algorithms offered to the server, strongest first. This is the
// russh default set PLUS the ecdh-sha2-nistp* curves and the legacy
// diffie-hellman-group{14,1}-sha1 exchanges appended as last-resort fallbacks, so
// we can still reach old servers / network gear that only speak SHA-1 KEX and
// otherwise fail with "No common algorithm" (#172). Modern servers still pick a
// strong algorithm because the client's order decides and SHA-1 is last.
pub(crate) const COMPAT_KEX: &[russh::kex::Name] = &[
    russh::kex::CURVE25519,
    russh::kex::CURVE25519_PRE_RFC_8731,
    russh::kex::DH_G16_SHA512,
    russh::kex::DH_G14_SHA256,
    russh::kex::ECDH_SHA2_NISTP256,
    russh::kex::ECDH_SHA2_NISTP384,
    russh::kex::ECDH_SHA2_NISTP521,
    russh::kex::DH_G14_SHA1, // legacy fallback
    russh::kex::DH_G1_SHA1,  // legacy fallback
    // Keep the OpenSSH ext-info / strict-kex markers so modern servers still
    // negotiate ext-info and strict kex (mirrors russh's default tail).
    russh::kex::EXTENSION_SUPPORT_AS_CLIENT,
    russh::kex::EXTENSION_SUPPORT_AS_SERVER,
    russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
    russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
];

// Ciphers offered to the server, strongest first: russh's AEAD/CTR defaults plus
// the legacy CBC ciphers appended for old servers that only support CBC (#172).
pub(crate) const COMPAT_CIPHER: &[russh::cipher::Name] = &[
    russh::cipher::CHACHA20_POLY1305,
    russh::cipher::AES_256_GCM,
    russh::cipher::AES_256_CTR,
    russh::cipher::AES_192_CTR,
    russh::cipher::AES_128_CTR,
    russh::cipher::AES_256_CBC,    // legacy fallback
    russh::cipher::AES_192_CBC,    // legacy fallback
    russh::cipher::AES_128_CBC,    // legacy fallback
    russh::cipher::TRIPLE_DES_CBC, // legacy fallback
];

fn ssh_client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Keep idle connections alive (#160). The terminal usually has the
        // resource-monitor channel streaming every 2 s, but with shell
        // integration disabled (#140) it can go idle and be dropped by
        // NAT / firewall / server timeouts.
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        // Match the normal terminal connection exactly, including compatibility
        // fallbacks for older servers and network equipment (#172).
        preferred: russh::Preferred {
            kex: std::borrow::Cow::Borrowed(COMPAT_KEX),
            cipher: std::borrow::Cow::Borrowed(COMPAT_CIPHER),
            ..russh::Preferred::DEFAULT
        },
        ..<_>::default()
    })
}

/// Perform the same SSH handshake and authentication as a real terminal
/// connection, but disconnect immediately after authentication succeeds.
/// Prompt events are returned through `events` so the session dialog can reuse
/// the normal host-key, missing-credential, and MFA UI (#276).
pub async fn test_session_auth(
    session: Session,
    jump: Option<Session>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let config = ssh_client_config();
    let (mut handle, mut jump_handle) =
        connect_ssh(&session, jump.as_ref(), config.clone(), &events).await?;

    let auth = authenticate_session(
        &mut handle,
        &mut jump_handle,
        &session,
        jump.as_ref(),
        config,
        &events,
    )
    .await?;

    let result = match auth {
        AuthResult::Success => Ok(()),
        AuthResult::Cancelled => Err(anyhow!("login cancelled")),
        AuthResult::Failed => Err(anyhow!("authentication failed")),
    };

    let _ = handle
        .disconnect(Disconnect::ByApplication, "connection test complete", "")
        .await;
    if let Some(jump_handle) = jump_handle {
        let _ = jump_handle
            .disconnect(Disconnect::ByApplication, "connection test complete", "")
            .await;
    }
    result
}

/// Result of a non-interactive SSH command used by automation frontends.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommandExecution {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<u32>,
    pub timed_out: bool,
    pub truncated: bool,
}

/// Execute one command through the same transport, proxy, jump-host, host-key,
/// and authentication path as an interactive MeatShell session.
///
/// The caller must only pass a session after enforcing its own permission
/// policy. Missing credentials and unknown/changed host keys fail closed because
/// this headless path has no UI in which to ask the user.
pub async fn execute_command(
    session: Session,
    jump: Option<Session>,
    command: &str,
    timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<CommandExecution> {
    let (events, event_rx) = mpsc::unbounded_channel();
    drop(event_rx);
    let config = ssh_client_config();
    let (mut handle, mut jump_handle) =
        connect_ssh(&session, jump.as_ref(), config.clone(), &events).await?;

    match authenticate_session(
        &mut handle,
        &mut jump_handle,
        &session,
        jump.as_ref(),
        config,
        &events,
    )
    .await?
    {
        AuthResult::Success => {}
        AuthResult::Cancelled => return Err(anyhow!("login cancelled")),
        AuthResult::Failed => return Err(anyhow!("authentication failed")),
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .context("open command channel")?;
    channel
        .exec(true, command.as_bytes())
        .await
        .context("execute remote command")?;

    let collect = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        let mut truncated = false;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    append_bounded(&mut stdout, &data, max_output_bytes, &mut truncated);
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    append_bounded(&mut stderr, &data, max_output_bytes, &mut truncated);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(exit_status);
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        CommandExecution {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code,
            timed_out: false,
            truncated,
        }
    };

    let result = match tokio::time::timeout(timeout, collect).await {
        Ok(result) => result,
        Err(_) => CommandExecution {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            truncated: false,
        },
    };
    let _ = handle
        .disconnect(Disconnect::ByApplication, "command complete", "")
        .await;
    if let Some(jump_handle) = jump_handle {
        let _ = jump_handle
            .disconnect(Disconnect::ByApplication, "command complete", "")
            .await;
    }
    Ok(result)
}

fn append_bounded(target: &mut Vec<u8>, data: &[u8], limit: usize, truncated: &mut bool) {
    let remaining = limit.saturating_sub(target.len());
    let take = remaining.min(data.len());
    target.extend_from_slice(&data[..take]);
    *truncated |= take < data.len();
}

async fn run_session(
    session: Session,
    jump: Option<Session>,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
    let session_started = std::time::Instant::now();
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}:{} ...",
        t("连接中", "Connecting"),
        session.user,
        session.host,
        session.port
    )));

    let config = ssh_client_config();

    let (mut handle, mut jump_handle) =
        connect_ssh(&session, jump.as_ref(), config.clone(), &events).await?;
    tracing::info!(
        "[SESSION_START] id={} stage=transport-ready elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    // --- Auth (shared with SFTP + jump-host paths) ---------------------
    // Try plain `password` first, then `keyboard-interactive` on a fresh handle —
    // many bastions (JumpServer) disable `password` (#86). Missing credentials
    // are prompted for (#110).
    match authenticate_session(
        &mut handle,
        &mut jump_handle,
        &session,
        jump.as_ref(),
        config.clone(),
        &events,
    )
    .await?
    {
        AuthResult::Success => {}
        AuthResult::Cancelled => {
            let _ = events.send(SessionEvent::Closed(
                t("已取消登录", "login cancelled").into(),
            ));
            let _ = handle
                .disconnect(Disconnect::ByApplication, "cancelled", "")
                .await;
            return Ok(());
        }
        AuthResult::Failed => {
            tracing::warn!(
                "ssh authentication failed for {}@{}",
                session.user,
                session.host
            );
            let _ = events.send(SessionEvent::Closed(
                t("认证失败", "authentication failed").into(),
            ));
            let _ = handle
                .disconnect(Disconnect::ByApplication, "auth failed", "")
                .await;
            return Ok(());
        }
    };
    tracing::info!(
        "[SESSION_START] id={} stage=authenticated elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    // Keep the jump-host connection alive for the whole session — the direct-tcpip
    // tunnel that carries this session rides on it (#211).
    let _jump_keepalive = jump_handle;

    // The integration body is Bash/Zsh-specific. Probe out-of-band before the
    // interactive channel exists, so ash/dash/fish/unknown shells never receive
    // (and therefore can never display or get stuck parsing) the setup command.
    let prompt_setup_supported =
        !session.disable_shell_integration && remote_supports_prompt_setup(&handle).await;

    // --- Shell channel --------------------------------------------------
    let mut channel = handle
        .channel_open_session()
        .await
        .context("open session channel")?;

    channel
        .request_pty(
            true,
            "xterm-256color",
            initial_cols,
            initial_rows,
            0,
            0,
            &[],
        )
        .await
        .context("request PTY")?;
    channel.request_shell(true).await.context("request shell")?;

    tracing::info!(
        "[SESSION_START] id={} stage=terminal-ready elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}",
        t("已连接", "Connected"),
        session.user,
        session.host
    )));

    // Whether we have already injected the PROMPT_COMMAND setup.
    // We wait for the first non-empty data chunk (the initial shell prompt)
    // before sending so the command doesn't interleave with banner text.
    let mut prompt_injected = false;
    // True from injecting PROMPT_SETUP until the echoed setup line has been
    // received and stripped; output is buffered (not shown) during that window.
    let mut suppress_echo = false;
    // Buffers output while `suppress_echo` so the (long) echoed setup line can be
    // stripped even when it splits across reads (#98).
    let mut echo_buf = String::new();
    // `strip_late_prompt_setup_echo` must only run while an initial setup echo
    // can genuinely still be in flight. Leaving it enabled for the whole SSH
    // session makes recalling an accidentally saved setup command clear normal
    // terminal rows (#289).
    let mut late_prompt_echo_pending = false;
    // After a ZMODEM transfer finishes we briefly ignore ZMODEM detection so the
    // sender's lingering close frames can't spawn a spurious second receive (#76).
    let mut zmodem_done_at: Option<std::time::Instant> = None;

    // Cwd-notification (OSC 7) setup, injected once after the first prompt so
    // the SFTP panel can follow `cd` (#91). It must work across shells:
    //   • bash/sh  → PROMPT_COMMAND runs `__ms7` before every prompt.
    //   • zsh      → bash's PROMPT_COMMAND is IGNORED by zsh, so we register a
    //                `precmd` hook via `add-zsh-hook` instead (non-destructive —
    //                it preserves oh-my-zsh / p10k hooks, unlike `precmd(){…}`).
    //   • fish     → guarded out (fish 3.1+ emits OSC 7 itself).
    // `__ms7` is called once at the end so the initial cwd arrives immediately.
    //
    // The whole shell-specific body lives inside `eval '…'`: fish can't parse
    // bash/zsh function & `if` syntax, but it CAN parse `eval '<opaque string>'`,
    // and the `test -z "$FISH_VERSION" &&` guard short-circuits before the eval
    // ever runs under fish (#71). The body uses only double quotes inside so the
    // outer single-quoted string needs no escaping; printf turns \033/\007 into
    // ESC/BEL at prompt time. No array syntax → safe to *parse* in dash/ash too.
    //
    // The leading space keeps the line out of shell history (HISTCONTROL=
    // ignorespace, the default on most distros); its echo is stripped locally
    // (the needle below) so the bookkeeping command never shows up.
    //
    // Besides OSC 7 (cwd), the hook also captures the command the user just ran
    // and reports it via a private `OSC 697 ; <cmd> BEL` so it can join the
    // command-box history (#113) — terminal-typed commands aren't otherwise
    // recorded. `__msc` reads the last history entry with `fc -ln -1`; this only
    // ever sees real executed commands, never password prompts (those use
    // `read -s` and aren't shell commands). `__cl` remembers the last reported
    // command so a redrawn prompt (e.g. Enter on an empty line) doesn't re-emit
    // it, and is primed once up front so the pre-session history isn't replayed.
    //
    // The echoed setup line is discarded through the private OSC 699 completion
    // marker emitted after installation (see the suppress block below), so zsh
    // redraws and soft wrapping cannot make the internal command visible.
    let prompt_setup = format!(" {}\r", PROMPT_BODY);
    // --- Remote resource monitor (separate exec channel) ----------------
    // A tiny remote loop streams /proc/stat + /proc/meminfo every 2s; we parse
    // it into CPU% / mem / swap for the sidebar.  Best-effort: if the channel
    // or exec fails (e.g. a non-Linux host without /proc), monitoring is
    // silently skipped and the interactive shell is unaffected.
    // Reset PATH to the standard system directories first (#27): the monitor
    // runs over an exec channel, so a server with a hijacked PATH (or a
    // BASH_ENV pointing at a malicious file) could otherwise shadow awk/cat/df/
    // sleep with arbitrary binaries. A fixed PATH covering /usr/bin and /bin is
    // more portable than hardcoding one absolute path per tool (their location
    // differs across distros). Monitoring is best-effort, so even if this shell
    // is unusual and the reset finds nothing, only the sidebar stats are lost.
    // The `ps` section feeds the process monitor (#23): top-40 by CPU, columns
    // pid/user/pcpu/pmem/args, each line clipped to 200 chars so a giant command
    // line can't bloat the stream. A host whose `ps` lacks `--sort`/`-o` simply
    // yields nothing (2>/dev/null), degrading to an empty process list.
    const MON_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; while :; do awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree|Buffers|Cached):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __MSTICK__; sleep 2; done\n";
    // Detailed system information is intentionally one-shot and last priority.
    // It includes commands such as lspci/hostname that may be slow on some hosts
    // and must never delay either the terminal or the lightweight sidebar sample.
    const SYS_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree|Buffers|Cached):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __SYS__; { . /etc/os-release 2>/dev/null; echo OS=${PRETTY_NAME:-$(uname -o 2>/dev/null)}; }; echo KERNEL=$(uname -s 2>/dev/null); echo KERNEL_RELEASE=$(uname -r 2>/dev/null); echo ARCH=$(uname -m 2>/dev/null); echo HOSTNAME=$(hostname 2>/dev/null); echo IPS=$(hostname -I 2>/dev/null); echo UPTIME=$(uptime -p 2>/dev/null); echo LOAD=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null); awk -F: '/model name|Hardware/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_MODEL=\"$2; exit}' /proc/cpuinfo 2>/dev/null; echo CPU_CORES=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null); awk -F: '/cache size/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_CACHE=\"$2; exit}' /proc/cpuinfo 2>/dev/null; awk -F: '/bogomips/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_BOGO=\"$2; exit}' /proc/cpuinfo 2>/dev/null; lspci 2>/dev/null | awk -F': ' '/VGA|3D|Display/{print \"GPU=\" $2; exit}'; echo __MSTICK__\n";
    // Skip the resource monitor entirely when shell integration is off (a
    // non-POSIX / Windows server) — the /proc-based loop only spews errors there
    // (#140).
    let mut mon_channel: Option<Channel<Msg>> = None;
    let mut mon_buf = String::new();
    let mut sys_buf = String::new();
    let mut prev_cpu: Option<(u64, u64)> = None; // (total jiffies, idle jiffies)
    let mut prev_net: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new(); // iface -> (rx_bytes, tx_bytes)
    let mut prev_net_at = std::time::Instant::now();

    // Process sampling has its own channel. The broader resource command above
    // includes probes such as `df` which can block indefinitely on a stale NFS
    // mount; that must not leave dead PIDs frozen in the process window.
    const PROC_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; while :; do echo __ME__; id -un 2>/dev/null; echo __PS__; ps -eo pid,user:32,pcpu,pmem,args --sort=-pcpu 2>/dev/null | head -n 41 | cut -c -200; echo __PSTICK__; sleep 2; done\n";
    let mut proc_channel: Option<Channel<Msg>> = None;
    let mut sys_channel: Option<Channel<Msg>> = None;
    let mut proc_buf = String::new();

    // --- Port forwarding / tunnels (#56) --------------------------------
    // Remote (-R) first, while we still hold `handle` mutably (tcpip_forward
    // takes &mut self); the server then opens channels back, serviced in the
    // handler. Then wrap the handle in an Arc so the local/dynamic listener
    // tasks can share it (russh's Handle isn't Clone, but its methods are &self).
    let mut runtime_forwards: std::collections::HashMap<String, RuntimeForward> =
        std::collections::HashMap::new();
    for (idx, f) in session
        .forwards
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind == "remote")
    {
        let bind = if f.bind_addr.trim().is_empty() {
            "127.0.0.1".to_string()
        } else {
            f.bind_addr.trim().to_string()
        };
        let id = format!("config-{idx}");
        match handle.tcpip_forward(bind.clone(), f.bind_port as u32).await {
            Ok(_) => {
                let _ = events.send(SessionEvent::Output(format!(
                    "\r\n[meatshell] -R {bind}:{} → {}:{}\r\n",
                    f.bind_port, f.host, f.host_port
                )));
                runtime_forwards.insert(
                    id.clone(),
                    RuntimeForward {
                        info: tunnel_info(id, f, true, t("运行中", "running")),
                        task: None,
                    },
                );
            }
            Err(e) => {
                let _ = events.send(SessionEvent::Output(format!(
                    "\r\n[meatshell] -R {bind}:{} 请求失败 / request failed: {e}\r\n",
                    f.bind_port
                )));
                runtime_forwards.insert(
                    id.clone(),
                    RuntimeForward {
                        info: tunnel_info(id, f, false, t("启动失败", "failed")),
                        task: None,
                    },
                );
            }
        }
    }
    let handle = Arc::new(handle);

    // Delay auxiliary channels until after the terminal is usable, but open them
    // from this task. Opening them in detached tasks while `channel.wait()` was
    // being polled could make russh/strict servers close the primary PTY exactly
    // when the first monitor started (#264 follow-up).
    let auxiliary_started_at = tokio::time::Instant::now();
    let mut auxiliary_startup = AuxiliaryStartup::new(!session.disable_shell_integration);
    let mut auxiliary_deadline = auxiliary_startup
        .pending()
        .map(|kind| auxiliary_started_at + kind.delay());
    let mut resource_monitoring = true;
    let mut first_terminal_output = true;
    // Local (-L) and dynamic (-D) listen client-side; their tasks are aborted
    // on session exit.
    for (idx, f) in session.forwards.iter().enumerate() {
        match f.kind.as_str() {
            "local" | "dynamic" => {
                let id = format!("config-{idx}");
                runtime_forwards.insert(
                    id.clone(),
                    start_runtime_forward(handle.clone(), id, f.clone(), &events),
                );
            }
            _ => {}
        }
    }
    emit_tunnel_update(&runtime_forwards, &events);

    let mut terminal_decoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let mut extended_decoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let terminal_encoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let mut trigger_engine = TriggerEngine::new(&session.triggers);

    // --- Main pump ------------------------------------------------------
    loop {
        tokio::select! {
            _ = async {
                match auxiliary_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(kind) = auxiliary_startup.begin() {
                    let auxiliary_channel = match kind {
                        AuxiliaryChannelKind::Resources if resource_monitoring => {
                            open_auxiliary_channel(&handle, MON_CMD, "monitor").await
                        }
                        AuxiliaryChannelKind::Processes if resource_monitoring => {
                            open_auxiliary_channel(&handle, PROC_CMD, "process monitor").await
                        }
                        AuxiliaryChannelKind::SystemInfo => {
                            open_auxiliary_channel(&handle, SYS_CMD, "system-info").await
                        }
                        _ => None,
                    };
                    let auxiliary_available = auxiliary_channel.is_some();

                    match kind {
                        AuxiliaryChannelKind::Resources => mon_channel = auxiliary_channel,
                        AuxiliaryChannelKind::Processes => proc_channel = auxiliary_channel,
                        AuxiliaryChannelKind::SystemInfo => sys_channel = auxiliary_channel,
                    }
                    auxiliary_startup.complete();
                    auxiliary_deadline = auxiliary_startup
                        .pending()
                        .map(|next| tokio::time::Instant::now() + next.delay());
                    tracing::debug!(
                        "[SESSION_START] id={} stage={kind:?}-startup-finished available={} elapsed_ms={}",
                        session.id,
                        auxiliary_available,
                        session_started.elapsed().as_millis()
                    );
                } else {
                    auxiliary_deadline = None;
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        // Only log the byte count — never the bytes themselves,
                        // which are raw keystrokes and may contain passwords (#15).
                        tracing::debug!("ssh channel.data len={} bytes", bytes.len());
                        let bytes = terminal_encoder.encode(&bytes);
                        if let Err(err) = channel.data(&bytes[..]).await {
                            let _ = events.send(SessionEvent::Closed(format!("{}: {err}", t("写入失败", "write failed"))));
                            break;
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = channel.window_change(cols, rows, 0, 0).await;
                    }
                    Some(SessionCommand::SetResourceMonitoring(enabled)) => {
                        if enabled == resource_monitoring || session.disable_shell_integration {
                            continue;
                        }
                        resource_monitoring = enabled;
                        if !enabled {
                            if let Some(monitor) = mon_channel.take() {
                                let _ = monitor.close().await;
                            }
                            if let Some(processes) = proc_channel.take() {
                                let _ = processes.close().await;
                            }
                            mon_buf.clear();
                            proc_buf.clear();
                        } else {
                            match handle.channel_open_session().await {
                                Ok(monitor) => {
                                    if monitor.exec(true, MON_CMD).await.is_ok() {
                                        mon_channel = Some(monitor);
                                    }
                                }
                                Err(error) => tracing::warn!("monitor resume failed: {error}"),
                            }
                            match handle.channel_open_session().await {
                                Ok(processes) => {
                                    if processes.exec(true, PROC_CMD).await.is_ok() {
                                        proc_channel = Some(processes);
                                    }
                                }
                                Err(error) => tracing::warn!("process monitor resume failed: {error}"),
                            }
                            prev_cpu = None;
                            prev_net.clear();
                            prev_net_at = std::time::Instant::now();
                        }
                    }
                    Some(SessionCommand::AddTunnel { id, forward }) => {
                        if forward.kind == "local" || forward.kind == "dynamic" {
                            runtime_forwards.insert(
                                id.clone(),
                                start_runtime_forward(handle.clone(), id, forward, &events),
                            );
                            emit_tunnel_update(&runtime_forwards, &events);
                        } else {
                            let _ = events.send(SessionEvent::Output(format!(
                                "\r\n[meatshell] {}\r\n",
                                t("运行时暂不支持新增远程转发 -R", "runtime remote forwarding (-R) is not supported yet")
                            )));
                        }
                    }
                    Some(SessionCommand::StopTunnel(id)) => {
                        if let Some(f) = runtime_forwards.get_mut(&id) {
                            if let Some(task) = f.task.take() {
                                task.abort();
                            }
                            f.info.active = false;
                            f.info.status = t("已停止", "stopped").to_string();
                            emit_tunnel_update(&runtime_forwards, &events);
                        }
                    }
                    Some(SessionCommand::KillProcess { pid, root_password, reply }) => {
                        let exec_handle = handle.clone();
                        tokio::spawn(async move {
                            let result = kill_remote_process(exec_handle, pid, root_password).await;
                            let _ = reply.send(result);
                        });
                    }
                    Some(SessionCommand::Close) | None => {
                        let _ = channel.eof().await;
                        break;
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        // Route the two ZMODEM handshakes in opposite directions:
                        // remote `sz` downloads into Downloads; remote `rz` opens
                        // a local multi-file picker and uploads the selection.
                        let zmodem_cooldown = zmodem_done_at
                            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
                        if let Some(direction) = (!zmodem_cooldown)
                            .then(|| zmodem_direction(&data))
                            .flatten()
                        {
                            let result = match direction {
                                ZmodemDirection::Download => {
                                    crate::terminal::zmodem::receive(&mut channel, &data, &events)
                                        .await
                                }
                                ZmodemDirection::Upload => {
                                    let files = tokio::task::spawn_blocking(|| {
                                        rfd::FileDialog::new()
                                            .set_title(t(
                                                "选择要通过 rz 上传的文件",
                                                "Choose files to upload via rz",
                                            ))
                                            .pick_files()
                                            .unwrap_or_default()
                                    })
                                    .await
                                    .unwrap_or_default();
                                    if files.is_empty() {
                                        let _ = channel.data(&ZMODEM_CANCEL[..]).await;
                                        let _ = events.send(SessionEvent::Output(format!(
                                            "\r\n[meatshell] {}\r\n",
                                            t("已取消 rz 上传", "rz upload cancelled")
                                        )));
                                        zmodem_done_at = Some(std::time::Instant::now());
                                        continue;
                                    }
                                    let _ = events.send(SessionEvent::Output(format!(
                                        "\r\n[meatshell] {} {}...\r\n",
                                        t("开始上传", "Uploading"),
                                        files
                                            .iter()
                                            .filter_map(|path| path.file_name())
                                            .map(|name| name.to_string_lossy())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )));
                                    crate::terminal::zmodem::send(
                                        &mut channel,
                                        &data,
                                        &files,
                                        &events,
                                    )
                                    .await
                                }
                            };
                            zmodem_done_at = Some(std::time::Instant::now());
                            match result {
                                Ok(leftover) => {
                                    // Bytes after the transfer (the shell prompt):
                                    // run them through the normal output path so
                                    // the prompt shows and the cwd updates.
                                    if !leftover.is_empty() {
                                        let text = terminal_decoder.decode(&leftover);
                                        if let Some(cwd) = extract_osc7_path(&text) {
                                            let _ =
                                                events.send(SessionEvent::CwdChanged(cwd));
                                        }
                                        let _ = events.send(SessionEvent::Output(text));
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("zmodem {direction:?} failed: {e:#}");
                                    let _ = channel.data(&ZMODEM_CANCEL[..]).await;
                                    let _ = events.send(SessionEvent::Output(format!(
                                        "\r\n[meatshell] {}: {e:#}\r\n",
                                        match direction {
                                            ZmodemDirection::Download => t(
                                                "ZMODEM 接收失败,已取消",
                                                "ZMODEM receive failed; cancelled",
                                            ),
                                            ZmodemDirection::Upload => t(
                                                "ZMODEM 上传失败,已取消",
                                                "ZMODEM upload failed; cancelled",
                                            ),
                                        }
                                    )));
                                }
                            }
                            continue;
                        }

                        let chunk = terminal_decoder.decode(&data);

                        if first_terminal_output {
                            first_terminal_output = false;
                            tracing::info!(
                                "[SESSION_START] id={} stage=first-terminal-output elapsed_ms={}",
                                session.id,
                                session_started.elapsed().as_millis()
                            );
                        }

                        // Inject PROMPT_COMMAND after the first real shell output,
                        // unless shell integration is disabled for this session
                        // (e.g. a Windows pwsh/cmd server) (#140).
                        if !prompt_injected
                            && !chunk.trim().is_empty()
                            && prompt_setup_supported
                        {
                            prompt_injected = true;
                            suppress_echo = true;
                            // A separate exec probe already confirmed bash or zsh.
                            // Keep buffering until the hook's OSC 7 arrives: slow
                            // Linux/macOS PTYs may echo this command after several
                            // seconds, while unsupported Windows shells never enter
                            // this branch.
                            // Paint the banner/prompt immediately so the first
                            // usable terminal frame no longer waits for shell
                            // integration (later output carrying the injected
                            // setup command is still buffered and stripped).
                            // On hosts without a login banner this frame IS the
                            // shell prompt, and the shell prints an identical one
                            // after the setup command returns — rendering it
                            // twice. Drop the trailing prompt line here so only
                            // the post-setup prompt (sent via the normal path
                            // below) is shown; any banner text above it is
                            // preserved.
                            let mut painted = chunk.clone();
                            if let Some(prompt_line) = painted.rsplit('\n').next() {
                                if prompt_line
                                    .trim_end()
                                    .ends_with(['#', '$', '%', '>'])
                                {
                                    if let Some(pos) = painted.rfind(prompt_line) {
                                        painted.truncate(pos);
                                    }
                                }
                            }
                            let _ = events.send(SessionEvent::Output(painted));
                            let _ = channel.data(prompt_setup.as_bytes()).await;
                            continue;
                        }

                        // While suppressing, wait for the private OSC 699 completion
                        // marker emitted by the executed setup command. Do not infer
                        // completion from echoed text size: zsh/ZLE may redraw the
                        // long input line often enough to exceed 16 KiB before it
                        // executes, which previously released the internal command
                        // onto the terminal (#344). Output before the marker is
                        // private setup echo and is safely discarded; the rolling
                        // buffer remains bounded while preserving split markers.
                        let mut text = if suppress_echo {
                            echo_buf.push_str(&chunk);
                        if let Some(tail) = take_after_prompt_setup_done(&mut echo_buf) {
                            suppress_echo = false;
                            late_prompt_echo_pending = false;
                            if let Some(cwd) = extract_osc7_path(&tail) {
                                tracing::debug!("OSC7 cwd={:?}", cwd);
                                let _ = events.send(SessionEvent::CwdChanged(cwd));
                            }
                            tail
                        } else {
                                bound_prompt_setup_echo(&mut echo_buf);
                                continue; // keep buffering; show nothing yet
                            }
                        } else {
                            // Scan for the OSC 7 CWD notification (cd-follow).
                            if let Some(cwd) = extract_osc7_path(&chunk) {
                                tracing::debug!("OSC7 cwd={:?}", cwd);
                                let _ = events.send(SessionEvent::CwdChanged(cwd));
                            }
                            let mut clean = chunk;
                            strip_pending_prompt_setup_echo(
                                &mut clean,
                                &mut late_prompt_echo_pending,
                            );
                            clean
                        };

                        // Capture commands run in the terminal via our OSC 697
                        // hook, and strip the sequence so it never reaches the
                        // renderer (#113). Skip our own injected setup line in the
                        // rare case HISTCONTROL=ignorespace isn't in effect.
                        while let Some((cmd, range)) = extract_osc_command(&text) {
                            text.replace_range(range, "");
                            let cmd = cmd.trim();
                            if !cmd.is_empty() && !cmd.contains("__ms7") {
                                let _ = events.send(SessionEvent::CommandRan(cmd.to_string()));
                            }
                        }

                        for (response, append_enter) in trigger_engine.feed(&text) {
                            let mut bytes = response.as_str().as_bytes().to_vec();
                            if append_enter {
                                bytes.push(b'\r');
                            }
                            let encoded = terminal_encoder.encode(&bytes);
                            if let Err(error) = channel.data(&encoded[..]).await {
                                tracing::warn!("session trigger response failed: {error}");
                            }
                        }
                        let _ = events.send(SessionEvent::Output(text));
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                        let text = extended_decoder.decode(&data);
                        for (response, append_enter) in trigger_engine.feed(&text) {
                            let mut bytes = response.as_str().as_bytes().to_vec();
                            if append_enter {
                                bytes.push(b'\r');
                            }
                            let encoded = terminal_encoder.encode(&bytes);
                            if let Err(error) = channel.data(&encoded[..]).await {
                                tracing::warn!("session trigger response failed: {error}");
                            }
                        }
                        let _ = events.send(SessionEvent::Output(text));
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        tracing::warn!(host = %session.host, exit_status, "SSH shell exited");
                        let _ = events.send(SessionEvent::Status(
                            format!("{} (code {exit_status})", t("远程进程退出", "remote process exited")),
                        ));
                    }
                    Some(ChannelMsg::ExitSignal { signal_name, core_dumped, error_message, .. }) => {
                        tracing::warn!(host = %session.host, signal = ?signal_name,
                            core_dumped, message = ?error_message, "SSH shell terminated by signal");
                    }
                    Some(ChannelMsg::Close) => {
                        tracing::warn!(host = %session.host,
                            compatibility_mode = session.disable_shell_integration,
                            elapsed_ms = session_started.elapsed().as_millis(),
                            "SSH shell channel closed by peer");
                        break;
                    }
                    None => {
                        tracing::warn!(host = %session.host,
                            compatibility_mode = session.disable_shell_integration,
                            elapsed_ms = session_started.elapsed().as_millis(),
                            "SSH shell channel receiver ended; check transport disconnect log");
                        break;
                    }
                    _ => {}
                }
            }
            // Remote resource monitor channel.  The `async { ... }` lets us poll
            // an Option<Channel>: once the monitor channel closes we replace it
            // with `pending()` so this arm simply never fires again.
            mon = async {
                match mon_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match mon {
                    Some(ChannelMsg::Data { data }) => {
                        mon_buf.push_str(&String::from_utf8_lossy(&data));
                        // Process every complete sample terminated by the marker.
                        while let Some(idx) = mon_buf.find("__MSTICK__") {
                            let block = mon_buf[..idx].to_string();
                            let rest = mon_buf[idx + "__MSTICK__".len()..]
                                .trim_start_matches(['\r', '\n'])
                                .to_string();
                            mon_buf = rest;
                            if let Some(stats) = parse_monitor_block(
                                &block,
                                &mut prev_cpu,
                                &mut prev_net,
                                &mut prev_net_at,
                            ) {
                                let _ = events.send(stats);
                            }
                        }
                        // Bound the leftover (incomplete) tail: a server that
                        // streams data but never emits the __MSTICK__ marker must
                        // not grow this buffer without limit (memory DoS, #27).
                        // A real sample is a few KiB; 1 MiB is a generous ceiling.
                        const MON_BUF_CAP: usize = 1 << 20;
                        if mon_buf.len() > MON_BUF_CAP {
                            mon_buf.clear();
                        }
                    }
                    Some(ChannelMsg::Close) | None => {
                        mon_channel = None;
                    }
                    _ => {}
                }
            }
            sys = async {
                match sys_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match sys {
                    Some(ChannelMsg::Data { data }) => {
                        sys_buf.push_str(&String::from_utf8_lossy(&data));
                        if let Some(idx) = sys_buf.find("__MSTICK__") {
                            let block = sys_buf[..idx].to_string();
                            let mut detail_cpu = None;
                            let mut detail_net = std::collections::HashMap::new();
                            let mut detail_at = std::time::Instant::now();
                            if let Some(details) = parse_monitor_block(
                                &block,
                                &mut detail_cpu,
                                &mut detail_net,
                                &mut detail_at,
                            ) {
                                let _ = events.send(details);
                            }
                            sys_buf.clear();
                            sys_channel = None;
                        }
                    }
                    Some(ChannelMsg::Close) | None => {
                        sys_channel = None;
                    }
                    _ => {}
                }
            }
            proc_msg = async {
                match proc_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match proc_msg {
                    Some(ChannelMsg::Data { data }) => {
                        proc_buf.push_str(&String::from_utf8_lossy(&data));
                        while let Some(idx) = proc_buf.find("__PSTICK__") {
                            let block = proc_buf[..idx].to_string();
                            proc_buf = proc_buf[idx + "__PSTICK__".len()..]
                                .trim_start_matches(['\r', '\n'])
                                .to_string();
                            let (current_user, procs) = parse_process_block(&block);
                            let _ = events.send(SessionEvent::ProcessStats {
                                current_user,
                                procs,
                            });
                        }
                        const PROC_BUF_CAP: usize = 1 << 18;
                        if proc_buf.len() > PROC_BUF_CAP {
                            proc_buf.clear();
                        }
                    }
                    Some(ChannelMsg::Close) | None => proc_channel = None,
                    _ => {}
                }
            }
        }
    }

    // Tear down any port-forward listeners (#56); -R forwards die with the
    // session's disconnect below.
    for f in runtime_forwards.into_values() {
        if let Some(task) = f.task {
            task.abort();
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    // The shell pump loop only exits when the channel closes / EOFs (incl. a
    // peer/bastion-initiated disconnect), so record it for #86 diagnostics.
    tracing::warn!("ssh connection closed ({}@{})", session.user, session.host);
    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
}

fn parse_process_block(block: &str) -> (String, Vec<ProcInfo>) {
    const MAX_PROCESS_ENTRIES: usize = 64;
    enum Section {
        None,
        User,
        Processes,
    }
    let mut section = Section::None;
    let mut current_user = String::new();
    let mut procs = Vec::new();
    for line in block.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match line {
            "__ME__" => section = Section::User,
            "__PS__" => section = Section::Processes,
            _ => match section {
                Section::User if current_user.is_empty() => current_user = line.to_string(),
                Section::Processes if procs.len() < MAX_PROCESS_ENTRIES => {
                    if let Some(process) = parse_ps_line(line) {
                        procs.push(process);
                    }
                }
                _ => {}
            },
        }
    }
    (current_user, procs)
}

/// Parse one monitor sample (a block of `/proc/stat` cpu line + `/proc/meminfo`
/// fields) into a [`SessionEvent::ResourceStats`].
///
/// CPU usage needs two consecutive `/proc/stat` snapshots; `prev` carries the
/// previous (total, idle) jiffies across calls.  The first sample therefore
/// reports 0% (no baseline yet).
fn parse_monitor_block(
    block: &str,
    prev: &mut Option<(u64, u64)>,
    prev_net: &mut std::collections::HashMap<String, (u64, u64)>,
    prev_net_at: &mut std::time::Instant,
) -> Option<SessionEvent> {
    let mut cpu_total = 0u64;
    let mut cpu_idle = 0u64;
    let mut have_cpu = false;
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut mem_buffers = 0u64;
    let mut mem_cached = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    let mut cpu_nums: Vec<u64> = Vec::new();
    // Raw /proc/net/dev counters this sample: iface -> (rx_bytes, tx_bytes).
    let mut net_now: Vec<(String, u64, u64)> = Vec::new();
    // Filesystems from `df -kP`: (mount, available_bytes, total_bytes).
    let mut disks: Vec<(String, u64, u64)> = Vec::new();
    // Dedup duplicate filesystems before they reach the panel (#38): NAS boxes
    // (FNOS …) report the same underlying volume dozens of times — one Docker
    // overlay mount per container layer, all with identical size. Like dropping rows
    // into a Set: skip a (total, available) we've already shown. `df` lists the real
    // mount first, so that's the one kept.
    let mut seen_fs: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    // Processes from `ps` (#23): top-by-CPU rows.
    let mut procs: Vec<ProcInfo> = Vec::new();
    let mut current_user = String::new();
    let mut sys_kv: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // The sample is split into sections by `echo` markers; everything before the
    // first marker is the cpu/mem/net block.
    enum Section {
        Top,
        Df,
        Me,
        Ps,
        Sys,
    }
    let mut section = Section::Top;

    // Cap how many interfaces / filesystems / processes we accept from one sample
    // so a hostile server can't flood the parser and sidebar with fabricated rows
    // (#27). No real machine has anywhere near this many.
    const MAX_MON_ENTRIES: usize = 64;

    for line in block.lines() {
        if line == "__DF__" {
            section = Section::Df;
            continue;
        }
        if line == "__PS__" {
            section = Section::Ps;
            continue;
        }
        if line == "__ME__" {
            section = Section::Me;
            continue;
        }
        if line == "__SYS__" {
            section = Section::Sys;
            continue;
        }
        match section {
            Section::Df => {
                if disks.len() < MAX_MON_ENTRIES {
                    if let Some((mount, avail, total)) = parse_df_line(line) {
                        // Set-style dedup: skip a filesystem whose (total, available)
                        // we've already added — collapses the dozens of identical
                        // Docker overlay mounts a NAS reports down to one row (#38).
                        if seen_fs.insert((total, avail)) {
                            disks.push((mount, avail, total));
                        }
                    }
                }
                continue;
            }
            Section::Ps => {
                if procs.len() < MAX_MON_ENTRIES {
                    if let Some(p) = parse_ps_line(line) {
                        procs.push(p);
                    }
                }
                continue;
            }
            Section::Me => {
                if current_user.is_empty() {
                    current_user = line.trim().chars().take(64).collect();
                }
                continue;
            }
            Section::Sys => {
                if let Some((k, v)) = line.split_once('=') {
                    sys_kv.insert(k.trim().to_string(), v.trim().to_string());
                }
                continue;
            }
            Section::Top => {}
        }
        if let Some(rest) = line.strip_prefix("cpu ") {
            let nums: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            // user nice system idle iowait irq softirq steal ...
            if nums.len() >= 4 {
                // Saturating arithmetic: a server can send arbitrary jiffy
                // values, and a plain sum/add would panic on overflow in debug.
                cpu_total = nums.iter().copied().fold(0u64, u64::saturating_add);
                cpu_idle = nums[3].saturating_add(nums.get(4).copied().unwrap_or(0)); // idle + iowait
                have_cpu = true;
                cpu_nums = nums;
            }
        } else if let Some(v) = line.strip_prefix("MemTotal:") {
            mem_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            mem_avail = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Buffers:") {
            mem_buffers = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Cached:") {
            mem_cached = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapTotal:") {
            swap_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapFree:") {
            swap_free = parse_meminfo_kib(v);
        } else if net_now.len() < MAX_MON_ENTRIES {
            if let Some((iface, counters)) = parse_net_dev_line(line) {
                net_now.push((iface, counters.0, counters.1));
            }
        }
    }

    // Convert raw byte counters into per-second rates using the previous sample.
    let now = std::time::Instant::now();
    let elapsed = now.duration_since(*prev_net_at).as_secs_f64().max(0.001);
    let mut net: Vec<(String, u64, u64)> = Vec::new();
    let net_counters = net_now.clone();
    if !net_now.is_empty() {
        for (iface, rx, tx) in &net_now {
            if let Some((prx, ptx)) = prev_net.get(iface) {
                let rx_bps = (rx.saturating_sub(*prx) as f64 / elapsed) as u64;
                let tx_bps = (tx.saturating_sub(*ptx) as f64 / elapsed) as u64;
                net.push((iface.clone(), rx_bps, tx_bps));
            }
        }
        prev_net.clear();
        for (iface, rx, tx) in net_now {
            prev_net.insert(iface, (rx, tx));
        }
        *prev_net_at = now;
        // Show busiest first so the default-selected NIC is the active one.
        net.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
    }

    let cpu_percent = if have_cpu {
        let result = match *prev {
            Some((ptotal, pidle)) => {
                let dt = cpu_total.saturating_sub(ptotal);
                let di = cpu_idle.saturating_sub(pidle);
                if dt > 0 {
                    (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        *prev = Some((cpu_total, cpu_idle));
        result
    } else {
        0.0
    };

    // Need at least memory numbers to be a useful sample.
    if mem_total == 0 {
        return None;
    }

    let sys = (!sys_kv.is_empty()).then(|| {
        build_system_details(
            &sys_kv,
            &cpu_nums,
            mem_total,
            mem_avail,
            mem_buffers,
            mem_cached,
            swap_total,
            swap_free,
            &net_counters,
            &disks,
        )
    });

    Some(SessionEvent::ResourceStats {
        cpu_percent,
        mem_used_kib: mem_total.saturating_sub(mem_avail),
        mem_total_kib: mem_total,
        swap_used_kib: swap_total.saturating_sub(swap_free),
        swap_total_kib: swap_total,
        net,
        disks,
        current_user,
        procs,
        sys,
    })
}

fn sys_value(sys: &std::collections::HashMap<String, String>, key: &str) -> String {
    sys.get(key)
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "-".to_string())
}

fn kib_size(kib: u64) -> String {
    format_size(kib.saturating_mul(1024))
}

fn percent_text(used: u64, total: u64) -> String {
    if total == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", used as f64 * 100.0 / total as f64)
    }
}

fn cpu_usage_rows(nums: &[u64]) -> Vec<(String, String)> {
    let labels = [
        ("用户", "User"),
        ("Nice", "Nice"),
        ("系统", "System"),
        ("空闲", "Idle"),
        ("IO", "IO"),
        ("硬件中断", "IRQ"),
        ("软件中断", "SoftIRQ"),
        ("实时", "Steal"),
    ];
    let total = nums.iter().copied().fold(0u64, u64::saturating_add);
    labels
        .iter()
        .enumerate()
        .map(|(idx, (zh, en))| {
            let value = nums.get(idx).copied().unwrap_or(0);
            let pct = if total == 0 {
                "0.0%".to_string()
            } else {
                format!("{:.1}%", value as f64 * 100.0 / total as f64)
            };
            (t(zh, en).to_string(), pct)
        })
        .collect()
}

fn build_system_details(
    sys: &std::collections::HashMap<String, String>,
    cpu_nums: &[u64],
    mem_total: u64,
    mem_avail: u64,
    mem_buffers: u64,
    mem_cached: u64,
    swap_total: u64,
    swap_free: u64,
    net_counters: &[(String, u64, u64)],
    disks: &[(String, u64, u64)],
) -> SystemDetails {
    let mem_used = mem_total.saturating_sub(mem_avail);
    let swap_used = swap_total.saturating_sub(swap_free);
    let cpu_model = sys_value(sys, "CPU_MODEL");
    let gpu = sys.get("GPU").cloned().unwrap_or_default();
    let gpu_info = if gpu.trim().is_empty() {
        Vec::new()
    } else {
        vec![
            (t("名称", "Name").to_string(), gpu),
            (t("厂商", "Vendor").to_string(), "-".to_string()),
            (t("驱动", "Driver").to_string(), "-".to_string()),
            (t("内存", "Memory").to_string(), "-".to_string()),
        ]
    };

    SystemDetails {
        overview: vec![
            (
                t("操作系统", "Operating system").to_string(),
                sys_value(sys, "OS"),
            ),
            (
                t("内核版本", "Kernel version").to_string(),
                sys_value(sys, "KERNEL_RELEASE"),
            ),
            (
                t("主机名称", "Hostname").to_string(),
                sys_value(sys, "HOSTNAME"),
            ),
            (t("IP", "IP").to_string(), sys_value(sys, "IPS")),
            (t("负载", "Load").to_string(), sys_value(sys, "LOAD")),
            (t("内核", "Kernel").to_string(), sys_value(sys, "KERNEL")),
            (
                t("硬件架构", "Architecture").to_string(),
                sys_value(sys, "ARCH"),
            ),
            (t("连接", "Connection").to_string(), sys_value(sys, "IPS")),
            (t("运行", "Uptime").to_string(), sys_value(sys, "UPTIME")),
        ],
        cpu_info: vec![
            (t("名称", "Name").to_string(), cpu_model),
            (
                t("核心数", "Cores").to_string(),
                sys_value(sys, "CPU_CORES"),
            ),
            (t("频率", "Frequency").to_string(), "-".to_string()),
            (t("缓存", "Cache").to_string(), sys_value(sys, "CPU_CACHE")),
            ("BogoMips".to_string(), sys_value(sys, "CPU_BOGO")),
        ],
        gpu_info,
        cpu_usage: cpu_usage_rows(cpu_nums),
        memory: vec![
            (t("总计", "Total").to_string(), kib_size(mem_total)),
            (t("已使用", "Used").to_string(), kib_size(mem_used)),
            (t("剩余", "Free").to_string(), kib_size(mem_avail)),
            (
                t("已用", "Usage").to_string(),
                percent_text(mem_used, mem_total),
            ),
            (t("缓冲", "Buffers").to_string(), kib_size(mem_buffers)),
            (t("缓存", "Cached").to_string(), kib_size(mem_cached)),
        ],
        swap: vec![
            (t("总计", "Total").to_string(), kib_size(swap_total)),
            (t("已使用", "Used").to_string(), kib_size(swap_used)),
            (t("剩余", "Free").to_string(), kib_size(swap_free)),
            (
                t("已用", "Usage").to_string(),
                percent_text(swap_used, swap_total),
            ),
        ],
        networks: net_counters
            .iter()
            .map(|(name, rx, tx)| {
                (
                    name.clone(),
                    format_size(*tx),
                    format_size(*rx),
                    "-".to_string(),
                    "-".to_string(),
                )
            })
            .collect(),
        filesystems: disks
            .iter()
            .map(|(mount, avail, total)| {
                let used = total.saturating_sub(*avail);
                (
                    mount.clone(),
                    format_size(*total),
                    percent_text(used, *total),
                    format_size(*avail),
                    mount.clone(),
                )
            })
            .collect(),
    }
}

/// Parse one `ps -eo pid,user,pcpu,pmem,args` line into a [`ProcInfo`]. The
/// header row (`PID` is not numeric) and any malformed line yield `None`.
/// `args` (everything past the four fixed columns) keeps internal spacing
/// collapsed — fine for a display-only command column.
fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let mut it = line.split_whitespace();
    let pid: u32 = it.next()?.parse().ok()?;
    let user = it.next()?.to_string();
    let cpu: f32 = it.next()?.parse().ok()?;
    let mem: f32 = it.next()?.parse().ok()?;
    let command = it.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcInfo {
        pid,
        user,
        cpu,
        mem,
        command,
    })
}

/// Parse one `df -kP` data line into `(mount, available_bytes, total_bytes)`.
/// Columns: `Filesystem 1024-blocks Used Available Capacity Mounted-on`.
fn parse_df_line(line: &str) -> Option<(String, u64, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 6 || f[0] == "Filesystem" {
        return None;
    }
    let total_kb: u64 = f[1].parse().ok()?;
    let avail_kb: u64 = f[3].parse().ok()?;
    if total_kb == 0 {
        return None;
    }
    // Mount point is the last column (joined in case it contains spaces).
    let mount = f[5..].join(" ");
    // Saturating: a server can report arbitrary block counts; KiB→bytes must
    // not overflow-panic in debug (#27).
    Some((
        mount,
        avail_kb.saturating_mul(1024),
        total_kb.saturating_mul(1024),
    ))
}

/// Extract the leading integer (KiB) from a `/proc/meminfo` value like
/// `"  3288560 kB"`.
fn parse_meminfo_kib(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0)
}

/// Parse one `/proc/net/dev` data line into `(iface, (rx_bytes, tx_bytes))`.
/// Format: `  eth0: <rx_bytes> <rx_pkts> ... <tx_bytes> <tx_pkts> ...`
/// (16 numeric columns; rx_bytes is col 0, tx_bytes is col 8).  The `lo`
/// loopback interface is skipped — it never reflects real traffic.
fn parse_net_dev_line(line: &str) -> Option<(String, (u64, u64))> {
    let (name, rest) = line.split_once(':')?;
    let iface = name.trim();
    if iface.is_empty() || iface == "lo" || iface.contains(' ') {
        return None;
    }
    let nums: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    if nums.len() < 9 {
        return None;
    }
    Some((iface.to_string(), (nums[0], nums[8])))
}

/// True if a keyboard-interactive prompt is asking for a second factor (an MFA /
/// OTP / verification code) rather than the account password. We answer password
/// challenges automatically with the stored password but must ask the user for
/// these (#86-MFA). Heuristic over the common English/Chinese wordings used by
/// JumpServer, Google Authenticator (PAM), Duo, etc.
fn looks_like_mfa(prompt: &str) -> bool {
    let t = prompt.to_lowercase();
    t.contains("code")
        || t.contains("otp")
        || t.contains("mfa")
        || t.contains("2fa")
        || t.contains("factor") // two-factor / second factor
        || t.contains("duo")
        || t.contains("verification")
        || t.contains("verify")
        || t.contains("token")
        || t.contains("authenticator")
        || t.contains("passcode")
        || t.contains("one-time")
        || t.contains("one time")
        || t.contains("验证码")
        || t.contains("动态")
        || t.contains("令牌")
}

/// Authenticate via `keyboard-interactive`. The stored password answers the
/// first password challenge automatically (the JumpServer-style bastions that
/// disable the plain `password` method, #86); any *other* challenge — an MFA /
/// verification-code prompt — is shown to the user, whose typed answer is sent
/// back. This is what makes MFA-enabled bastions (JumpServer with MFA forced on)
/// work (#86-MFA).
pub(crate) async fn keyboard_interactive_auth<H>(
    handle: &mut Handle<H>,
    user: &str,
    password: &str,
    session_id: &str,
    host: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<bool>
where
    H: Handler + 'static,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    use russh::client::KeyboardInteractiveAuthResponse as Kb;
    let mut res = handle
        .authenticate_keyboard_interactive_start(user.to_string(), None)
        .await?;
    let mut password_used = false;
    // Bound the exchange so a misbehaving server can't loop us forever.
    for _ in 0..16 {
        match res {
            Kb::Success => return Ok(true),
            Kb::Failure => return Ok(false),
            Kb::InfoRequest { prompts, .. } => {
                let mut responses = Vec::with_capacity(prompts.len());
                for p in &prompts {
                    // Use the stored password for the first password-like
                    // challenge; ask the user for everything else (MFA codes).
                    if !password_used && !password.is_empty() && !looks_like_mfa(&p.prompt) {
                        responses.push(password.to_string());
                        password_used = true;
                    } else {
                        match ask_mfa_prompt(session_id, host, &p.prompt, p.echo, events).await {
                            Some(answer) => responses.push(answer),
                            None => return Ok(false), // user cancelled
                        }
                    }
                }
                res = handle
                    .authenticate_keyboard_interactive_respond(responses)
                    .await?;
            }
        }
    }
    Ok(false)
}

/// Ask the UI for a single keyboard-interactive answer (an MFA / verification
/// code), blocking until the user responds. `None` = cancelled or no UI (#86-MFA).
async fn ask_mfa_prompt(
    session_id: &str,
    host: &str,
    prompt: &str,
    echo: bool,
    events: &UnboundedSender<SessionEvent>,
) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = events.send(SessionEvent::MfaPrompt {
        session_id: session_id.to_string(),
        host: host.to_string(),
        prompt: prompt.to_string(),
        echo,
        responder: MfaResponder::new(tx),
    });
    if sent.is_err() {
        return None; // no UI to ask
    }
    rx.await.ok().flatten()
}

/// Client handler. Verifies the server host key against the known_hosts store,
/// prompting the user on first contact / on a changed key (#109-5).
///
/// Carries the remote-forward (-R) map so we can service channels the server
/// opens back to us: server bind-port → local `(host, port)` target (#56).
pub(crate) struct ClientHandler {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) remote_forwards: std::collections::HashMap<u32, (String, u16)>,
    pub(crate) events: UnboundedSender<SessionEvent>,
}

/// Shared host-key check used by both the shell and SFTP connections: trust a
/// matching stored key silently; otherwise ask the UI (via `events`) and, on
/// acceptance, remember the key. A dropped/closed reply channel (UI gone)
/// counts as a rejection so we never connect to an unverified host.
pub(crate) async fn verify_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    events: &UnboundedSender<SessionEvent>,
) -> bool {
    use crate::ssh::HostKeyStatus;
    match crate::ssh::known_hosts::verify(host, port, key) {
        HostKeyStatus::Match => true,
        status => {
            let changed = status == HostKeyStatus::Changed;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = events.send(SessionEvent::HostKeyPrompt {
                host: host.to_string(),
                port,
                key_type: key.algorithm().to_string(),
                fingerprint: crate::ssh::known_hosts::fingerprint(key),
                changed,
                responder: HostKeyResponder::new(tx),
            });
            if sent.is_err() {
                return false; // no UI to ask
            }
            match rx.await {
                Ok(true) => {
                    if let Err(e) = crate::ssh::known_hosts::remember(host, port, key) {
                        tracing::warn!("could not save host key for {host}:{port}: {e:#}");
                    }
                    true
                }
                _ => false,
            }
        }
    }
}

/// Resolve a session's username/password, prompting the UI for whatever is
/// missing (#110). Returns the effective `(user, password)`, or `None` if the
/// user cancelled. Both the shell and SFTP connections call this; the UI
/// de-duplicates by session id so a single dialog serves both. A dropped reply
/// channel (no UI) falls through with the stored values so auth fails normally.
pub(crate) async fn resolve_credentials(
    session: &Session,
    events: &UnboundedSender<SessionEvent>,
) -> Option<(String, String)> {
    let mut user = session.user.trim().to_string();
    let mut password = session.password.as_str().to_string();
    let need_user = user.is_empty();
    let need_password = matches!(session.auth, AuthMethod::Password) && password.is_empty();
    if !(need_user || need_password) {
        return Some((user, password));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = events.send(SessionEvent::CredentialPrompt {
        session_id: session.id.clone(),
        host: session.host.clone(),
        user: user.clone(),
        need_user,
        need_password,
        responder: CredentialResponder::new(tx),
    });
    if sent.is_err() {
        return Some((user, password));
    }
    match rx.await {
        Ok(Some((u, p, _remember))) => {
            if need_user {
                user = u.trim().to_string();
            }
            if need_password {
                password = p;
            }
            Some((user, password))
        }
        _ => None,
    }
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    // The channel receiver closes for both transport failures and peer shutdown.
    // Preserve the transport reason at WARN level before the shell pump loses it.
    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                tracing::warn!(
                    host = %self.host,
                    port = self.port,
                    reason = ?info.reason_code,
                    message = ?info.message,
                    "SSH server sent disconnect"
                );
                Ok(())
            }
            client::DisconnectReason::Error(error) => {
                tracing::warn!(
                    host = %self.host,
                    port = self.port,
                    error = ?error,
                    "SSH transport ended with error"
                );
                // Match russh's default handler: callers must still receive it.
                Err(error)
            }
        }
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(verify_host_key(&self.host, self.port, server_public_key, &self.events).await)
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Remote forward (-R): the server opened a channel for a connection that
    /// arrived on a port we asked it to listen on. Connect to the configured
    /// local target and splice the two together (#56).
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let target = self.remote_forwards.get(&connected_port).cloned();
        let events = self.events.clone();
        let bind = connected_address.to_string();
        tokio::spawn(async move {
            let Some((host, port)) = target else {
                tracing::warn!("forwarded-tcpip on {bind}:{connected_port} with no mapping");
                return;
            };
            match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(mut tcp) => {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                }
                Err(e) => {
                    let _ = events.send(SessionEvent::Output(format!(
                        "\r\n[meatshell] -R {host}:{port} 连接失败 / connect failed: {e}\r\n"
                    )));
                }
            }
        });
        Ok(())
    }
}

// Marker trait impl so `Arc<Handle<Handler>>` is nameable in external code.
#[allow(dead_code)]
fn _assert_handle_send() {
    fn takes<T: Send>() {}
    takes::<Handle<ClientHandler>>();
}

#[cfg(test)]
mod prompt_setup_echo_tests {
    use super::{
        bound_prompt_setup_echo, prompt_setup_echo_end, prompt_setup_supported,
        strip_late_prompt_setup_echo, strip_pending_prompt_setup_echo, strip_prompt_setup_echo,
        take_after_prompt_setup_done, PROMPT_BODY, PROMPT_SETUP_DONE, PROMPT_SETUP_HISTORY_MARKER,
        PROMPT_SETUP_PREFIX,
    };

    #[test]
    fn only_bash_and_zsh_receive_prompt_setup() {
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:bash\n"),
            Some(true)
        );
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:zsh\n"),
            Some(true)
        );
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:other\n"),
            Some(false)
        );
        assert_eq!(prompt_setup_supported("ash: syntax error\n"), None);
    }

    #[test]
    fn bash_setup_removes_current_and_stale_history_entries() {
        assert!(PROMPT_BODY.contains(PROMPT_SETUP_HISTORY_MARKER));
        assert!(PROMPT_BODY.contains("history 2>/dev/null"));
        assert!(PROMPT_BODY.contains("__ms7()"));
        assert!(PROMPT_BODY.contains("history -d \"$__mn\""));
        // Re-prime command capture only after deleting the setup entry, so the
        // previous real user command does not get reported as newly executed.
        assert!(PROMPT_BODY.find("history -d").unwrap() < PROMPT_BODY.rfind("__cl=").unwrap());
        assert!(PROMPT_BODY.contains("699;ready"));
    }

    #[test]
    fn completion_marker_hides_corrupted_large_zsh_redraws() {
        let mut buffered = "cst test -z redraw\r".repeat(5000);
        buffered.push_str(PROMPT_SETUP_DONE);
        buffered.push_str("\u{1b}]7;file://host/home/user\u{07}prompt");

        let tail = take_after_prompt_setup_done(&mut buffered).expect("completion marker");
        assert!(buffered.is_empty());
        assert_eq!(tail, "\u{1b}]7;file://host/home/user\u{07}prompt");
        assert!(!tail.contains("test -z"));
    }

    #[test]
    fn rolling_setup_buffer_preserves_a_split_completion_marker() {
        let split = 6;
        let mut buffered = "redraw".repeat(20_000);
        buffered.push_str(&PROMPT_SETUP_DONE[..split]);
        bound_prompt_setup_echo(&mut buffered);
        assert!(buffered.len() < 1024);

        buffered.push_str(&PROMPT_SETUP_DONE[split..]);
        buffered.push_str("prompt");
        assert_eq!(
            take_after_prompt_setup_done(&mut buffered).as_deref(),
            Some("prompt")
        );
    }

    #[test]
    fn strips_oh_my_zsh_echo_without_newline() {
        let mut text = format!(
            "➜  ~  {} && eval 'body; __ms7'\rafter prompt",
            PROMPT_SETUP_PREFIX
        );
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        let end = prompt_setup_echo_end(&text, p);
        strip_prompt_setup_echo(&mut text, p, end);
        assert_eq!(text, "\r\x1b[2Kafter prompt");
    }

    #[test]
    fn strips_echo_through_osc7() {
        let mut text = format!(
            "banner\n➜  ~  {} && eval 'body; __ms7'\r\u{1b}]7;file://host/home/jeff\u{07}prompt",
            PROMPT_SETUP_PREFIX
        );
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        let osc_end = text.find("prompt").unwrap();
        strip_prompt_setup_echo(&mut text, p, osc_end);
        assert_eq!(text, "banner\n\r\x1b[2Kprompt");
    }

    #[test]
    fn strips_late_echoed_setup_command() {
        let mut text = format!(
            "prompt\r\n{} && eval 'body; __ms7'\r\nafter",
            PROMPT_SETUP_PREFIX
        );
        assert!(strip_late_prompt_setup_echo(&mut text));
        assert_eq!(text, "prompt\r\n\r\x1b[2Kafter");
    }

    #[test]
    fn late_setup_filter_disables_itself_after_one_match() {
        let echoed = format!(
            "prompt\r\n{} && eval 'body; __ms7'\r\nafter",
            PROMPT_SETUP_PREFIX
        );
        let mut pending = true;
        let mut first = echoed.clone();
        assert!(strip_pending_prompt_setup_echo(&mut first, &mut pending));
        assert!(!pending);

        // A later readline recall can contain the same private setup text. It
        // must reach the terminal untouched instead of clearing visible rows.
        let mut recalled = echoed.clone();
        assert!(!strip_pending_prompt_setup_echo(
            &mut recalled,
            &mut pending
        ));
        assert_eq!(recalled, echoed);
    }

    #[test]
    fn hidden_setup_echo_resynchronizes_the_prompt_cursor() {
        let prompt = "root@host:~# ";
        let mut parser = vt100::Parser::new(4, 80, 0);
        // The initial prompt is painted immediately before shell integration is
        // injected. The buffered setup echo must replace, not append to, it.
        parser.process(prompt.as_bytes());
        let mut echoed = format!(
            "{prompt}{} && eval 'body; __ms7'\r\n\u{1b}]7;file://host/root\u{07}{prompt}",
            PROMPT_SETUP_PREFIX
        );
        let prefix = echoed.find(PROMPT_SETUP_PREFIX).unwrap();
        let osc_end = echoed.rfind(prompt).unwrap();
        strip_prompt_setup_echo(&mut echoed, prefix, osc_end);
        parser.process(echoed.as_bytes());

        assert_eq!(parser.screen().contents().lines().next(), Some(prompt));
        assert_eq!(parser.screen().cursor_position(), (0, prompt.len() as u16));
    }
}

#[cfg(test)]
mod zmodem_detection_tests {
    use super::{zmodem_direction, ZmodemDirection};

    #[test]
    fn distinguishes_remote_sz_from_remote_rz() {
        assert_eq!(
            zmodem_direction(b"**\x18B00000000000000\r\n"),
            Some(ZmodemDirection::Download)
        );
        assert_eq!(
            zmodem_direction(b"rz waiting to receive.**\x18B0100000023be50\r\n\x11"),
            Some(ZmodemDirection::Upload)
        );
    }

    #[test]
    fn ignores_non_handshake_frames_and_ctrl_x() {
        assert_eq!(zmodem_direction(b"plain \x18 text"), None);
        assert_eq!(zmodem_direction(b"**\x18B08000000000000\r\n"), None);
    }
}

#[cfg(test)]
mod auxiliary_startup_tests {
    use super::{auxiliary_timeout, AuxiliaryChannelKind, AuxiliaryStartup};

    #[test]
    fn starts_channels_in_order_and_never_overlaps_them() {
        let mut startup = AuxiliaryStartup::new(true);

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::Resources));
        assert_eq!(startup.begin(), None);
        startup.complete();

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::Processes));
        assert_eq!(startup.begin(), None);
        startup.complete();

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::SystemInfo));
        startup.complete();
        assert_eq!(startup.begin(), None);
    }

    #[test]
    fn disabled_shell_integration_skips_all_auxiliary_channels() {
        let mut startup = AuxiliaryStartup::new(false);

        assert_eq!(startup.begin(), None);
    }

    #[tokio::test]
    async fn auxiliary_channel_startup_timeout_releases_the_pump() {
        let result = auxiliary_timeout(
            std::time::Duration::from_millis(1),
            std::future::pending::<()>(),
        )
        .await;

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod osc_command_tests {
    use super::extract_osc_command;

    #[test]
    fn extracts_and_locates_bel_terminated() {
        let text = "before\u{1b}]697;ls -la\u{07}after";
        let (cmd, range) = extract_osc_command(text).expect("found");
        assert_eq!(cmd, "ls -la");
        // Stripping the range leaves the surrounding text intact.
        let mut s = text.to_string();
        s.replace_range(range, "");
        assert_eq!(s, "beforeafter");
    }

    #[test]
    fn extracts_st_terminated() {
        let text = "\u{1b}]697;echo hi\u{1b}\\";
        let (cmd, _) = extract_osc_command(text).expect("found");
        assert_eq!(cmd, "echo hi");
    }

    #[test]
    fn ignores_other_osc_and_incomplete() {
        // OSC 7 (cwd) is not a command sequence.
        assert!(extract_osc_command("\u{1b}]7;file:///home\u{07}").is_none());
        // No terminator yet → wait for more.
        assert!(extract_osc_command("\u{1b}]697;ls").is_none());
        assert!(extract_osc_command("plain text").is_none());
    }
}

#[cfg(test)]
mod monitor_hardening_tests {
    use super::{parse_df_line, parse_monitor_block, parse_process_block};
    use std::collections::HashMap;
    use std::time::Instant;

    #[test]
    fn df_line_saturates_instead_of_overflowing() {
        // avail/total near u64::MAX must not panic on the KiB->bytes multiply.
        let line = "/dev/sda1 18446744073709551615 0 18446744073709551615 100% /";
        let (_, avail, total) = parse_df_line(line).expect("parses");
        assert_eq!(avail, u64::MAX);
        assert_eq!(total, u64::MAX);
    }

    #[test]
    fn cpu_overflow_values_do_not_panic() {
        let big = u64::MAX;
        let block =
            format!("cpu {big} {big} {big} {big} {big}\nMemTotal: 1000 kB\nMemAvailable: 500 kB");
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        // Must not panic; with no baseline the first sample reports 0% CPU.
        assert!(parse_monitor_block(&block, &mut prev, &mut prev_net, &mut at).is_some());
    }

    #[test]
    fn floods_of_fake_interfaces_are_capped() {
        let mut block = String::from("MemTotal: 1000 kB\nMemAvailable: 500 kB\n");
        for i in 0..500 {
            block.push_str(&format!("eth{i}: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n"));
        }
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        assert!(parse_monitor_block(&block, &mut prev, &mut prev_net, &mut at).is_some());
        // The remembered interface set is capped, not 500.
        assert!(prev_net.len() <= 64, "prev_net held {}", prev_net.len());
    }

    #[test]
    fn monitor_reports_effective_user_for_ownership_checks() {
        let block = "MemTotal: 1000 kB\nMemAvailable: 500 kB\n__DF__\n__ME__\nalice\n__PS__\n10 alice 1.0 2.0 sleep 30";
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        let event = parse_monitor_block(block, &mut prev, &mut prev_net, &mut at).unwrap();
        match event {
            super::SessionEvent::ResourceStats {
                current_user,
                procs,
                ..
            } => {
                assert_eq!(current_user, "alice");
                assert_eq!(procs[0].user, "alice");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn lightweight_resource_sample_does_not_replace_system_details() {
        let block = "cpu 1 2 3 4\nMemTotal: 1000 kB\nMemAvailable: 500 kB\n__DF__\n";
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        let event = parse_monitor_block(block, &mut prev, &mut prev_net, &mut at).unwrap();
        match event {
            super::SessionEvent::ResourceStats { sys, .. } => assert!(sys.is_none()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn delayed_system_sample_carries_detailed_information() {
        let block = "cpu 1 2 3 4\nMemTotal: 1000 kB\nMemAvailable: 500 kB\n__DF__\n__SYS__\nOS=Debian GNU/Linux 12\nKERNEL=Linux\n";
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        let event = parse_monitor_block(block, &mut prev, &mut prev_net, &mut at).unwrap();
        match event {
            super::SessionEvent::ResourceStats { sys, .. } => {
                let sys = sys.expect("delayed sample should include details");
                assert!(sys
                    .overview
                    .iter()
                    .any(|(_, value)| value == "Debian GNU/Linux 12"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn dedicated_process_block_reports_user_and_rows() {
        let (user, procs) = parse_process_block(
            "__ME__\nalice\n__PS__\nPID USER %CPU %MEM COMMAND\n42 root 3.5 1.2 java -jar demo.jar\n",
        );
        assert_eq!(user, "alice");
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 42);
        assert_eq!(procs[0].user, "root");
        assert_eq!(procs[0].command, "java -jar demo.jar");
    }
}

#[cfg(test)]
mod process_control_tests {
    use super::{looks_like_sudo_password_prompt, process_control_log_text, process_kill_command};

    #[test]
    fn own_process_uses_plain_term_signal() {
        assert_eq!(process_kill_command(4242, false), "kill -TERM 4242");
    }

    #[test]
    fn privileged_process_uses_root_su_without_embedding_password() {
        assert_eq!(
            process_kill_command(4242, true),
            "LC_ALL=C sudo -S -p 'Password:' -- kill -TERM 4242"
        );
    }

    #[test]
    fn recognizes_su_password_prompt() {
        assert!(looks_like_sudo_password_prompt("Password: "));
        assert!(looks_like_sudo_password_prompt("请输入密码："));
        assert!(!looks_like_sudo_password_prompt("Authentication failure"));
    }

    #[test]
    fn diagnostic_output_redacts_password_and_controls() {
        let safe =
            process_control_log_text("Password:\r\nsecret-value\x1b[0m", Some("secret-value"));
        assert!(!safe.contains("secret-value"));
        assert!(safe.contains("[REDACTED]"));
        assert!(!safe.contains('\r'));
        assert!(!safe.contains('\n'));
    }
}

#[cfg(test)]
mod mfa_tests {
    use super::looks_like_mfa;

    #[test]
    fn password_prompts_are_not_mfa() {
        // These should be answered automatically with the stored password.
        for p in [
            "Password: ",
            "password:",
            "jeff@host's password:",
            "请输入密码:",
            "Password for jeff:",
        ] {
            assert!(!looks_like_mfa(p), "wrongly flagged as MFA: {p:?}");
        }
    }

    #[test]
    fn verification_code_prompts_are_mfa() {
        // These must prompt the user (JumpServer / Google Authenticator / Duo …).
        for p in [
            "MFA code: ",
            "[MFA] Please enter 6 digit code: ",
            "Verification code: ",
            "Verification code (from your authenticator app): ",
            "One-time password (OATH-TOTP): ",
            "Enter passcode or select one of the following options:",
            "Duo two-factor login",
            "请输入验证码:",
            "动态口令:",
            "请输入令牌:",
        ] {
            assert!(looks_like_mfa(p), "missed an MFA prompt: {p:?}");
        }
    }
}
