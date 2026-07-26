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

use crate::config::{AuthMethod, PortForward, Session};
use crate::i18n::t;

// ---------------------------------------------------------------------------
// SFTP-related shared types
// ---------------------------------------------------------------------------

/// Metadata for a single remote filesystem entry returned by SFTP listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    /// Raw size in bytes (0 for directories or unknown).
    pub size: u64,
    /// Modification time as Unix timestamp (seconds, u32 = SFTP wire format).
    pub modified: u32,
    /// POSIX permission bits (the low 12, i.e. rwx + setuid/setgid/sticky).
    /// 0 when the server didn't report permissions. Used to prefill the chmod
    /// dialog (#84).
    pub mode: u32,
}

/// One node in the remote directory tree panel.
#[derive(Debug, Clone)]
pub struct RemoteTreeNode {
    pub path: String,
    pub name: String,
    pub depth: u32,
    pub expanded: bool,
    pub has_children: bool,
}

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
    if bytes < 1_024 {
        format!("{} B", bytes)
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else if bytes < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", bytes as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1_024.0 * 1_024.0 * 1_024.0))
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
const PROMPT_BODY: &str = "test -z \"$FISH_VERSION\" && eval '__msc(){ __c=\"$(fc -ln -1 2>/dev/null)\"; [ -n \"$__c\" ] && [ \"$__c\" != \"$__cl\" ] && { __cl=\"$__c\"; printf \"\\033]697;%s\\007\" \"$__c\"; }; }; __ms7(){ printf \"\\033]7;file://%s%s\\007\" \"$HOSTNAME\" \"$PWD\"; __msc; }; if [ -n \"$ZSH_VERSION\" ]; then autoload -Uz add-zsh-hook 2>/dev/null; add-zsh-hook precmd __ms7; else PROMPT_COMMAND=\"__ms7${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; fi; : __MEATSHELL_INTERNAL_SETUP_1; if [ -n \"$BASH_VERSION\" ]; then __md=\"$(history 2>/dev/null | { __md=\"\"; while read -r __mn __mr; do case \"$__mr\" in *\"__ms7()\"*\"PROMPT_COMMAND=\"*) __mn=\"${__mn%\\*}\"; __md=\"$__mn $__md\";; esac; done; printf \"%s\" \"$__md\"; })\"; for __mn in $__md; do history -d \"$__mn\" 2>/dev/null; done; unset __md __mn __mr; fi; __cl=\"$(fc -ln -1 2>/dev/null)\"; __ms7'";

/// Detect the start of a ZMODEM transfer (sz/rz) in a raw channel chunk.
///
/// Every ZMODEM frame begins with ZDLE (0x18) followed by a type byte; the
/// `sz` handshake leads with a ZRQINIT hex header (`**\x18B00...`). Matching
/// ZDLE followed by `B` (hex frame) or `C` (binary frame) reliably catches the
/// handshake without false-positiving on a lone 0x18 (Ctrl-X) in normal output.
fn contains_zmodem_init(data: &[u8]) -> bool {
    data.windows(2)
        .any(|w| w[0] == 0x18 && (w[1] == b'B' || w[1] == b'C'))
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

/// Commands posted to the worker task by the UI.
#[derive(Debug)]
pub enum SessionCommand {
    /// Send raw bytes directly to the PTY (individual keystrokes, no modification).
    RawInput(Vec<u8>),
    /// Notify the remote PTY of a terminal resize.
    Resize(u32, u32),
    /// Start a runtime-only SSH tunnel for this connected session (#206).
    AddTunnel {
        id: String,
        forward: crate::config::PortForward,
    },
    /// Stop a runtime tunnel created for this connected session (#206).
    StopTunnel(String),
    /// Terminate one remote process on a short-lived exec channel. Supplying a
    /// password selects the privileged `sudo -S` path; the secret is never
    /// written to the interactive PTY or shell history.
    KillProcess {
        pid: u32,
        root_password: Option<crate::config::Secret>,
        reply: tokio::sync::oneshot::Sender<ProcessKillResult>,
    },
    /// Gracefully disconnect and drop the session.
    Close,
}

#[derive(Debug)]
pub struct ProcessKillResult {
    pub success: bool,
    pub message: String,
}

/// Carries the user's answer to a host-key confirmation prompt back to the
/// blocked `check_server_key` handler. Wrapped in `Arc<Mutex<Option<…>>>` so the
/// enclosing [`SessionEvent`] stays `Clone` (a bare `oneshot::Sender` is not);
/// the first `respond` consumes the sender, later calls are no-ops.
#[derive(Clone)]
pub struct HostKeyResponder(Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>);

impl HostKeyResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<bool>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's decision (`true` = trust). Idempotent.
    pub fn respond(&self, accept: bool) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(accept);
            }
        }
    }
}

impl std::fmt::Debug for HostKeyResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostKeyResponder")
    }
}

/// The user's answer to a connect-time credential prompt: `(username, password,
/// remember)`, or `None` if they cancelled.
pub type CredentialReply = (String, String, bool);

/// Carries the credential prompt's answer back to the blocked auth flow (#110).
/// `Arc<Mutex<Option<…>>>` so the enclosing [`SessionEvent`] stays `Clone`.
#[derive(Clone)]
pub struct CredentialResponder(
    Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Option<CredentialReply>>>>>,
);

impl CredentialResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<Option<CredentialReply>>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's answer (`None` = cancelled). Idempotent.
    pub fn respond(&self, reply: Option<CredentialReply>) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(reply);
            }
        }
    }
}

impl std::fmt::Debug for CredentialResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialResponder")
    }
}

/// Carries the answer to a keyboard-interactive (MFA / verification-code) prompt
/// back to the blocked auth flow (#86-MFA). `None` = the user cancelled.
/// `Arc<Mutex<Option<…>>>` so the enclosing [`SessionEvent`] stays `Clone`.
#[derive(Clone)]
pub struct MfaResponder(
    Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>>,
);

impl MfaResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<Option<String>>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's answer (`None` = cancelled). Idempotent.
    pub fn respond(&self, reply: Option<String>) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(reply);
            }
        }
    }
}

impl std::fmt::Debug for MfaResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MfaResponder")
    }
}

/// One process row sampled from the remote `ps` (#23). CPU/mem are percentages
/// as reported by `ps` (pcpu/pmem); `command` is the (width-truncated) args.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub command: String,
}

#[derive(Debug, Clone, Default)]
pub struct SystemDetails {
    pub overview: Vec<(String, String)>,
    pub cpu_info: Vec<(String, String)>,
    pub gpu_info: Vec<(String, String)>,
    pub cpu_usage: Vec<(String, String)>,
    pub memory: Vec<(String, String)>,
    pub swap: Vec<(String, String)>,
    pub networks: Vec<(String, String, String, String, String)>,
    pub filesystems: Vec<(String, String, String, String, String)>,
}

/// One SSH tunnel row shown in the runtime tunnel panel (#206).
#[derive(Debug, Clone)]
pub struct RuntimeTunnelInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub bind_addr: String,
    pub bind_port: u16,
    pub host: String,
    pub host_port: u16,
    pub active: bool,
    pub status: String,
}

/// Events emitted back to the UI thread.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Free-form status text for the tab header / status line.
    Status(String),
    /// A chunk of stdout/stderr output from the remote shell.
    Output(String),
    /// Connection is up.
    Connected,
    /// Connection closed (either cleanly or after an error).
    Closed(String),
    /// The server presented a host key that is unknown or has changed; the UI
    /// must show a confirmation dialog and answer via `responder` (#109-5). The
    /// handler is blocked awaiting that answer.
    HostKeyPrompt {
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        /// True when a *different* key was previously stored (possible MITM).
        changed: bool,
        responder: HostKeyResponder,
    },
    /// The session is missing a username and/or password; the UI must prompt for
    /// them and answer via `responder`. The auth flow is blocked meanwhile (#110).
    CredentialPrompt {
        session_id: String,
        host: String,
        user: String,
        need_user: bool,
        need_password: bool,
        responder: CredentialResponder,
    },
    /// A keyboard-interactive challenge that isn't the account password —
    /// typically an MFA / OTP / verification-code prompt from a bastion such as
    /// JumpServer. The UI shows `prompt` and answers via `responder`; the auth
    /// flow is blocked meanwhile (#86-MFA).
    MfaPrompt {
        session_id: String,
        host: String,
        /// The server's prompt text, e.g. "MFA code: " / "Verification code:".
        prompt: String,
        /// Whether typed input should be visible (false = hide, like a password).
        echo: bool,
        responder: MfaResponder,
    },
    /// Remote machine resource sample (from the monitor channel).
    /// Memory/swap are in KiB (as reported by /proc/meminfo).
    ResourceStats {
        cpu_percent: f32,
        mem_used_kib: u64,
        mem_total_kib: u64,
        swap_used_kib: u64,
        swap_total_kib: u64,
        /// Per-interface (name, rx_bytes_per_sec, tx_bytes_per_sec).
        net: Vec<(String, u64, u64)>,
        /// Per-filesystem (mount_point, available_bytes, total_bytes).
        disks: Vec<(String, u64, u64)>,
        /// Effective login name reported by the remote host (`id -un`).
        current_user: String,
        /// Top processes by CPU (#23). Empty if the host's `ps` is unusable.
        procs: Vec<ProcInfo>,
        /// Detailed system information for the detached system-info window.
        /// Detailed data is present only for the separately delayed one-shot
        /// system-information probe; lightweight resource samples leave it None.
        sys: Option<SystemDetails>,
    },

    /// Effective user and top-process snapshot from the dedicated lightweight
    /// process channel. Keeping this separate prevents a slow `df`, `lspci`, or
    /// other system-information probe from freezing the process window.
    ProcessStats {
        current_user: String,
        procs: Vec<ProcInfo>,
    },

    /// A command the user ran in the terminal, captured via the shell hook
    /// (OSC 697) so it can join the command-box history (#113).
    CommandRan(String),

    /// Runtime tunnel state changed (#206).
    TunnelUpdate(Vec<RuntimeTunnelInfo>),

    // --- SFTP events -------------------------------------------------------
    /// The shell's current working directory changed (parsed from OSC 7).
    CwdChanged(String),
    /// SFTP directory listing arrived.
    SftpEntries {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    /// Free-form SFTP status message (progress, errors, etc.).
    SftpStatus(String),
    /// A directory listing failed (e.g. permission denied): show the message and
    /// stop the panel's loading spinner without disturbing the current view (#112).
    SftpError(String),
    /// Directory tree structure changed (full rebuild pushed on every toggle).
    SftpTreeUpdate(Vec<RemoteTreeNode>),
    /// File-transfer progress / completion (download or upload).
    SftpTransfer {
        id: String,
        name: String,
        is_upload: bool,
        transferred: u64,
        total: u64,
        state: u8, // 0 = active, 1 = done, 2 = error
        msg: String,
    },
    /// A remote text file loaded for the built-in viewer/editor (#70). On
    /// failure (too large, binary, non-UTF-8, I/O error) `error` is non-empty
    /// and `content` is empty.
    SftpFileText {
        path: String,
        name: String,
        content: String,
        edit: bool,
        error: String,
    },
}

/// Handle retained by the UI layer to talk to a running session.
pub struct SessionHandle {
    #[allow(dead_code)] // used by future resize / reconnect flows
    pub tab_id: String,
    pub commands: UnboundedSender<SessionCommand>,
    #[allow(dead_code)] // keep alive; detach on Drop is fine for v0.1
    pub join: JoinHandle<()>,
}

impl SessionHandle {
    pub fn send_raw(&self, bytes: Vec<u8>) {
        let _ = self.commands.send(SessionCommand::RawInput(bytes));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.commands.send(SessionCommand::Resize(cols, rows));
    }

    pub fn add_tunnel(&self, id: String, forward: PortForward) {
        let _ = self
            .commands
            .send(SessionCommand::AddTunnel { id, forward });
    }

    pub fn stop_tunnel(&self, id: String) {
        let _ = self.commands.send(SessionCommand::StopTunnel(id));
    }

    pub fn kill_process(
        &self,
        pid: u32,
        root_password: Option<crate::config::Secret>,
    ) -> tokio::sync::oneshot::Receiver<ProcessKillResult> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SessionCommand::KillProcess {
            pid,
            root_password,
            reply,
        });
        rx
    }

    pub fn close(&self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
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
    // Hard deadline for the suppression window. A non-POSIX shell (Windows
    // pwsh/cmd) never runs our hook and so never echoes the OSC 7 we wait for —
    // without this, output stayed hidden until a 16 KiB cap, leaving the terminal
    // blank/"unusable" on Windows servers (#140-1). When the deadline passes we
    // stop suppressing and show whatever arrived.
    let mut suppress_deadline: Option<tokio::time::Instant> = None;
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
    // The echoed setup line is discarded by anchoring on the OSC 7 it produces
    // (see the suppress block below), so it doesn't matter that the long line
    // wraps — we never substring-match it.
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

    // Auxiliary channels are deliberately outside the terminal-ready critical
    // path. SFTP gets the first opportunity after Connected; lightweight
    // resources follow, and process/system enrichment starts last.
    let (mon_ready_tx, mut mon_ready_rx) = tokio::sync::oneshot::channel();
    let (proc_ready_tx, mut proc_ready_rx) = tokio::sync::oneshot::channel();
    let (sys_ready_tx, mut sys_ready_rx) = tokio::sync::oneshot::channel();
    if session.disable_shell_integration {
        let _ = mon_ready_tx.send(None);
        let _ = proc_ready_tx.send(None);
        let _ = sys_ready_tx.send(None);
    } else {
        let mon_handle = handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            let channel = match mon_handle.channel_open_session().await {
                Ok(ch) => match ch.exec(true, MON_CMD).await {
                    Ok(()) => Some(ch),
                    Err(error) => {
                        tracing::warn!("monitor exec failed: {error}");
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!("monitor channel open failed: {error}");
                    None
                }
            };
            let _ = mon_ready_tx.send(channel);
        });
        let proc_handle = handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let channel = match proc_handle.channel_open_session().await {
                Ok(ch) => match ch.exec(true, PROC_CMD).await {
                    Ok(()) => Some(ch),
                    Err(error) => {
                        tracing::warn!("process monitor exec failed: {error}");
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!("process monitor channel open failed: {error}");
                    None
                }
            };
            let _ = proc_ready_tx.send(channel);
        });
        let sys_handle = handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            let channel = match sys_handle.channel_open_session().await {
                Ok(ch) => match ch.exec(true, SYS_CMD).await {
                    Ok(()) => Some(ch),
                    Err(error) => {
                        tracing::warn!("system-info exec failed: {error}");
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!("system-info channel open failed: {error}");
                    None
                }
            };
            let _ = sys_ready_tx.send(channel);
        });
    }
    let mut mon_start_pending = true;
    let mut proc_start_pending = true;
    let mut sys_start_pending = true;
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

    // --- Main pump ------------------------------------------------------
    loop {
        tokio::select! {
            ready = &mut mon_ready_rx, if mon_start_pending => {
                mon_start_pending = false;
                mon_channel = ready.unwrap_or(None);
                tracing::debug!(
                    "[SESSION_START] id={} stage=resources-started elapsed_ms={}",
                    session.id,
                    session_started.elapsed().as_millis()
                );
            }
            ready = &mut proc_ready_rx, if proc_start_pending => {
                proc_start_pending = false;
                proc_channel = ready.unwrap_or(None);
                tracing::debug!(
                    "[SESSION_START] id={} stage=process-monitor-started elapsed_ms={}",
                    session.id,
                    session_started.elapsed().as_millis()
                );
            }
            ready = &mut sys_ready_rx, if sys_start_pending => {
                sys_start_pending = false;
                sys_channel = ready.unwrap_or(None);
                tracing::debug!(
                    "[SESSION_START] id={} stage=system-info-started elapsed_ms={}",
                    session.id,
                    session_started.elapsed().as_millis()
                );
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        // Only log the byte count — never the bytes themselves,
                        // which are raw keystrokes and may contain passwords (#15).
                        tracing::debug!("ssh channel.data len={} bytes", bytes.len());
                        if let Err(err) = channel.data(&bytes[..]).await {
                            let _ = events.send(SessionEvent::Closed(format!("{}: {err}", t("写入失败", "write failed"))));
                            break;
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = channel.window_change(cols, rows, 0, 0).await;
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
            // Suppression safety net: if the injected hook hasn't echoed its OSC 7
            // by the deadline, the remote shell isn't the POSIX one we injected for
            // (e.g. Windows pwsh/cmd). Stop hiding output so the terminal is usable
            // again; best-effort drop just the echoed setup line (#140-1).
            _ = async {
                match suppress_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            }, if suppress_echo => {
                suppress_echo = false;
                suppress_deadline = None;
                let mut buf = std::mem::take(&mut echo_buf);
                if let Some(p) = buf.find(PROMPT_SETUP_PREFIX) {
                    let end = prompt_setup_echo_end(&buf, p);
                    strip_prompt_setup_echo(&mut buf, p, end);
                    late_prompt_echo_pending = false;
                } else {
                    // Nothing identifiable arrived before the deadline. Allow
                    // one later setup echo to be removed, then permanently
                    // disable the special-case stripping for this session.
                    late_prompt_echo_pending = true;
                }
                if !buf.is_empty() {
                    let _ = events.send(SessionEvent::Output(buf));
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        // A `sz` in the terminal starts a ZMODEM send. Receive it
                        // straight to the Downloads dir (FinalShell style, #76).
                        // On any protocol error, cancel so the session recovers.
                        let zmodem_cooldown = zmodem_done_at
                            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
                        if !zmodem_cooldown && contains_zmodem_init(&data) {
                            let result =
                                crate::terminal::zmodem::receive(&mut channel, &data, &events).await;
                            zmodem_done_at = Some(std::time::Instant::now());
                            match result {
                                Ok(leftover) => {
                                    // Bytes after the transfer (the shell prompt):
                                    // run them through the normal output path so
                                    // the prompt shows and the cwd updates.
                                    if !leftover.is_empty() {
                                        let text =
                                            String::from_utf8_lossy(&leftover).into_owned();
                                        if let Some(cwd) = extract_osc7_path(&text) {
                                            let _ =
                                                events.send(SessionEvent::CwdChanged(cwd));
                                        }
                                        let _ = events.send(SessionEvent::Output(text));
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("zmodem receive failed: {e:#}");
                                    let _ = channel.data(&ZMODEM_CANCEL[..]).await;
                                    let _ = events.send(SessionEvent::Output(format!(
                                        "\r\n[meatshell] {}: {e}\r\n",
                                        t("ZMODEM 接收失败,已取消", "ZMODEM receive failed; cancelled")
                                    ).into()));
                                }
                            }
                            continue;
                        }

                        let chunk = String::from_utf8_lossy(&data).into_owned();

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
                            && !session.disable_shell_integration
                        {
                            prompt_injected = true;
                            suppress_echo = true;
                            // Give the hook ~2 s to echo its OSC 7; past that we
                            // assume a non-POSIX shell and stop hiding output (#140-1).
                            // 1.2 s was too tight for slow PTY/SSH servers — the echo
                            // + OSC 7 landed after the deadline, so the injected setup
                            // line leaked through (#176). The cost of the larger window
                            // is only a slightly longer blank on a non-POSIX shell that
                            // wasn't already flagged disable_shell_integration.
                            suppress_deadline = Some(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_millis(2000),
                            );
                            // Paint the banner/prompt immediately. Only later
                            // output containing our injected setup command is
                            // buffered and stripped; the first usable terminal
                            // frame no longer waits for shell integration.
                            let _ = events.send(SessionEvent::Output(chunk));
                            let _ = channel.data(prompt_setup.as_bytes()).await;
                            continue;
                        }

                        // While suppressing, buffer output until our echoed setup
                        // command AND the OSC 7 that the injected __ms7 prints right
                        // after it have both arrived. Then delete just that span —
                        // from the start of the command's line through the OSC 7 —
                        // which removes the echoed command (even if it WRAPPED across
                        // the terminal width, since we cut by byte range) and the
                        // now-redundant first prompt, while PRESERVING any MOTD/banner
                        // printed before it (#98). The command line is located by a
                        // short, un-wrappable prefix of the injected command. A size
                        // cap is the safety valve for a shell that never reports back
                        // (e.g. dash without PROMPT_COMMAND).
                        let mut text = if suppress_echo {
                            echo_buf.push_str(&chunk);
                            const ECHO_BUF_CAP: usize = 1 << 14; // 16 KiB
                            // The command echo + its trailing OSC 7 (the one after
                            // our command, not any earlier prompt OSC 7).
                            let landed = echo_buf.find(PROMPT_SETUP_PREFIX).and_then(|p| {
                                extract_osc7_end(&echo_buf[p..])
                                    .map(|(cwd, rel)| (p, p + rel, cwd))
                            });
                            if let Some((cmd_pos, osc_end, cwd)) = landed {
                                suppress_echo = false;
                                suppress_deadline = None;
                                late_prompt_echo_pending = false;
                                tracing::debug!("OSC7 cwd={:?}", cwd);
                                let _ = events.send(SessionEvent::CwdChanged(cwd));
                                let mut buf = std::mem::take(&mut echo_buf);
                                strip_prompt_setup_echo(&mut buf, cmd_pos, osc_end);
                                buf
                            } else if echo_buf.len() >= ECHO_BUF_CAP {
                                suppress_echo = false;
                                suppress_deadline = None;
                                let mut buf = std::mem::take(&mut echo_buf);
                                if let Some(p) = buf.find(PROMPT_SETUP_PREFIX) {
                                    let end = prompt_setup_echo_end(&buf, p);
                                    strip_prompt_setup_echo(&mut buf, p, end);
                                    late_prompt_echo_pending = false;
                                } else {
                                    late_prompt_echo_pending = true;
                                }
                                buf
                            } else {
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

                        let _ = events.send(SessionEvent::Output(text));
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        let _ = events.send(SessionEvent::Output(text));
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        let _ = events.send(SessionEvent::Status(
                            format!("{} (code {exit_status})", t("远程进程退出", "remote process exited")),
                        ));
                    }
                    Some(ChannelMsg::Close) | None => {
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
    use crate::ssh::known_hosts::HostKeyStatus;
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
        prompt_setup_echo_end, strip_late_prompt_setup_echo, strip_pending_prompt_setup_echo,
        strip_prompt_setup_echo, PROMPT_BODY, PROMPT_SETUP_HISTORY_MARKER, PROMPT_SETUP_PREFIX,
    };

    #[test]
    fn bash_setup_removes_current_and_stale_history_entries() {
        assert!(PROMPT_BODY.contains(PROMPT_SETUP_HISTORY_MARKER));
        assert!(PROMPT_BODY.contains("history 2>/dev/null"));
        assert!(PROMPT_BODY.contains("__ms7()"));
        assert!(PROMPT_BODY.contains("history -d \"$__mn\""));
        // Re-prime command capture only after deleting the setup entry, so the
        // previous real user command does not get reported as newly executed.
        assert!(PROMPT_BODY.find("history -d").unwrap() < PROMPT_BODY.rfind("__cl=").unwrap());
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
