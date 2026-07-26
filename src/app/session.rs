use std::collections::VecDeque;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use slint::{ModelRc, SharedString, VecModel};

use crate::config::{AuthMethod, ConfigStore, Secret, Session, SessionKind};
use crate::i18n::t;
use crate::session::ConnectCtx;
use crate::sftp::spawn_sftp;
use crate::ssh::{spawn_session, SessionEvent};

use super::*;

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

/// Distinct named groups (explicit folders ∪ the groups sessions are filed under),
/// de-duplicated and sorted alphabetically — feeds the new/edit dialog's group
/// dropdown (#179). Ungrouped ("") is excluded; the dialog leaves the field blank
/// for that case.
pub(crate) fn session_groups_model(store: &ConfigStore) -> ModelRc<SharedString> {
    let sessions = store.sessions();
    let mut named: Vec<String> = store
        .groups()
        .iter()
        .cloned()
        .chain(
            sessions
                .iter()
                .filter(|s| !s.group.is_empty())
                .map(|s| s.group.clone()),
        )
        .collect();
    named.sort_by_key(|g| g.to_lowercase());
    named.dedup();
    ModelRc::from(Rc::new(VecModel::from(
        named
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

/// Build the jump-host picker's parallel label/id lists for the session dialog
/// (#211). Index 0 is always the "no jump host" entry (empty id); the rest are
/// the saved SSH sessions except `exclude_id` (a session can't jump through
/// itself). Returns `(labels, ids, selected_index)` where `selected_index`
/// points at `current_jump_id` (0 if unset / dangling).
pub(crate) fn jump_candidates(
    store: &ConfigStore,
    exclude_id: &str,
    current_jump_id: &str,
) -> (ModelRc<SharedString>, ModelRc<SharedString>, i32) {
    let mut labels: Vec<SharedString> = vec![t("无（直接连接）", "None (direct)").into()];
    let mut ids: Vec<SharedString> = vec!["".into()];
    let mut selected: i32 = 0;
    for s in store.sessions() {
        if s.kind != SessionKind::Ssh || s.id == exclude_id {
            continue;
        }
        let label = if s.name.trim().is_empty() {
            if s.user.trim().is_empty() {
                s.host.clone()
            } else {
                format!("{}@{}", s.user, s.host)
            }
        } else {
            format!("{} ({}@{})", s.name, s.user, s.host)
        };
        if s.id == current_jump_id {
            selected = ids.len() as i32;
        }
        labels.push(label.into());
        ids.push(s.id.clone().into());
    }
    (
        ModelRc::from(Rc::new(VecModel::from(labels))),
        ModelRc::from(Rc::new(VecModel::from(ids))),
        selected,
    )
}

pub(crate) fn sync_sessions_to_model(store: &ConfigStore, model: &VecModel<SessionInfo>) {
    // Group sessions by their `group` (named groups alphabetically, ungrouped
    // last), then by name within each group, and tag the first row of every
    // group with a header so the welcome list can render a folder heading (#41).
    let sessions = store.sessions();

    // Ordered list of display groups:
    //  - "default" only when there are ungrouped sessions (group == "")
    //  - named groups: explicit folders (incl. empty ones) ∪ sessions' groups,
    //    de-duplicated, alphabetical.
    let has_default = sessions.iter().any(|s| s.group.is_empty());
    let mut named: Vec<String> = store
        .groups()
        .iter()
        .cloned()
        .chain(
            sessions
                .iter()
                .filter(|s| !s.group.is_empty())
                .map(|s| s.group.clone()),
        )
        .collect();
    named.sort_by_key(|g| g.to_lowercase());
    named.dedup();

    let mut display_groups: Vec<String> = Vec::new();
    if has_default {
        display_groups.push("default".to_string());
    }
    display_groups.extend(named);

    // Placeholder row for an empty folder; id == "" marks it as a group header
    // with no session (used by the UI to gate the "delete group" action).
    let blank = |group: &str| SessionInfo {
        id: "".into(),
        name: "".into(),
        host: "".into(),
        port: 0,
        user: "".into(),
        auth: "".into(),
        last_used: "".into(),
        group: group.into(),
        group_header: group.into(),
        collapsed: false,
    };

    let mut rows: Vec<SessionInfo> = Vec::new();
    for (i, s) in builtin_local_sessions().iter().enumerate() {
        rows.push(SessionInfo {
            id: s.id.clone().into(),
            name: s.name.clone().into(),
            host: s.host.clone().into(),
            port: 0,
            user: s.user.clone().into(),
            auth: s.kind.as_str().into(),
            last_used: "".into(),
            group: "system".into(),
            group_header: if i == 0 { "system".into() } else { "".into() },
            collapsed: true,
        });
    }
    for group in &display_groups {
        let mut gs: Vec<&Session> = if group == "default" {
            sessions.iter().filter(|s| s.group.is_empty()).collect()
        } else {
            sessions.iter().filter(|s| &s.group == group).collect()
        };
        gs.sort_by_key(|s| s.name.to_lowercase());

        if gs.is_empty() {
            rows.push(blank(group));
        } else {
            for (i, s) in gs.iter().enumerate() {
                rows.push(SessionInfo {
                    id: s.id.clone().into(),
                    name: s.name.clone().into(),
                    host: s.host.clone().into(),
                    port: s.port as i32,
                    user: s.user.clone().into(),
                    auth: s.auth.as_str().into(),
                    last_used: s
                        .last_used
                        .clone()
                        .unwrap_or_else(|| "never".to_string())
                        .into(),
                    group: group.clone().into(),
                    group_header: if i == 0 {
                        group.clone().into()
                    } else {
                        "".into()
                    },
                    collapsed: false,
                });
            }
        }
    }
    model.set_vec(rows);
}

pub(crate) fn builtin_local_sessions() -> Vec<Session> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        out.push(builtin_local_session("system:powershell", "PowerShell", "powershell"));
        out.push(builtin_local_session("system:cmd", "CMD", "cmd"));
        if wsl_available() {
            out.push(builtin_local_session("system:wsl", "WSL", "wsl"));
        }
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let name = std::path::Path::new(&shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Shell")
            .to_string();
        out.push(builtin_local_session("system:shell", name, "shell"));
    }
    out
}

pub(crate) fn builtin_local_session(id: &str, name: impl Into<String>, host: &str) -> Session {
    let mut s = Session::new_empty();
    s.id = id.to_string();
    s.name = name.into();
    s.host = host.to_string();
    s.user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    s.group = "system".to_string();
    s.kind = SessionKind::Local;
    s
}

#[cfg(windows)]
pub(crate) fn wsl_available() -> bool {
    use std::os::windows::process::CommandExt;

    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("wsl.exe")
            .arg("--status")
            .creation_flags(0x08000000)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Build the effective session represented by the dialog. When editing, blank
/// secret fields retain their saved values because real passwords and pasted
/// private keys are deliberately never echoed back into the UI (#10, #276).
pub(crate) fn session_from_draft(
    draft: &SessionDraft,
    existing: Option<&Session>,
    forwards: Vec<crate::config::PortForward>,
) -> Session {
    let password = if draft.password.is_empty() {
        existing.map(|s| s.password.clone()).unwrap_or_default()
    } else {
        Secret::new(draft.password.to_string())
    };
    let private_key_inline = if draft.private_key_inline_mode {
        if draft.private_key_inline.is_empty() {
            existing
                .map(|s| s.private_key_inline.clone())
                .unwrap_or_default()
        } else {
            Secret::new(draft.private_key_inline.to_string())
        }
    } else {
        Secret::default()
    };
    let private_key_path = if draft.private_key_inline_mode {
        String::new()
    } else {
        draft.private_key_path.to_string().replace('\\', "/")
    };
    let kind = SessionKind::from_str(&draft.kind.to_string());
    let auto_name = match kind {
        SessionKind::Serial => format!("{} @{}", draft.serial_port, draft.baud_rate),
        _ if draft.user.trim().is_empty() => draft.host.to_string(),
        _ => format!("{}@{}", draft.user, draft.host),
    };
    let default_port = if kind == SessionKind::Telnet { 23 } else { 22 };

    Session {
        id: draft.id.to_string(),
        name: if draft.name.is_empty() {
            auto_name
        } else {
            draft.name.to_string()
        },
        host: draft.host.to_string(),
        port: if draft.port <= 0 {
            default_port
        } else {
            draft.port as u16
        },
        user: draft.user.to_string(),
        auth: AuthMethod::from_str(&draft.auth.to_string()),
        password,
        private_key_path,
        private_key_inline,
        proxy: draft.proxy.to_string(),
        last_used: None,
        group: draft.group.to_string(),
        kind,
        serial_port: draft.serial_port.to_string(),
        baud_rate: if draft.baud_rate <= 0 {
            115_200
        } else {
            draft.baud_rate as u32
        },
        data_bits: draft.data_bits as u8,
        stop_bits: draft.stop_bits as u8,
        parity: draft.parity.to_string(),
        flow_control: draft.flow_control.to_string(),
        forwards,
        disable_shell_integration: draft.disable_shell_integration,
        note: draft.note.to_string(),
        jump_session_id: draft.jump_session_id.to_string(),
    }
}

/// Resolve a session's configured SSH jump host to the saved session it points
/// at, ignoring a missing / dangling / self reference (#211).
pub(crate) fn resolve_jump(store: &Rc<RefCell<ConfigStore>>, session: &Session) -> Option<Session> {
    if session.kind != SessionKind::Ssh || session.jump_session_id.trim().is_empty() {
        return None;
    }
    if session.jump_session_id == session.id {
        return None;
    }
    store.borrow().get(&session.jump_session_id).cloned()
}

/// Spawn the shell (+ SFTP) workers and their event-pump threads for an
/// already-registered tab. Used by the initial connect and by in-place
/// reconnect (#79); the tab/terminal/parser must already exist.
pub(crate) fn start_session_in_tab(tab_id: &str, session: Session, ctx: &ConnectCtx) {
    let has_sftp = session.kind == SessionKind::Ssh;
    let (initial_cols, initial_rows) = *ctx.last_term_size.lock().unwrap();
    // Resolve the optional SSH jump host now (on the UI thread, where the store
    // lives) so the owned Session can be handed to the worker threads (#211).
    let jump = resolve_jump(&ctx.store, &session);
    let (handle, rx) = match session.kind {
        SessionKind::Ssh => spawn_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
            jump.clone(),
            initial_cols,
            initial_rows,
        ),
        SessionKind::Serial => crate::terminal::serial::spawn_serial_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
        ),
        SessionKind::Telnet => crate::terminal::telnet::spawn_telnet_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
        SessionKind::Local => crate::terminal::local::spawn_local_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
    };
    ctx.handles.borrow_mut().insert(tab_id.to_string(), handle);

    // Separate SFTP connection for the same session (SSH only). It waits for
    // the interactive PTY to report Connected so a second SSH handshake cannot
    // contend with terminal startup on the same host/network path.
    let (sftp_evt_tx, sftp_ready_tx) = if has_sftp {
        let (sftp_tx, sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let sftp_runtime = ctx.runtime.clone();
        let sftp_task_runtime = sftp_runtime.clone();
        let sftp_handles = ctx.sftp_handles.clone();
        let sftp_tab_id = tab_id.to_string();
        sftp_runtime.spawn(async move {
            if ready_rx.await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
            let sftp_handle =
                spawn_sftp(sftp_task_runtime.handle(), session, jump, sftp_tx);
            if let Ok(mut handles) = sftp_handles.lock() {
                handles.insert(sftp_tab_id, sftp_handle);
            }
        });
        (Some(sftp_rx), Some(ready_tx))
    } else {
        (None, None)
    };

    // --- Shell event pump (dedicated thread) ---
    {
        let weak_inner = ctx.weak.clone();
        let bufs_thread = ctx.bufs.clone();
        let sftp_handles_pump = ctx.sftp_handles.clone();
        let sftp_last_cwd_pump = ctx.sftp_last_cwd.clone();
        let rt_pump = ctx.runtime.clone();
        let tab_id_pump = tab_id.to_string();
        let statuses_pump = ctx.tab_statuses.clone();
        let local_pump = ctx.local_snap.clone();
        let net_pump = ctx.local_net_hist.clone();
        let follow_cd_pump = ctx.sftp_follow_cd.clone();
        let render_gates_pump = ctx.render_gates.clone();
        std::thread::spawn(move || {
            let mut shell_rx = rx;
            let mut sftp_ready_tx = sftp_ready_tx;
            let mut cwd_debounce: Option<tokio::task::JoinHandle<()>> = None;
            // Reusable scratch so a fast firehose doesn't reallocate every batch.
            let mut drained: Vec<SessionEvent> = Vec::new();
            loop {
                // Block for the first event, then sweep up everything else that's
                // already queued. A burst — e.g. `tail -f` on a busy log (#171) —
                // then collapses into ONE invoke_from_event_loop and (after merging
                // adjacent Output below) ONE vt100 ingest + render, instead of one
                // UI task per chunk flooding the event loop and freezing the app.
                match shell_rx.blocking_recv() {
                    None => break,
                    Some(first) => drained.push(first),
                }
                // Cap the sweep so an unending stream still yields to the renderer
                // between batches (keeps the UI live rather than starved).
                const DRAIN_CAP: usize = 2048;
                while drained.len() < DRAIN_CAP {
                    match shell_rx.try_recv() {
                        Ok(evt) => drained.push(evt),
                        Err(_) => break,
                    }
                }

                // Run CwdChanged side-effects here (off the UI thread), drop the
                // swallowed ones, and concatenate runs of Output into a single chunk
                // so the UI parses + renders the whole burst once.
                let mut ui_batch: Vec<SessionEvent> = Vec::with_capacity(drained.len());
                for evt in drained.drain(..) {
                    match evt {
                        SessionEvent::Connected => {
                            if let Some(ready) = sftp_ready_tx.take() {
                                let _ = ready.send(());
                            }
                            ui_batch.push(SessionEvent::Connected);
                        }
                        SessionEvent::CwdChanged(cwd) => {
                            // Shared map (not a thread-local) so manual SFTP
                            // navigation can clear the entry — then the very next
                            // OSC 7, same directory or not, snaps the panel back to
                            // the shell's cwd. Unchanged repeats (every prompt
                            // re-emits OSC 7) are ignored (#59).
                            let changed = match sftp_last_cwd_pump.lock() {
                                Ok(mut m) => {
                                    m.insert(tab_id_pump.clone(), cwd.clone()).as_deref()
                                        != Some(cwd.as_str())
                                }
                                Err(_) => false,
                            };
                            // Swallow when follow-cd is off: forwarding it would set
                            // sftp_loading without any ListDir to clear it (the #59
                            // stuck-"loading" trap).
                            if !changed
                                || !follow_cd_pump.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                continue;
                            }
                            if let Some(prev) = cwd_debounce.take() {
                                prev.abort();
                            }
                            let cwd_spawn = cwd.clone();
                            let sftp_h = sftp_handles_pump.clone();
                            let tid = tab_id_pump.clone();
                            cwd_debounce = Some(rt_pump.spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                if let Ok(handles) = sftp_h.lock() {
                                    if let Some(h) = handles.get(&tid) {
                                        h.list_dir(cwd_spawn);
                                    }
                                }
                            }));
                            ui_batch.push(SessionEvent::CwdChanged(cwd));
                        }
                        SessionEvent::Output(chunk) => {
                            // Merge with the immediately preceding Output so the
                            // whole run is one vt100 ingest + one render. Only
                            // *adjacent* chunks merge, so byte order (and any
                            // interleaved event) is preserved exactly. Cap the
                            // merged size so one batch can't monopolize the UI
                            // thread for hundreds of ms (#209).
                            if let Some(SessionEvent::Output(prev)) = ui_batch.last_mut() {
                                if prev.len() + chunk.len() <= super::OUTPUT_MERGE_BYTE_CAP {
                                    prev.push_str(&chunk);
                                } else {
                                    ui_batch.push(SessionEvent::Output(chunk));
                                }
                            } else {
                                ui_batch.push(SessionEvent::Output(chunk));
                            }
                        }
                        other => ui_batch.push(other),
                    }
                }
                if ui_batch.is_empty() {
                    continue;
                }

                // Ingest terminal output on this pump thread (not the UI thread)
                // so a firehose can't block keyboard input or repaints (#209).
                let mut had_output = false;
                let mut ui_only: Vec<SessionEvent> = Vec::with_capacity(ui_batch.len());
                for evt in ui_batch {
                    match evt {
                        SessionEvent::Output(chunk) => {
                            super::ingest_terminal_output(&bufs_thread, &tab_id_pump, chunk.as_bytes());
                            had_output = true;
                        }
                        other => ui_only.push(other),
                    }
                }

                if had_output {
                    super::request_tab_render(
                        weak_inner.clone(),
                        &tab_id_pump,
                        &bufs_thread,
                        &render_gates_pump,
                    );
                }

                if ui_only.is_empty() {
                    continue;
                }

                let weak_evt = weak_inner.clone();
                let tid = tab_id_pump.clone();
                let bufs_evt = bufs_thread.clone();
                let st_evt = statuses_pump.clone();
                let lc_evt = local_pump.clone();
                let nh_evt = net_pump.clone();
                let gates_evt = render_gates_pump.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = weak_evt.upgrade() {
                        for evt in ui_only {
                            apply_session_event_to_window(
                                &win, &tid, evt, &bufs_evt, &gates_evt, &st_evt, &lc_evt, &nh_evt,
                            );
                        }
                    }
                });
            }
        });
    }

    // --- SFTP event pump (separate thread, SSH only) ---
    if let Some(sftp_evt_tx) = sftp_evt_tx {
        let weak_sftp = ctx.weak.clone();
        let bufs_sftp = ctx.bufs.clone();
        let tab_id_sftp = tab_id.to_string();
        let statuses_sftp = ctx.tab_statuses.clone();
        let local_sftp = ctx.local_snap.clone();
        let net_sftp = ctx.local_net_hist.clone();
        let gates_sftp = ctx.render_gates.clone();
        std::thread::spawn(move || {
            let mut sftp_rx = sftp_evt_tx;
            let mut drained: Vec<SessionEvent> = Vec::new();
            loop {
                match sftp_rx.blocking_recv() {
                    None => break,
                    Some(first) => drained.push(first),
                }
                const SFTP_DRAIN_CAP: usize = 256;
                while drained.len() < SFTP_DRAIN_CAP {
                    match sftp_rx.try_recv() {
                        Ok(evt) => drained.push(evt),
                        Err(_) => break,
                    }
                }
                let ui_batch: Vec<SessionEvent> = drained.drain(..).collect();
                if ui_batch.is_empty() {
                    continue;
                }
                let weak_s = weak_sftp.clone();
                let tid = tab_id_sftp.clone();
                let bufs_s = bufs_sftp.clone();
                let st_s = statuses_sftp.clone();
                let lc_s = local_sftp.clone();
                let nh_s = net_sftp.clone();
                let gates_s = gates_sftp.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = weak_s.upgrade() {
                        for sftp_evt in ui_batch {
                            apply_session_event_to_window(
                                &win, &tid, sftp_evt, &bufs_s, &gates_s, &st_s, &lc_s, &nh_s,
                            );
                        }
                    }
                });
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Session callbacks (welcome page + dialog)
// ---------------------------------------------------------------------------

pub(crate) fn wire_session_callbacks(window: &AppWindow, ctx: Rc<AppContext>) {
    let store = ctx.store.clone();
    let sessions_model = ctx.sessions_model.clone();
    let tabs_model = ctx.tabs_model.clone();
    let terminals_model = ctx.terminals_model.clone();
    let layout = ctx.layout.clone();
    let content_size = ctx.content_size.clone();
    let panes_model = ctx.panes_model.clone();
    let splitters_model = ctx.splitters_model.clone();
    let handles = ctx.handles.clone();
    let bufs = ctx.bufs.clone();
    let render_gates = ctx.render_gates.clone();
    let runtime = ctx.runtime.clone();
    let last_term_size = ctx.last_term_size.clone();
    let sftp_handles = ctx.sftp_handles.clone();
    let sftp_last_cwd = ctx.sftp_last_cwd.clone();
    let tab_statuses = ctx.tab_statuses.clone();
    let local_snap = ctx.local_snap.clone();
    let local_net_hist = ctx.local_net_hist.clone();
    let sftp_follow_cd = ctx.sftp_follow_cd.clone();
    // Working set of port forwards (#56) for the session being created/edited.
    // The forward add/delete callbacks mutate it; saving reads it into
    // Session.forwards; opening the dialog (new/edit) resets it.
    let edit_forwards: Rc<RefCell<Vec<PortFwd>>> =
        Rc::new(RefCell::new(vec![blank_forward_draft()]));

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    let store_ng = store.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            *ef_new.borrow_mut() = vec![blank_forward_draft()];
            w.set_session_groups(session_groups_model(&store_ng.borrow()));
            w.set_dialog_forwards(forward_model(&ef_new.borrow()));
            let empty = Session::new_empty();
            let (jump_labels, jump_ids, jump_idx) =
                jump_candidates(&store_ng.borrow(), &empty.id, "");
            w.set_jump_choices(jump_labels);
            w.set_jump_ids(jump_ids);
            w.set_dialog_jump_index(jump_idx);
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            // No default username (#110): leaving it blank makes the connect-time
            // prompt ask for it, Xshell-style.
            w.set_dialog_user("".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_key_inline("".into());
            w.set_dialog_key_inline_mode(false);
            w.set_dialog_test_status("".into());
            w.set_dialog_proxy_type("none".into());
            w.set_dialog_proxy_hostport("".into());
            w.set_dialog_group("".into());
            w.set_dialog_kind("ssh".into());
            w.set_dialog_serial_port("".into());
            w.set_dialog_baud("115200".into());
            w.set_dialog_data_bits("8".into());
            w.set_dialog_stop_bits("1".into());
            w.set_dialog_parity("none".into());
            w.set_dialog_flow("none".into());
            w.set_dialog_disable_shell_integration(false);
            w.set_dialog_note("".into());
            w.set_dialog_editing(false);
            w.set_dialog_open(true);
        }
    });

    // Import hosts from ~/.ssh/config -> add them as sessions (skipping dups).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_ssh_config(move || {
            let hosts = crate::ssh::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(
                        t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                    );
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == h.user));
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() {
                            "root".into()
                        } else {
                            h.user
                        },
                        auth,
                        private_key_path: h.identity_file,
                        ..Session::new_empty()
                    });
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if added > 0 {
                    format!("{} {}", t("已导入", "imported"), added)
                } else {
                    t("没有新主机可导入", "no new hosts to import").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Export all sessions to a portable JSON file (issue #46). Passwords are
    // obfuscated with the built-in export key; host/user/port stay plaintext.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("meatshell-connections.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let res = store.borrow().export_to(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok(n) => format!("{} {}", t("已导出连接", "exported"), n),
                        Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Batch-import connections from pasted text (#150). One per line:
    // `host|port|user|password|name` (trailing fields optional).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_batch_import_confirm(move |text: SharedString| {
            let parsed = parse_batch_import(text.as_str());
            let total = parsed.len();
            let mut added = 0usize;
            {
                let mut s = store.borrow_mut();
                for sess in parsed {
                    // Skip a host/user/port we already have.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.host == sess.host && x.user == sess.user && x.port == sess.port);
                    if dup {
                        continue;
                    }
                    s.upsert(sess);
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if total == 0 {
                    t("没有可导入的连接", "nothing to import").to_string()
                } else if added > 0 {
                    format!("{} {}/{}", t("已导入", "imported"), added, total)
                } else {
                    t("没有新连接可导入(已存在)", "no new connections (all exist)").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Import sessions from a portable JSON file (issue #46).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                let res = store.borrow_mut().import_from(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok((added, skipped)) => {
                            sync_sessions_to_model(&store.borrow(), &sessions_model);
                            format!(
                                "{} {} / {} {}",
                                t("已导入", "imported"),
                                added,
                                t("跳过重复", "skipped"),
                                skipped
                            )
                        }
                        Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Edit -> open dialog prefilled.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ef_edit = edit_forwards.clone();
        window.on_edit_session(move |id: SharedString| {
            let id = id.to_string();
            let store = store.borrow();
            let Some(session) = store.get(&id) else {
                return;
            };
            *ef_edit.borrow_mut() = forward_drafts(&session.forwards);
            if ef_edit.borrow().is_empty() {
                ef_edit.borrow_mut().push(blank_forward_draft());
            }
            if let Some(w) = weak.upgrade() {
                w.set_session_groups(session_groups_model(&store));
                w.set_dialog_forwards(forward_model(&ef_edit.borrow()));
                w.set_dialog_id(session.id.clone().into());
                w.set_dialog_name(session.name.clone().into());
                w.set_dialog_host(session.host.clone().into());
                w.set_dialog_port(session.port.to_string().into());
                w.set_dialog_user(session.user.clone().into());
                w.set_dialog_auth(session.auth.as_str().into());
                // Never echo the stored password back into the UI (issue #10) —
                // leave it blank; a blank field on save keeps the existing one.
                w.set_dialog_password("".into());
                w.set_dialog_key_path(session.private_key_path.clone().into());
                w.set_dialog_key_inline("".into());
                w.set_dialog_key_inline_mode(!session.private_key_inline.is_empty());
                w.set_dialog_test_status("".into());
                let (proxy_type, proxy_hostport) = split_proxy(&session.proxy);
                w.set_dialog_proxy_type(proxy_type.into());
                w.set_dialog_proxy_hostport(proxy_hostport.into());
                let (jump_labels, jump_ids, jump_idx) =
                    jump_candidates(&store, &session.id, &session.jump_session_id);
                w.set_jump_choices(jump_labels);
                w.set_jump_ids(jump_ids);
                w.set_dialog_jump_index(jump_idx);
                w.set_dialog_group(session.group.clone().into());
                w.set_dialog_kind(session.kind.as_str().into());
                w.set_dialog_serial_port(session.serial_port.clone().into());
                w.set_dialog_baud(session.baud_rate.to_string().into());
                w.set_dialog_data_bits(session.data_bits.to_string().into());
                w.set_dialog_stop_bits(session.stop_bits.to_string().into());
                w.set_dialog_parity(session.parity.clone().into());
                w.set_dialog_flow(session.flow_control.clone().into());
                w.set_dialog_disable_shell_integration(session.disable_shell_integration);
                w.set_dialog_note(session.note.clone().into());
                w.set_dialog_editing(true);
                w.set_dialog_open(true);
            }
        });
    }

    // Remove session.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_remove_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove(&id.to_string());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                // Touch a property so the list re-renders reliably.
                let _ = w.get_sessions();
            }
        });
    }

    // Duplicate a session: clone it with a fresh id and a " (copy)" name (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_duplicate_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(&id.to_string()).cloned() {
                    let mut copy = orig;
                    copy.id = uuid::Uuid::new_v4().to_string();
                    copy.name = format!("{} (copy)", copy.name);
                    copy.last_used = None;
                    s.upsert(copy);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Move a session to another group (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_move_session(move |id: SharedString, group: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(&id.to_string()).cloned() {
                    let mut moved = orig;
                    // "default" is the display label for ungrouped → store empty.
                    moved.group = if group.as_str() == "default" {
                        String::new()
                    } else {
                        group.to_string()
                    };
                    s.upsert(moved);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Collapse / expand a group in the welcome list (#41). Toggling flips the
    // `collapsed` flag on every row of that group in place — no full re-sync —
    // so the open/closed state stays put until the list is actually rebuilt.
    {
        let weak = window.as_weak();
        let sessions_model = sessions_model.clone();
        window.on_toggle_group(move |group: SharedString| {
            use slint::Model as _;
            let target = group.to_string();
            let n = sessions_model.row_count();
            // New state = the opposite of the group's first row.
            let mut new_state = false;
            for i in 0..n {
                if let Some(row) = sessions_model.row_data(i) {
                    if row.group.as_str() == target {
                        new_state = !row.collapsed;
                        break;
                    }
                }
            }
            for i in 0..n {
                if let Some(mut row) = sessions_model.row_data(i) {
                    if row.group.as_str() == target {
                        row.collapsed = new_state;
                        sessions_model.set_row_data(i, row);
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Group create / rename (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_submit_group(move |orig: SharedString, name: SharedString| {
            {
                let mut s = store.borrow_mut();
                if orig.is_empty() {
                    s.add_group(name.to_string());
                } else {
                    s.rename_group(&orig.to_string(), name.to_string());
                }
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }
    // Group delete (#41) — UI only offers this on empty groups.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_delete_group(move |name: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove_group(&name.to_string());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Dialog submit -> persist + (optionally) connect.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let edit_forwards = edit_forwards.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // The edit dialog never echoes the real password (issue #10): a blank
            // field while editing means "keep the existing password" rather than
            // "clear it".  Only overwrite when the user actually typed something.
            let password = if draft.password.is_empty() {
                store
                    .borrow()
                    .get(&id)
                    .map(|s| s.password.clone())
                    .unwrap_or_default()
            } else {
                Secret::new(draft.password.to_string())
            };
            let private_key_inline = if draft.private_key_inline_mode {
                if draft.private_key_inline.is_empty() {
                    store
                        .borrow()
                        .get(&id)
                        .map(|s| s.private_key_inline.clone())
                        .unwrap_or_default()
                } else {
                    Secret::new(draft.private_key_inline.to_string())
                }
            } else {
                Secret::default()
            };
            let private_key_path = if draft.private_key_inline_mode {
                String::new()
            } else {
                draft.private_key_path.to_string().replace('\\', "/")
            };
            let kind = crate::config::SessionKind::from_str(&draft.kind.to_string());
            // Auto-name: serial → port label; otherwise user@host, or just the
            // host when no username was given (#110).
            let auto_name = match kind {
                crate::config::SessionKind::Serial => {
                    format!("{} @{}", draft.serial_port, draft.baud_rate)
                }
                _ if draft.user.trim().is_empty() => draft.host.to_string(),
                _ => format!("{}@{}", draft.user, draft.host),
            };
            // Telnet defaults to port 23, SSH to 22; serial ignores port.
            let default_port = if kind == crate::config::SessionKind::Telnet {
                23
            } else {
                22
            };
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    auto_name
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 {
                    default_port
                } else {
                    draft.port as u16
                },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(&draft.auth.to_string()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path,
                private_key_inline,
                proxy: draft.proxy.to_string(),
                last_used: None,
                group: draft.group.to_string(),
                kind,
                serial_port: draft.serial_port.to_string(),
                baud_rate: if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                },
                data_bits: draft.data_bits as u8,
                stop_bits: draft.stop_bits as u8,
                parity: draft.parity.to_string(),
                flow_control: draft.flow_control.to_string(),
                forwards,
                disable_shell_integration: draft.disable_shell_integration,
                note: draft.note.to_string(),
                jump_session_id: draft.jump_session_id.to_string(),
            };
            {
                let mut s = store.borrow_mut();
                s.upsert(new_session);
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Test connection from the session dialog. SSH tests use the same handshake,
    // host-key verification, proxy/jump routing, and authentication as a real
    // terminal connection (#276). Telnet and serial retain reachability tests.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        window.on_session_dialog_test(move |draft: SessionDraft| {
            let kind = draft.kind.to_string();
            if kind == "serial" {
                let port_name = draft.serial_port.to_string();
                let baud = if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                };
                let weak_done = weak.clone();
                runtime.spawn(async move {
                    let message = match tokio::task::spawn_blocking(move || {
                        serialport::new(&port_name, baud)
                            .timeout(std::time::Duration::from_millis(800))
                            .open()
                    })
                    .await
                    {
                        Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                        Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                        Err(e) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            let session = session_from_draft(&draft, existing.as_ref(), forwards);
            let weak_done = weak.clone();

            if kind == "ssh" {
                let jump = resolve_jump(&store, &session);
                let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.spawn(async move {
                    let mut test = Box::pin(test_session_auth(session, jump, events_tx));
                    let result = loop {
                        tokio::select! {
                            result = &mut test => break result,
                            event = events_rx.recv() => {
                                let Some(event) = event else { continue };
                                if matches!(
                                    event,
                                    SessionEvent::HostKeyPrompt { .. }
                                        | SessionEvent::CredentialPrompt { .. }
                                        | SessionEvent::MfaPrompt { .. }
                                ) {
                                    let weak_prompt = weak_done.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(w) = weak_prompt.upgrade() else { return };
                                        match event {
                                            SessionEvent::HostKeyPrompt {
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            } => enqueue_hostkey_prompt(
                                                &w,
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            ),
                                            SessionEvent::CredentialPrompt {
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            } => enqueue_cred_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            ),
                                            SessionEvent::MfaPrompt {
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            } => enqueue_mfa_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            ),
                                            _ => {}
                                        }
                                    });
                                }
                            }
                        }
                    };
                    let message = match result {
                        Ok(()) => t("连接正常", "Connection OK").to_string(),
                        Err(e) => format!("{}: {e:#}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let host = session.host;
            let port = session.port;
            runtime.spawn(async move {
                let target = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await;
                let message = match result {
                    Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                    Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    Err(_) => format!("{}: {target}", t("连接超时", "Connection timed out")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                });
            });
        });
    }

    // Cancel dialog.
    {
        let weak = window.as_weak();
        window.on_session_dialog_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Private-key file picker: pick the private key and store its path with
    // forward-slash separators (uniform across Windows/Linux; russh accepts them).
    {
        let weak = window.as_weak();
        window.on_session_dialog_pick_key(move || {
            let mut dialog =
                rfd::FileDialog::new()
                    .set_title(t("选择私钥文件", "Choose private key file"))
                    .add_filter(
                        t("SSH 私钥", "SSH private keys"),
                        &["ppk", "pem", "key"],
                    );
            // Start in ~/.ssh if it exists.
            if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh")) {
                if home.is_dir() {
                    dialog = dialog.set_directory(home);
                }
            }
            if let Some(file) = dialog.pick_file() {
                let path = file.to_string_lossy().replace('\\', "/");
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_path(path.into());
                }
            }
        });
    }

    // Add another editable port-forward row (#56, #277).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_add_forward(move || {
            ef.borrow_mut().push(blank_forward_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }
    // Keep each editable row in the Rust-side working set. Saving validates and
    // converts all non-empty rows together, so no separate "added" state exists.
    {
        let ef = edit_forwards.clone();
        window.on_update_forward(move |index: i32, forward: PortFwd| {
            let i = index as usize;
            let mut forwards = ef.borrow_mut();
            if i < forwards.len() {
                forwards[i] = forward;
            }
        });
    }
    // Delete a port forward by index (#56).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_delete_forward(move |index: i32| {
            let i = index as usize;
            {
                let mut v = ef.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_forward_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }

    // Connect session -> open a new terminal tab.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let runtime = runtime.clone();
        let last_term_size = last_term_size.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let tab_statuses = tab_statuses.clone();
        let local_snap = local_snap.clone();
        let local_net_hist = local_net_hist.clone();
        let sftp_follow_cd = sftp_follow_cd.clone();
        window.on_connect_session(move |id: SharedString| {
            let id = id.to_string();
            let session = if id.starts_with("system:") {
                match builtin_local_sessions().into_iter().find(|s| s.id == id) {
                    Some(s) => s,
                    None => return,
                }
            } else {
                match store.borrow().get(&id).cloned() {
                    Some(s) => s,
                    None => return,
                }
            };
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            let tab_title = session.name.clone();

            // Connection label shown in the sidebar / status line, per transport.
            let conn_label = match session.kind {
                SessionKind::Ssh => format!("{}@{}", session.user, session.host),
                SessionKind::Serial => {
                    format!("{} @{}", session.serial_port, session.baud_rate)
                }
                SessionKind::Telnet => format!("telnet {}:{}", session.host, session.port),
                SessionKind::Local => format!("local {}", session.name),
            };
            // Serial / Telnet have no SFTP side-channel.
            let has_sftp = session.kind == SessionKind::Ssh;

            // Seed the per-tab status so the sidebar shows "连接中 host" the
            // moment this tab becomes active (the `changed active-tab-id`
            // handler fires refresh-sidebar right after set_active_tab_id below).
            tab_statuses.lock().unwrap().insert(
                tab_id.clone(),
                TabStatus {
                    host: conn_label.clone(),
                    user: session.user.clone(),
                    session_id: id.clone(),
                    state: 0,
                    ..Default::default()
                },
            );

            // Register tab + terminal state (SFTP fields start empty/loading).
            tabs_model.push(TabInfo {
                id: tab_id.clone().into(),
                title_len: tab_title_len(&tab_title),
                title: tab_title.into(),
                kind: "terminal".into(),
                connected: false,
            });
            // Each session keeps its own SFTP collapse state + sizes, seeded from
            // the global defaults (the "collapse SFTP by default" pref and the
            // persisted panel sizes) so they no longer bleed across panes (#v0.5).
            let (sftp_collapsed_default, sftp_h_default, sftp_w_default) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_collapse_sftp_default(),
                        w.get_sftp_panel_height(),
                        w.get_sftp_panel_width(),
                    )
                })
                .unwrap_or((false, 220.0, 380.0));
            terminals_model.push(TerminalState {
                id: tab_id.clone().into(),
                status: t("连接中...", "Connecting...").into(),
                spans: ModelRc::from(std::rc::Rc::new(VecModel::<TermSpan>::default())),
                cursor_row: 0,
                cursor_col: 0,
                rows_used: 0,
                scroll_max: 0,
                scroll_offset: 0,
                is_alt_screen: false,
                find_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                selection: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                sftp_path: "/".into(),
                sftp_entries: ModelRc::from(std::rc::Rc::new(VecModel::<SftpEntry>::default())),
                sftp_status: if has_sftp {
                    t("SFTP 连接中...", "SFTP connecting...").into()
                } else {
                    t(
                        "此会话类型不支持 SFTP",
                        "SFTP not available for this session",
                    )
                    .into()
                },
                sftp_loading: has_sftp,
                sftp_tree_nodes: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpTreeNode>::default(),
                )),
                sftp_selected_count: 0,
                sftp_sort_key: "".into(),
                sftp_sort_dir: 0,
                sftp_available: has_sftp,
                tunnels: ModelRc::from(std::rc::Rc::new(VecModel::<TunnelInfo>::default())),
                sftp_collapsed: !has_sftp || sftp_collapsed_default,
                sftp_panel_height: sftp_h_default,
                sftp_panel_width: sftp_w_default,
                sftp_saved_height: sftp_h_default,
            });
            // Create vt100 parser for this tab (default 24×80; resized on first
            // terminal-resize callback). 5000-line scrollback is stored for
            // future scroll-navigation support.
            let is_dark_now = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            let (output_highlight, custom_highlight_rules) = {
                let settings = store.borrow();
                (
                    OutputHighlightPreset::from_settings(
                        settings.output_highlight_enabled(),
                        settings.output_highlight_preset(),
                    ),
                    compile_output_rules(settings.output_highlight_rules()),
                )
            };
            bufs.lock().unwrap().insert(
                tab_id.clone(),
                Arc::new(Mutex::new(TermBuffer {
                    parser: vt100::Parser::new(24, 80, 5000),
                    find_query: String::new(),
                    is_dark: is_dark_now,
                    output_highlight,
                    custom_highlight_rules,
                    sel_anchor: None,
                    sel_focus: None,
                    sel_ranges: Vec::new(),
                    history: VecDeque::new(),
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                    raw: std::collections::VecDeque::new(),
                })),
            );
            render_gates
                .lock()
                .unwrap()
                .insert(
                    tab_id.clone(),
                    Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)),
                );
            // No followed-cwd yet: the first OSC 7 always triggers a follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            // Add the new tab to the focused pane and re-flatten (this also sets
            // active-tab-id to the new tab via refresh_panes).
            layout.borrow_mut().add_tab(tab_id.clone());
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }

            // Spawn the shell (+ SFTP) workers and their event-pump threads.
            // Shared with in-place reconnect (#79) via start_session_in_tab.
            let connect_ctx = ConnectCtx {
                weak: weak.clone(),
                runtime: runtime.clone(),
                handles: handles.clone(),
                sftp_handles: sftp_handles.clone(),
                sftp_last_cwd: sftp_last_cwd.clone(),
                bufs: bufs.clone(),
                render_gates: render_gates.clone(),
                tab_statuses: tab_statuses.clone(),
                local_snap: local_snap.clone(),
                local_net_hist: local_net_hist.clone(),
                last_term_size: last_term_size.clone(),
                sftp_follow_cd: sftp_follow_cd.clone(),
                store: store.clone(),
            };
            start_session_in_tab(&tab_id, session, &connect_ctx);
        });
    }

    // Duplicate a tab's connection (#v0.5): open a fresh tab to the same saved
    // session, landing in the same pane as the source tab.
    {
        let weak = window.as_weak();
        let tab_statuses = tab_statuses.clone();
        let layout = layout.clone();
        window.on_tab_duplicate(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            let session_id = tab_statuses
                .lock()
                .unwrap()
                .get(&tab_id)
                .map(|s| s.session_id.clone())
                .unwrap_or_default();
            if session_id.is_empty() {
                return;
            }
            // Land the new tab in the same pane as the source. Read the pane id
            // into a local first so the immutable borrow is dropped before the
            // borrow_mut (else RefCell panics on the overlapping borrow).
            let pane = layout.borrow().leaf_of_tab(&tab_id);
            if let Some(pane) = pane {
                layout.borrow_mut().focused = pane;
            }
            if let Some(w) = weak.upgrade() {
                w.invoke_connect_session(session_id.into());
            }
        });
    }
}

pub(crate) fn tuple5_rows(rows: &[(String, String, String, String, String)]) -> Vec<SysInfoRow> {
    rows.iter()
        .map(|r| SysInfoRow {
            c1: r.0.clone().into(),
            c2: r.1.clone().into(),
            c3: r.2.clone().into(),
            c4: r.3.clone().into(),
            c5: r.4.clone().into(),
        })
        .collect()
}

/// Cumulative grid columns for a rendered line. The plain text we keep stores
/// ONE char per glyph, but a wide (CJK) glyph occupies TWO grid cells, so a char
/// index is *not* a grid column. `prefix[i]` is the starting grid column of
/// char `i`; `prefix[chars.len()]` is the line's total cell width. Zero-width
/// chars (combining marks) share their base char's column (#132).
pub(crate) fn cell_prefix(chars: &[char]) -> Vec<usize> {
    use unicode_width::UnicodeWidthChar;
    let mut prefix = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0usize;
    for &ch in chars {
        prefix.push(acc);
        acc += ch.width().unwrap_or(0);
    }
    prefix.push(acc);
    prefix
}

/// First char index whose cell span contains grid column `target` — i.e. the
/// char a selection STARTING at that column should begin on. Clamps to the end
/// of the line when `target` is past the content (#132).
pub(crate) fn char_at_cell_start(prefix: &[usize], target: usize) -> usize {
    let n = prefix.len().saturating_sub(1); // chars.len()
    for i in 0..n {
        if prefix[i] <= target && target < prefix[i + 1] {
            return i;
        }
    }
    n
}

/// Exclusive char index just past grid column `target` — i.e. the slice end for
/// a selection ENDING (inclusive) at that column. Trailing zero-width marks on
/// the last glyph are kept because their start column is not strictly greater
/// than `target` (#132).
pub(crate) fn char_after_cell_end(prefix: &[usize], target: usize) -> usize {
    let n = prefix.len().saturating_sub(1); // chars.len()
    for i in 0..n {
        if prefix[i] > target {
            return i;
        }
    }
    n
}

/// Find every (case-insensitive) occurrence of `query` across the currently
/// displayed rows and return highlight rectangles in GRID-COLUMN space (wide
/// CJK glyphs count as two columns, so highlights line up over the text #132).
pub(crate) fn compute_find_matches(rows: &[String], query: &str) -> Vec<TermMatch> {
    let mut out: Vec<TermMatch> = Vec::new();
    if query.is_empty() {
        return out;
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return out;
    }
    for (r, line) in rows.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
        let prefix = cell_prefix(&chars);
        let mut i = 0usize;
        while i + q.len() <= lower.len() {
            if lower[i..i + q.len()] == q[..] {
                let col = prefix[i] as i32;
                let len = (prefix[i + q.len()] - prefix[i]) as i32;
                out.push(TermMatch {
                    row: r as i32,
                    col,
                    len,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Apply a session event to the live UI models. Must be called on the Slint
/// event loop thread.
fn apply_session_event_to_window(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    bufs: &TermBuffers,
    gates: &RenderGates,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
) {
    let tabs_rc = win.get_tabs();
    let terminals_rc = win.get_terminals();
    // `ModelRc::as_any` lets us downcast to the concrete `VecModel<T>`.
    let tabs = tabs_rc
        .as_any()
        .downcast_ref::<VecModel<TabInfo>>()
        .expect("tabs model must be a VecModel");
    let terminals = terminals_rc
        .as_any()
        .downcast_ref::<VecModel<TerminalState>>()
        .expect("terminals model must be a VecModel");

    let update_terminal = |mutator: &dyn Fn(&mut TerminalState)| {
        for i in 0..terminals.row_count() {
            if let Some(mut row) = terminals.row_data(i) {
                if row.id.as_str() == tab_id {
                    mutator(&mut row);
                    terminals.set_row_data(i, row);
                    break;
                }
            }
        }
    };
    let update_tab = |mutator: &dyn Fn(&mut TabInfo)| {
        for i in 0..tabs.row_count() {
            if let Some(mut row) = tabs.row_data(i) {
                if row.id.as_str() == tab_id {
                    mutator(&mut row);
                    tabs.set_row_data(i, row);
                    break;
                }
            }
        }
        // The per-pane tab strips (v0.5 split panes) render snapshots copied from
        // `tabs_model`, so they don't track this change on their own — propagate
        // it into each pane's tab sub-model too (e.g. so the connected dot turns
        // green without needing a tab switch).
        let panes = win.get_panes();
        if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
            for pi in 0..pm.row_count() {
                let Some(pane) = pm.row_data(pi) else {
                    continue;
                };
                let Some(tm) = pane.tabs.as_any().downcast_ref::<VecModel<TabInfo>>() else {
                    continue;
                };
                for ti in 0..tm.row_count() {
                    if let Some(mut row) = tm.row_data(ti) {
                        if row.id.as_str() == tab_id {
                            mutator(&mut row);
                            tm.set_row_data(ti, row);
                            break;
                        }
                    }
                }
            }
        }
    };

    match event {
        SessionEvent::Status(status) => {
            update_terminal(&|t| t.status = status.clone().into());
        }
        SessionEvent::Output(chunk) => {
            // Synthetic Output (disconnect hint, editor error, …) — rare, already
            // on the UI thread. Live shell output is ingested on the pump thread.
            ingest_terminal_output(bufs, tab_id, chunk.as_bytes());
            run_coalesced_tab_render(&win.as_weak(), tab_id, bufs, gates);
        }
        SessionEvent::Connected => {
            update_tab(&|t| t.connected = true);
            update_terminal(&|t| t.status = crate::i18n::t("已连接", "Connected").into());
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.state = 1;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::Closed(reason) => {
            // Print the hint into the terminal itself (FinalShell-style), via a
            // synthetic Output event so it reuses the normal render path (#79).
            apply_session_event_to_window(
                win,
                tab_id,
                SessionEvent::Output(format!(
                    "\r\n\x1b[31m{}\x1b[0m\r\n",
                    crate::i18n::t(
                        "连接已断开,按 Enter 重新连接",
                        "Disconnected — press Enter to reconnect"
                    )
                )),
                bufs,
                gates,
                statuses,
                local,
                local_net_hist,
            );
            update_tab(&|t| t.connected = false);
            update_terminal(&|t| {
                t.status = format!("{} — {reason}", crate::i18n::t("已断开", "Disconnected")).into()
            });
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.state = 2;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::ResourceStats {
            cpu_percent,
            mem_used_kib,
            mem_total_kib,
            swap_used_kib,
            swap_total_kib,
            net,
            disks,
            current_user: _,
            procs: _,
            sys,
        } => {
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.cpu = cpu_percent;
                st.mem_used_kib = mem_used_kib;
                st.mem_total_kib = mem_total_kib;
                st.swap_used_kib = swap_used_kib;
                st.swap_total_kib = swap_total_kib;
                st.net = net;
                st.disks = disks;
                if let Some(sys) = sys {
                    st.sys = sys;
                }
                // A sample means the channel is alive → treat as connected.
                if st.state != 1 {
                    st.state = 1;
                }
                // Append the selected interface's total rate to its sparkline.
                let (_, rx, tx) = selected_iface(st);
                push_ring(&mut st.net_hist, (rx + tx) as f32);
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::ProcessStats {
            current_user,
            procs,
        } => {
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                if !current_user.is_empty() {
                    st.user = current_user;
                }
                st.procs = procs;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::TunnelUpdate(rows) => {
            let items = rows
                .into_iter()
                .map(|r| TunnelInfo {
                    id: r.id.into(),
                    name: r.name.into(),
                    kind: r.kind.clone().into(),
                    bind: format!("{}:{}", r.bind_addr, r.bind_port).into(),
                    target: if r.kind == "dynamic" {
                        "SOCKS5".into()
                    } else if r.host.is_empty() || r.host_port == 0 {
                        "".into()
                    } else {
                        format!("{}:{}", r.host, r.host_port).into()
                    },
                    status: r.status.into(),
                    active: r.active,
                })
                .collect::<Vec<_>>();
            update_terminal(&|t| {
                t.tunnels = ModelRc::from(std::rc::Rc::new(VecModel::from(items.clone())));
            });
        }

        // --- SFTP events ---------------------------------------------------
        SessionEvent::CwdChanged(path) => {
            // Just update the displayed path; the pump thread already sent
            // SftpCommand::ListDir so a SftpEntries event is inbound.
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_loading = true;
            });
        }
        SessionEvent::SftpEntries { path, entries } => {
            let mut slint_entries: Vec<SftpEntry> = entries
                .iter()
                .map(|e| SftpEntry {
                    name: e.name.clone().into(),
                    full_path: e.full_path.clone().into(),
                    is_dir: e.is_dir,
                    size: if e.is_dir {
                        "".into()
                    } else {
                        format_size(e.size).into()
                    },
                    size_bytes: e.size as f32,
                    modified: format_mtime(e.modified).into(),
                    modified_ts: e.modified as f32,
                    mode: (e.mode & 0o7777) as i32,
                    selected: false,
                })
                .collect();
            let (sort_key, sort_dir) = (0..terminals.row_count())
                .find_map(|i| {
                    let row = terminals.row_data(i)?;
                    (row.id.as_str() == tab_id)
                        .then(|| (row.sftp_sort_key.to_string(), row.sftp_sort_dir))
                })
                .unwrap_or_default();
            sort_sftp_entries(&mut slint_entries, &sort_key, sort_dir);
            let model = ModelRc::from(std::rc::Rc::new(VecModel::from(slint_entries)));
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_entries = model.clone();
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpStatus(msg) => {
            update_terminal(&|t| t.sftp_status = msg.clone().into());
        }
        SessionEvent::SftpError(msg) => {
            // Show the reason and stop the spinner; leave the current listing in
            // place so a failed navigation doesn't blank the panel (#112).
            update_terminal(&|t| {
                t.sftp_status = msg.clone().into();
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpFileText {
            path,
            name,
            content,
            edit,
            error,
        } => {
            if error.is_empty() {
                // Open the built-in viewer/editor (#70).
                win.set_editor_line_numbers(line_numbers_for(&content).into());
                win.set_editor_path(path.into());
                win.set_editor_name(name.into());
                win.set_editor_content(content.into());
                win.set_editor_readonly(!edit);
                win.set_editor_dirty(false);
                win.set_editor_open(true);
            } else {
                // Couldn't open as text. The SFTP status line alone is easy to
                // miss (looks like "nothing happened"), so also print the reason
                // into the terminal via a synthetic Output event (#70).
                apply_session_event_to_window(
                    win,
                    tab_id,
                    SessionEvent::Output(format!(
                        "\r\n[meatshell] {} {}: {}\r\n",
                        crate::i18n::t("无法打开", "Cannot open"),
                        name,
                        error
                    )),
                    bufs,
                    gates,
                    statuses,
                    local,
                    local_net_hist,
                );
                update_terminal(&|t| t.sftp_status = error.clone().into());
            }
        }
        SessionEvent::SftpTreeUpdate(nodes) => {
            let slint_nodes: Vec<SftpTreeNode> = nodes
                .iter()
                .map(|n| SftpTreeNode {
                    path: n.path.clone().into(),
                    name: n.name.clone().into(),
                    depth: n.depth as i32,
                    expanded: n.expanded,
                    has_children: n.has_children,
                })
                .collect();
            let model = ModelRc::from(std::rc::Rc::new(VecModel::from(slint_nodes)));
            update_terminal(&|t| t.sftp_tree_nodes = model.clone());
        }
        SessionEvent::SftpTransfer {
            id,
            name,
            is_upload,
            transferred,
            total,
            state,
            msg,
        } => {
            let detail = match state {
                // On error, show the actual message when we have one.
                2 => {
                    if msg.is_empty() {
                        t("失败", "Failed").to_string()
                    } else {
                        msg
                    }
                }
                1 => t("已完成", "Done").to_string(),
                // Remote-side prep (e.g. tar packing) before bytes start flowing (#100).
                3 => t("文件准备中", "Preparing...").to_string(),
                // User-cancelled transfer (#100).
                4 => t("已取消", "Cancelled").to_string(),
                _ => {
                    if total > 0 {
                        format!("{}/{}", format_size(transferred), format_size(total))
                    } else {
                        format_size(transferred)
                    }
                }
            };
            let percent = if state == 1 {
                1.0
            } else if total > 0 {
                (transferred as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let rec = TransferInfo {
                id: id.clone().into(),
                name: name.into(),
                detail: detail.into(),
                percent,
                state: state as i32,
                is_upload,
            };
            if let Some(model) = win
                .get_transfers()
                .as_any()
                .downcast_ref::<VecModel<TransferInfo>>()
            {
                let mut found = None;
                for i in 0..model.row_count() {
                    if let Some(row) = model.row_data(i) {
                        if row.id.as_str() == id.as_str() {
                            found = Some(i);
                            break;
                        }
                    }
                }
                match found {
                    Some(i) => model.set_row_data(i, rec),
                    None => model.insert(0, rec), // newest at top
                }
            }
        }
        SessionEvent::HostKeyPrompt {
            host,
            port,
            key_type,
            fingerprint,
            changed,
            responder,
        } => {
            enqueue_hostkey_prompt(win, host, port, key_type, fingerprint, changed, responder);
        }
        SessionEvent::CredentialPrompt {
            session_id,
            host,
            user,
            need_user,
            need_password,
            responder,
        } => {
            enqueue_cred_prompt(
                win,
                session_id,
                host,
                user,
                need_user,
                need_password,
                responder,
            );
        }
        SessionEvent::MfaPrompt {
            session_id,
            host,
            prompt,
            echo,
            responder,
        } => {
            enqueue_mfa_prompt(win, session_id, host, prompt, echo, responder);
        }
        SessionEvent::CommandRan(cmd) => {
            // A command typed directly in the terminal, captured via the shell
            // hook (#113). Record it in the same command-box history, reusing the
            // de-dup/move-to-end logic, and refresh the model.
            HISTORY_STORE.with(|s| {
                if let Some(store) = s.borrow().as_ref() {
                    {
                        let mut st = store.borrow_mut();
                        st.push_command_history(cmd);
                        let _ = st.save();
                    }
                    win.set_command_history(history_model(&store.borrow()));
                }
            });
        }
    }
}

thread_local! {
    /// The config store, made reachable from the Slint-thread event handler so
    /// terminal-captured commands (#113) can be appended to history. Set once at
    /// startup; only touched on the Slint event-loop thread.
    pub(crate) static HISTORY_STORE: RefCell<Option<Rc<RefCell<ConfigStore>>>> = const { RefCell::new(None) };
}
