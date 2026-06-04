//! SFTP subsystem worker.
//!
//! Each terminal tab that spawns an SSH shell also spawns a *separate* SSH
//! connection for SFTP. This keeps the shell PTY completely unblocked: large
//! file transfers cannot stall readline or vim.
//!
//! The public API is a simple command channel (`SftpHandle::commands`) that
//! accepts `SftpCommand` messages. Results and status updates are pushed back
//! via the shared `UnboundedSender<SessionEvent>` that already exists for the
//! terminal tab.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::Disconnect;
use russh_sftp::client::SftpSession;
use ssh_key::PublicKey;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{AuthMethod, Session};
use crate::ssh::{format_mtime, format_size, RemoteEntry, RemoteTreeNode, SessionEvent};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Commands sent to the SFTP worker task from the UI thread.
#[derive(Debug)]
pub enum SftpCommand {
    /// List the contents of a remote directory.
    ListDir(String),
    /// Toggle a directory node in the tree (expand if collapsed, collapse if expanded).
    ToggleTreeNode(String),
    /// Download a remote file to a local directory.
    Download { remote: String, local_dir: String },
    /// Upload a local file into a remote directory.
    Upload { local: String, remote_dir: String },
    /// Delete a remote file (falls back to removing an empty directory).
    Delete(String),
    /// Download a file to a temp dir and open it with the OS default app.
    /// When `edit` is set, watch the temp copy and re-upload on every change.
    OpenTemp { remote: String, edit: bool },
    /// Gracefully shut down the SFTP worker.
    Close,
}

/// Handle retained by the UI to drive a running SFTP worker.
pub struct SftpHandle {
    pub commands: UnboundedSender<SftpCommand>,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

impl SftpHandle {
    pub fn list_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ListDir(path));
    }
    pub fn download(&self, remote: String, local_dir: String) {
        let _ = self.commands.send(SftpCommand::Download { remote, local_dir });
    }
    pub fn upload(&self, local: String, remote_dir: String) {
        let _ = self.commands.send(SftpCommand::Upload { local, remote_dir });
    }
    pub fn toggle_tree_node(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ToggleTreeNode(path));
    }
    pub fn delete(&self, path: String) {
        let _ = self.commands.send(SftpCommand::Delete(path));
    }
    pub fn open_temp(&self, remote: String, edit: bool) {
        let _ = self.commands.send(SftpCommand::OpenTemp { remote, edit });
    }
    pub fn close(&self) {
        let _ = self.commands.send(SftpCommand::Close);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Spawn an SFTP worker on the Tokio runtime.
///
/// The worker opens its own SSH connection to the same server, authenticates,
/// and requests the `sftp` subsystem. Events (directory listings, progress,
/// errors) are sent back via `events`, which is the same sender used by the
/// terminal's shell session.
pub fn spawn_sftp(
    runtime: &tokio::runtime::Handle,
    session: Session,
    events: UnboundedSender<SessionEvent>,
) -> SftpHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let self_tx = cmd_tx.clone();
    let events_err = events.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_sftp(session, cmd_rx, self_tx, events).await {
            let _ = events_err.send(SessionEvent::SftpStatus(
                format!("SFTP 错误: {err:#}"),
            ));
        }
    });
    SftpHandle { commands: cmd_tx, join }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tree state helpers
// ---------------------------------------------------------------------------

/// Recursively build the flat node list from tree state (DFS pre-order).
fn build_tree_nodes(
    path: &str,
    depth: u32,
    expanded: &std::collections::HashSet<String>,
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    nodes: &mut Vec<RemoteTreeNode>,
) {
    let name = if path == "/" {
        "/".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    let children = tree_dirs.get(path);
    let has_children = children.map(|c| !c.is_empty()).unwrap_or(true);
    let is_expanded = expanded.contains(path);
    nodes.push(RemoteTreeNode { path: path.to_string(), name, depth, expanded: is_expanded, has_children });
    if is_expanded {
        if let Some(ch) = children {
            for (_, child_path) in ch {
                build_tree_nodes(child_path, depth + 1, expanded, tree_dirs, nodes);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn run_sftp(
    session: Session,
    mut commands: UnboundedReceiver<SftpCommand>,
    self_tx: UnboundedSender<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let _ = events.send(SessionEvent::SftpStatus("SFTP 连接中...".into()));

    // Open a dedicated SSH connection for SFTP.
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 30)),
        ..<_>::default()
    });

    let addr = format!("{}:{}", session.host, session.port);
    let mut handle = client::connect(config, addr.as_str(), SftpClientHandler)
        .await
        .with_context(|| format!("sftp connect {} failed", addr))?;

    // --- Authenticate (same method as the shell session) -------------------
    let authed = match session.auth {
        AuthMethod::Password => handle
            .authenticate_password(&session.user, &session.password)
            .await
            .context("sftp password auth failed")?,
        AuthMethod::Key => {
            if session.private_key_path.trim().is_empty() {
                return Err(anyhow!("私钥路径为空"));
            }
            let keypair =
                load_secret_key(Path::new(&session.private_key_path), None)
                    .with_context(|| {
                        format!("failed to load key {}", session.private_key_path)
                    })?;
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), None)
                .context("invalid private key")?;
            handle
                .authenticate_publickey(&session.user, key_with_hash)
                .await
                .context("sftp publickey auth failed")?
        }
    };

    if !authed {
        return Err(anyhow!("SFTP 认证失败"));
    }

    // --- Open the sftp subsystem channel -----------------------------------
    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("sftp handshake")?;

    // Resolve the home directory and do an initial listing.
    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".to_string());
    let _ = events.send(SessionEvent::SftpStatus(format!("SFTP 加载 {}...", home)));
    match list_dir_impl(&sftp, &home).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: home.clone(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(home.clone()));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpStatus(format!("SFTP 错误: {e}")));
        }
    }

    // --- Directory tree initialization -------------------------------------
    // tree_dirs: path -> [(child_name, child_full_path)] for directories only
    // tree_expanded: set of paths currently shown as expanded
    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // Fetch root "/" subdirs, then expand path down to home.
    let root_dirs = list_dirs_only_impl(&sftp, "/").await.unwrap_or_default();
    tree_dirs.insert("/".to_string(), root_dirs);
    tree_expanded.insert("/".to_string());

    // Walk each path segment from "/" toward home, expanding as we go.
    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() { continue; }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            // Only expand if this child appeared in the parent listing.
            let found = tree_dirs.get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found { break; }
            let dirs = list_dirs_only_impl(&sftp, &child).await.unwrap_or_default();
            tree_dirs.insert(child.clone(), dirs);
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    {
        let mut nodes = Vec::new();
        build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
        let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
    }

    // --- Command loop -------------------------------------------------------
    while let Some(cmd) = commands.recv().await {
        match cmd {
            SftpCommand::Close => break,

            SftpCommand::ListDir(path) => {
                let _ = events.send(SessionEvent::SftpStatus(format!("加载 {}...", path)));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        let _ = events.send(SessionEvent::SftpEntries {
                            path: path.clone(),
                            entries,
                        });
                        let _ = events.send(SessionEvent::SftpStatus(path));
                    }
                    Err(e) => {
                        let _ =
                            events.send(SessionEvent::SftpStatus(format!("列目录失败: {e}")));
                    }
                }
            }

            SftpCommand::ToggleTreeNode(path) => {
                if tree_expanded.contains(&path) {
                    // Collapse this node and all descendants.
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    // Expand: fetch children if not yet cached.
                    if !tree_dirs.contains_key(&path) {
                        let dirs = list_dirs_only_impl(&sftp, &path).await.unwrap_or_default();
                        tree_dirs.insert(path.clone(), dirs);
                    }
                    tree_expanded.insert(path.clone());
                }
                let mut nodes = Vec::new();
                build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
                let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
            }

            SftpCommand::Download { remote, local_dir } => {
                let filename = base_name(&remote);
                let local_path =
                    format!("{}/{}", local_dir.trim_end_matches('/'), filename);
                let id = Uuid::new_v4().to_string();
                let _ =
                    events.send(SessionEvent::SftpStatus(format!("下载 {}...", filename)));
                match download_impl(&sftp, &remote, &local_path, &filename, &id, &events)
                    .await
                {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "下载完成: {}",
                            filename
                        )));
                    }
                    Err(e) => {
                        emit_transfer(&events, &id, &filename, false, 0, 0, 2);
                        let _ = events
                            .send(SessionEvent::SftpStatus(format!("下载失败: {e}")));
                    }
                }
            }

            SftpCommand::Upload { local, remote_dir } => {
                let filename = base_name(&local);
                let remote_path =
                    format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                let id = Uuid::new_v4().to_string();
                let _ =
                    events.send(SessionEvent::SftpStatus(format!("上传 {}...", filename)));
                match upload_impl(&sftp, &local, &remote_path, &filename, &id, &events).await
                {
                    Ok(_) => {
                        if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: remote_dir.clone(),
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "上传完成: {}",
                            filename
                        )));
                    }
                    Err(e) => {
                        emit_transfer(&events, &id, &filename, true, 0, 0, 2);
                        let _ = events
                            .send(SessionEvent::SftpStatus(format!("上传失败: {e}")));
                    }
                }
            }

            SftpCommand::Delete(path) => {
                let filename = base_name(&path);
                let _ =
                    events.send(SessionEvent::SftpStatus(format!("删除 {}...", filename)));
                // Try as a file first, then as an (empty) directory.
                let res = match sftp.remove_file(&path).await {
                    Ok(_) => Ok(()),
                    Err(_) => sftp.remove_dir(&path).await.map(|_| ()),
                };
                match res {
                    Ok(_) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: parent.clone(),
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "已删除: {}",
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events
                            .send(SessionEvent::SftpStatus(format!("删除失败: {e}")));
                    }
                }
            }

            SftpCommand::OpenTemp { remote, edit } => {
                let filename = base_name(&remote);
                let tmp_dir = std::env::temp_dir().join("meatshell");
                let _ = tokio::fs::create_dir_all(&tmp_dir).await;
                let local = tmp_dir.join(&filename);
                let local_str = local.to_string_lossy().to_string();
                let _ =
                    events.send(SessionEvent::SftpStatus(format!("打开 {}...", filename)));
                let xid = Uuid::new_v4().to_string();
                match download_impl(&sftp, &remote, &local_str, &filename, &xid, &events).await {
                    Ok(_) => {
                        open_with_os(&local_str);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "已{}: {}",
                            if edit { "打开编辑" } else { "打开" },
                            filename
                        )));
                        if edit {
                            spawn_edit_watcher(
                                self_tx.clone(),
                                local_str,
                                remote.clone(),
                                filename,
                                events.clone(),
                            );
                        }
                    }
                    Err(e) => {
                        let _ = events
                            .send(SessionEvent::SftpStatus(format!("打开失败: {e}")));
                    }
                }
            }
        }
    }

    let _ = handle.disconnect(Disconnect::ByApplication, "bye", "").await;
    Ok(())
}

/// File name component of a path.  Handles both remote (`/`) and local Windows
/// (`\`) separators, so uploading `C:\…\frp.tar.gz` yields `frp.tar.gz` rather
/// than the whole path (which previously became the remote file name).
fn base_name(path: &str) -> String {
    let sep = |c: char| c == '/' || c == '\\';
    path.trim_end_matches(sep)
        .rsplit(sep)
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Parent directory of a remote path ("/a/b" → "/a", "/a" → "/").
fn parent_dir(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// Open a local file with the OS default application.
fn open_with_os(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", path])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// Watch a downloaded temp file and re-upload it to the remote whenever it
/// changes on disk (the "edit" flow).  Re-upload is routed back through the
/// worker's own command channel.  Stops when the channel closes or after a
/// generous idle window.
fn spawn_edit_watcher(
    self_tx: UnboundedSender<SftpCommand>,
    local: String,
    remote: String,
    filename: String,
    events: UnboundedSender<SessionEvent>,
) {
    let remote_dir = parent_dir(&remote);
    tokio::spawn(async move {
        let mtime = |p: &str| {
            std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
        };
        let mut last = mtime(&local);
        // ~40 min of 2s polls; also exits early once the worker is gone.
        for _ in 0..1200 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if self_tx.is_closed() {
                break;
            }
            let cur = mtime(&local);
            if cur.is_some() && cur != last {
                last = cur;
                let _ = self_tx.send(SftpCommand::Upload {
                    local: local.clone(),
                    remote_dir: remote_dir.clone(),
                });
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "已上传修改: {}",
                    filename
                )));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// SFTP helpers
// ---------------------------------------------------------------------------

async fn list_dir_impl(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>> {
    let raw = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("read_dir {path} failed"))?;

    let mut entries: Vec<RemoteEntry> = raw
        .into_iter()
        .filter(|e| {
            let n = e.file_name();
            n != "." && n != ".."
        })
        .map(|e| {
            let name = e.file_name().to_string();
            let full_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let meta = e.metadata();
            // Determine if entry is a directory via Unix permission bits.
            let permissions = meta.permissions.unwrap_or(0);
            let is_dir = (permissions & 0o170_000) == 0o040_000;
            let size = meta.size.unwrap_or(0);
            let modified = meta.mtime.unwrap_or(0);
            RemoteEntry { name, full_path, is_dir, size, modified }
        })
        .collect();

    // Sort: directories first, then files; both groups alphabetically.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

/// List only the subdirectories of `path` (no files). Used to build the tree.
async fn list_dirs_only_impl(sftp: &SftpSession, path: &str) -> Result<Vec<(String, String)>> {
    let entries = list_dir_impl(sftp, path).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

/// Emit a transfer-progress event.
fn emit_transfer(
    events: &UnboundedSender<SessionEvent>,
    id: &str,
    name: &str,
    is_upload: bool,
    transferred: u64,
    total: u64,
    state: u8,
) {
    let _ = events.send(SessionEvent::SftpTransfer {
        id: id.to_string(),
        name: name.to_string(),
        is_upload,
        transferred,
        total,
        state,
    });
}

const XFER_CHUNK: usize = 64 * 1024;

async fn download_impl(
    sftp: &SftpSession,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let total = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    let mut remote_file = sftp
        .open(remote)
        .await
        .with_context(|| format!("open remote {remote}"))?;
    let mut local_file = tokio::fs::File::create(local)
        .await
        .with_context(|| format!("create local {local}"))?;

    emit_transfer(events, id, name, false, 0, total, 0);
    let mut buf = vec![0u8; XFER_CHUNK];
    let mut done: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = remote_file.read(&mut buf).await.context("read remote file")?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buf[..n])
            .await
            .context("write local file")?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(150) {
            last = Instant::now();
            emit_transfer(events, id, name, false, done, total, 0);
        }
    }
    local_file.flush().await.context("flush local file")?;
    emit_transfer(events, id, name, false, done, total.max(done), 1);
    Ok(())
}

async fn upload_impl(
    sftp: &SftpSession,
    local: &str,
    remote: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let total = tokio::fs::metadata(local)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut local_file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open local {local}"))?;
    let mut remote_file = sftp
        .create(remote)
        .await
        .with_context(|| format!("create remote {remote}"))?;

    emit_transfer(events, id, name, true, 0, total, 0);
    let mut buf = vec![0u8; XFER_CHUNK];
    let mut done: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = local_file.read(&mut buf).await.context("read local file")?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buf[..n])
            .await
            .context("write remote file")?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(150) {
            last = Instant::now();
            emit_transfer(events, id, name, true, done, total, 0);
        }
    }
    remote_file.flush().await.context("flush remote file")?;
    emit_transfer(events, id, name, true, done, total.max(done), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// russh client handler (accept any server key, same as the shell handler)
// ---------------------------------------------------------------------------

struct SftpClientHandler;

#[async_trait]
impl Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Keep format helpers and RemoteTreeNode imports live.
const _: fn() = || {
    let _ = format_size(0);
    let _ = format_mtime(0);
    let _: RemoteTreeNode;
};
