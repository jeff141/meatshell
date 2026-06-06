//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Per-terminal state: vt100 parser drives all rendering for both normal
/// (bash) and alt-screen (vim/nano/htop) modes.
///
/// Using vt100 for normal mode too is necessary because readline rewrites the
/// current input line using `\r` + full-line redraw + `\x1b[K` (erase to EOL)
/// whenever the cursor moves. A naive append-only buffer would duplicate the
/// text; vt100 tracks cursor position and overwrites in place correctly.
struct TermBuffer {
    parser: vt100::Parser,
    /// Active find query for this tab ("" = no search).
    find_query: String,
    /// Drag selection (start_row, start_col, end_row, end_col) in grid cells.
    sel: Option<(u16, u16, u16, u16)>,
    /// Session scrollback: lines that have scrolled off the top (oldest first).
    history: Vec<Line>,
    /// Previous frame's grid lines, for scroll-off detection.
    prev: Vec<Line>,
    /// Scrollback view offset in lines (0 = live bottom).
    view_offset: usize,
    /// Plain text of the rows currently displayed (drives find + selection).
    displayed_text: Vec<String>,
    /// CSI-scanner state for rewriting HVP (`ESC [ … f`) into CUP (`ESC [ … H`).
    /// vt100 0.15 only implements the `H` final byte, not the equivalent `f`
    /// that btop/htop use for cursor positioning — without this rewrite their
    /// absolute-positioned full-screen output collapses into a scrolling mess.
    /// Kept here so a sequence split across read chunks is still translated.
    csi_state: CsiState,
}

/// Minimal CSI-final-byte rewriter state (persists across read chunks).
#[derive(Clone, Copy, PartialEq)]
enum CsiState {
    /// Normal text.
    Normal,
    /// Saw ESC (0x1b), waiting to see if it starts a CSI (`[`).
    Esc,
    /// Inside a CSI sequence (after `ESC [`), scanning params/intermediates.
    Csi,
}

type TermBuffers = Arc<Mutex<HashMap<String, TermBuffer>>>;

use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;

use crate::config::{AuthMethod, ConfigStore, Secret, Session};
use crate::i18n::t;
use crate::sftp::{spawn_sftp, SftpHandle};
use crate::ssh::{
    format_mtime, format_size, spawn_session, SessionCommand, SessionEvent, SessionHandle,
};
use crate::system::{format_bytes_per_sec, format_mem_mib, SystemSampler, SystemSnapshot};

type SftpHandles = Arc<Mutex<HashMap<String, SftpHandle>>>;
/// Per-tab flag: once the user explicitly navigates via the SFTP tree or
/// toolbar, stop auto-syncing to the terminal's `cd` path.
type SftpManualNav = Arc<Mutex<HashMap<String, bool>>>;

/// Per-tab connection status + latest remote resource sample, used to drive the
/// sidebar for whichever tab is active.  `Arc<Mutex>` because the SSH event-pump
/// threads update it before bouncing to the UI thread.
#[derive(Clone, Default)]
struct TabStatus {
    host: String, // "root@192.168.100.2"
    state: u8,    // 0 = connecting, 1 = connected, 2 = disconnected
    cpu: f32,     // 0.0..1.0
    mem_used_kib: u64,
    mem_total_kib: u64,
    swap_used_kib: u64,
    swap_total_kib: u64,
    /// Latest per-interface rates: (name, rx_bps, tx_bps), busiest first.
    net: Vec<(String, u64, u64)>,
    /// Which interface drives the top sparkline (empty = auto = busiest).
    selected_iface: String,
    /// Ring buffer of the selected interface's total (rx+tx) bytes/sec.
    net_hist: Vec<f32>,
    /// Per-filesystem (mount, available_bytes, total_bytes).
    disks: Vec<(String, u64, u64)>,
}
type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
/// Last local-machine sample (shown on the welcome tab).
type LocalSnap = Arc<Mutex<SystemSnapshot>>;

// Slint generates types into this scope.
slint::include_modules!();

/// Number of samples kept for the sparkline.
const NET_HISTORY_LEN: usize = 60;

pub fn run() -> Result<()> {
    // --- Runtime + store -------------------------------------------------
    let runtime = Arc::new(
        Runtime::new().context("failed to start tokio runtime")?,
    );
    let store = Rc::new(RefCell::new(
        ConfigStore::load().context("failed to load config")?,
    ));

    // Per-tab SSH handles (shell only; lives on Slint thread via Rc).
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Per-tab SFTP handles — Arc<Mutex> so the event-pump OS thread and the
    // Slint UI thread can both post SftpCommands.
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    // Once the user navigates manually in the SFTP panel, stop auto-following cd.
    let sftp_manual_nav: SftpManualNav = Arc::new(Mutex::new(HashMap::new()));

    // Per-tab vt100 parsers + history logs (Arc<Mutex> so they can be cloned
    // into the thread that pumps session events into invoke_from_event_loop).
    let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));

    // Last-known terminal pixel dimensions, updated by every terminal-resize
    // callback.  Shared so on_connect_session can pass a sensible initial PTY
    // size to spawn_session before the first resize callback fires.
    // Default: 80 cols × 24 rows (SSH spec minimum).
    let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));

    // --- Build window + models ------------------------------------------
    // Set the Wayland app_id / X11 WM_CLASS *before* the window is created so
    // the Linux desktop shell can match the running window to the installed
    // `meatshell.desktop` entry and show our icon in the dock/taskbar.  (On
    // Windows the icon comes from the embedded .ico, so this is a no-op there.)
    let _ = slint::set_xdg_app_id("meatshell");
    let window = AppWindow::new().context("failed to build Slint window")?;

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions_model.clone()));
    sync_sessions_to_model(&store.borrow(), &sessions_model);

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title: t("新标签页", "New tab").into(),
        kind: "welcome".into(),
        connected: false,
    });
    window.set_tabs(ModelRc::from(tabs_model.clone()));
    window.set_active_tab_id("welcome".into());

    let terminals_model: Rc<VecModel<TerminalState>> = Rc::new(VecModel::default());
    window.set_terminals(ModelRc::from(terminals_model.clone()));

    // Per-tab connection status + remote resources, the latest local sample,
    // and the local machine's network history (bottom sparkline).
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));

    // --- Wire callbacks --------------------------------------------------
    wire_session_callbacks(
        &window,
        store.clone(),
        sessions_model.clone(),
        tabs_model.clone(),
        terminals_model.clone(),
        handles.clone(),
        bufs.clone(),
        runtime.clone(),
        last_term_size.clone(),
        sftp_handles.clone(),
        sftp_manual_nav.clone(),
        tab_statuses.clone(),
        local_snap.clone(),
        local_net_hist.clone(),
    );

    // Recompute the sidebar whenever the active tab changes (fired from Slint's
    // `changed active-tab-id`).
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        window.on_refresh_sidebar(move || {
            if let Some(w) = weak.upgrade() {
                refresh_sidebar(&w, &statuses, &local, &net);
            }
        });
    }

    // Switch UI language at runtime.  Static `@tr(...)` text updates live via
    // select_bundled_translation; we additionally refresh the Rust-driven
    // dynamic strings (sidebar status + the welcome tab title).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        window.on_set_language(move |code| {
            crate::i18n::set_language(&code.to_string());
            {
                let mut s = store.borrow_mut();
                s.set_language(crate::i18n::current_code().to_string());
                let _ = s.save();
            }
            // Re-translate the welcome tab's dynamic title.
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i) {
                    if row.id.as_str() == "welcome" {
                        row.title = t("新标签页", "New tab").into();
                        tabs_model.set_row_data(i, row);
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_lang_en(crate::i18n::is_en());
                w.invoke_refresh_sidebar();
            }
        });
    }

    // NIC selector: remember the user's choice for the active tab and refresh.
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        window.on_select_net_iface(move |iface: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let active = w.get_active_tab_id().to_string();
            if let Some(st) = statuses.lock().unwrap().get_mut(&active) {
                st.selected_iface = iface.to_string();
                st.net_hist = vec![0.0; NET_HISTORY_LEN]; // reset graph for new NIC
            }
            refresh_sidebar(&w, &statuses, &local, &net);
        });
    }

    // Settings: preset download directory (load + pick + open).
    window.set_download_dir(store.borrow().download_dir().to_string().into());
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_pick_download_dir(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                let dir = folder.to_string_lossy().to_string();
                {
                    let mut s = store.borrow_mut();
                    s.set_download_dir(dir.clone());
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_download_dir(dir.into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_download_dir(move || {
            let Some(w) = weak.upgrade() else { return };
            let dir = w.get_download_dir().to_string();
            if dir.is_empty() {
                return;
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            #[cfg(not(windows))]
            {
                let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
            }
        });
    }

    // Transfer records (download/upload progress + history) shown in the popup.
    let transfers_model: Rc<VecModel<TransferInfo>> = Rc::new(VecModel::default());
    window.set_transfers(ModelRc::from(transfers_model.clone()));
    {
        let tm = transfers_model.clone();
        window.on_clear_transfers(move || tm.set_vec(Vec::<TransferInfo>::new()));
    }

    // Open-source libraries shown in the About popup.
    {
        let libs: Vec<SharedString> = [
            t("Slint — 图形界面框架 (GUI)", "Slint — GUI framework"),
            t("russh / russh-keys — SSH 协议实现", "russh / russh-keys — SSH protocol"),
            t("russh-sftp — SFTP 文件传输", "russh-sftp — SFTP file transfer"),
            t("ssh-key — SSH 密钥解析", "ssh-key — SSH key parsing"),
            t("tokio — 异步运行时", "tokio — async runtime"),
            t("vt100 — 终端 (VT100/xterm) 解析", "vt100 — terminal (VT100/xterm) parser"),
            t("sysinfo — 本机资源采集", "sysinfo — local resource sampling"),
            t("serde / serde_json — 配置序列化", "serde / serde_json — config serialization"),
            t("arboard — 系统剪贴板", "arboard — system clipboard"),
            t("rfd — 原生文件对话框", "rfd — native file dialogs"),
            t("directories — 配置目录定位", "directories — config dir lookup"),
            t("chrono — 日期时间处理", "chrono — date/time handling"),
            t("uuid — 唯一标识符", "uuid — unique identifiers"),
            t("anyhow / thiserror — 错误处理", "anyhow / thiserror — error handling"),
            t("tracing / tracing-subscriber — 日志", "tracing / tracing-subscriber — logging"),
            t("futures / async-trait — 异步辅助", "futures / async-trait — async helpers"),
            t("rand — 随机数", "rand — randomness"),
            t("winresource — Windows 图标/资源嵌入", "winresource — Windows icon/resource embedding"),
        ]
        .iter()
        .map(|s| (*s).into())
        .collect();
        window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(libs))));
    }

    wire_tab_callbacks(
        &window,
        tabs_model.clone(),
        terminals_model.clone(),
        handles.clone(),
        bufs.clone(),
        sftp_handles.clone(),
        sftp_manual_nav.clone(),
    );
    wire_sftp_callbacks(&window, sftp_handles.clone(), sftp_manual_nav.clone());
    wire_key_input(&window, handles.clone(), bufs.clone(), last_term_size.clone());

    // --- System sampler (1 Hz) ------------------------------------------
    let sampler = Rc::new(Mutex::new(SystemSampler::new()));
    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_statuses = tab_statuses.clone();
    let tick_local = local_snap.clone();
    let tick_net = local_net_hist.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        SystemSampler::recommended_interval(),
        move || {
            let snap = {
                let mut s = tick_sampler.lock().expect("sampler poisoned");
                s.sample()
            };
            // Append the raw local throughput to the bottom-graph ring buffer
            // (normalisation happens at display time so the graph auto-scales).
            push_ring(
                &mut tick_net.lock().unwrap(),
                snap.net_bytes_per_sec as f32,
            );
            // Stash the local sample; the sidebar shows it on the welcome tab
            // and in the bottom network graph.
            *tick_local.lock().unwrap() = snap.clone();

            if let Some(w) = weak.upgrade() {
                // Everything (status, CPU/mem/swap, both graphs) follows the
                // active tab; refresh_sidebar reads the stores we just updated.
                refresh_sidebar(&w, &tick_statuses, &tick_local, &tick_net);
            }
        },
    );
    // Keep the timer alive for the entire event loop by parking it on a
    // leaked Box. Slint timers drop themselves on Drop, and we don't want
    // that here.
    Box::leak(Box::new(timer));

    // OS file drag-and-drop → upload to the active session's SFTP directory,
    // but only when the file is dropped over the file-list area.
    {
        use i_slint_backend_winit::winit::event::WindowEvent as WEvent;
        use i_slint_backend_winit::EventResult;
        let weak = window.as_weak();
        let sh = sftp_handles.clone();
        window.window().on_winit_window_event(move |_w, event| {
            if let WEvent::DroppedFile(path) = event {
                if let Some(win) = weak.upgrade() {
                    handle_file_drop(&win, &sh, path.to_string_lossy().to_string());
                }
            }
            EventResult::Propagate
        });
    }

    // Center the window on the primary monitor once it's shown (size is only
    // known after the first frame, so defer via a single-shot timer).
    {
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
            if let Some(w) = weak.upgrade() {
                center_window(&w);
            }
        });
    }

    window.run().context("event loop exited with error")?;
    Ok(())
}

/// Center the window on the primary monitor's work area (Windows).
#[cfg(windows)]
fn center_window(win: &AppWindow) {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn SystemParametersInfoW(action: u32, uiparam: u32, pvparam: *mut Rect, winini: u32) -> i32;
    }
    const SPI_GETWORKAREA: u32 = 0x0030;

    let size = win.window().size(); // physical pixels
    let mut wa = Rect { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa, 0) };
    if ok == 0 {
        return;
    }
    let area_w = (wa.right - wa.left).max(0) as u32;
    let area_h = (wa.bottom - wa.top).max(0) as u32;
    let x = wa.left + ((area_w.saturating_sub(size.width)) / 2) as i32;
    let y = wa.top + ((area_h.saturating_sub(size.height)) / 2) as i32;
    win.window()
        .set_position(slint::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
fn center_window(_win: &AppWindow) {}

/// The active terminal tab's current SFTP directory ("" if unknown).
fn active_sftp_path(win: &AppWindow, tab_id: &str) -> String {
    let model = win.get_terminals();
    if let Some(m) = model.as_any().downcast_ref::<VecModel<TerminalState>>() {
        for i in 0..m.row_count() {
            if let Some(row) = m.row_data(i) {
                if row.id.as_str() == tab_id {
                    return row.sftp_path.to_string();
                }
            }
        }
    }
    String::new()
}

/// Current mouse cursor position in physical screen pixels (Windows).
#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// Handle an OS file drop: if it landed over the SFTP file-list area of the
/// active session tab, upload the file to that tab's current remote directory.
#[cfg(windows)]
fn handle_file_drop(win: &AppWindow, sftp_handles: &SftpHandles, path: String) {
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let w = win.window();
    let scale = w.scale_factor().max(0.01);
    let size = w.size(); // physical
    let Some(inner) = w
        .with_winit_window(|ww| ww.inner_position().ok())
        .flatten()
    else {
        return;
    };
    let Some((cx, cy)) = cursor_pos() else {
        return;
    };
    // Drop point in logical client coordinates.
    let client_x = (cx - inner.x) as f32 / scale;
    let client_y = (cy - inner.y) as f32 / scale;
    let w_logical = size.width as f32 / scale;
    let h_logical = size.height as f32 / scale;
    let h_sftp = win.get_sftp_panel_height();

    // File-list box (logical): right of the sidebar(220)+tree(160)+sep(1),
    // below the SFTP toolbar(30)+header(20)+sep(1), above the status bar(18).
    let zone_left = 381.0_f32;
    let zone_top = h_logical - h_sftp + 51.0;
    let zone_bottom = h_logical - 18.0;
    if client_x < zone_left
        || client_x > w_logical
        || client_y < zone_top
        || client_y > zone_bottom
    {
        return; // dropped outside the file list — ignore
    }

    let dir = active_sftp_path(win, &active);
    if dir.is_empty() {
        return;
    }
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(&active) {
            h.upload(path, dir);
        }
    }
}

#[cfg(not(windows))]
fn handle_file_drop(_win: &AppWindow, _sftp_handles: &SftpHandles, _path: String) {}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

fn sync_sessions_to_model(store: &ConfigStore, model: &VecModel<SessionInfo>) {
    let rows: Vec<SessionInfo> = store
        .sessions()
        .iter()
        .map(|s| SessionInfo {
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
        })
        .collect();
    model.set_vec(rows);
}

// ---------------------------------------------------------------------------
// Session callbacks (welcome page + dialog)
// ---------------------------------------------------------------------------

fn wire_session_callbacks(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    sessions_model: Rc<VecModel<SessionInfo>>,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    runtime: Arc<Runtime>,
    last_term_size: Arc<Mutex<(u32, u32)>>,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
) {
    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            let empty = Session::new_empty();
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            w.set_dialog_user("root".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_proxy("".into());
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
            let hosts = crate::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into());
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s.sessions().iter().any(|x| {
                        x.name == h.alias || (x.host == h.hostname && x.user == h.user)
                    });
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() { "root".into() } else { h.user },
                        auth,
                        password: Secret::default(),
                        private_key_path: h.identity_file,
                        proxy: String::new(),
                        last_used: None,
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

    // Edit -> open dialog prefilled.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_edit_session(move |id: SharedString| {
            let id = id.to_string();
            let store = store.borrow();
            let Some(session) = store.get(&id) else { return; };
            if let Some(w) = weak.upgrade() {
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
                w.set_dialog_proxy(session.proxy.clone().into());
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

    // Dialog submit -> persist + (optionally) connect.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
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
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    format!("{}@{}", draft.user, draft.host)
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 { 22 } else { draft.port as u16 },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(&draft.auth.to_string()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path: draft.private_key_path.to_string().replace('\\', "/"),
                proxy: draft.proxy.to_string(),
                last_used: None,
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
            let mut dialog = rfd::FileDialog::new().set_title(t("选择私钥文件", "Choose private key file"));
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

    // Connect session -> open a new terminal tab.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let runtime = runtime.clone();
        let last_term_size = last_term_size.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_manual_nav = sftp_manual_nav.clone();
        let tab_statuses = tab_statuses.clone();
        let local_snap = local_snap.clone();
        let local_net_hist = local_net_hist.clone();
        window.on_connect_session(move |id: SharedString| {
            let id = id.to_string();
            let session = match store.borrow().get(&id).cloned() {
                Some(s) => s,
                None => return,
            };
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            let tab_title = session.name.clone();

            // Seed the per-tab status so the sidebar shows "连接中 host" the
            // moment this tab becomes active (the `changed active-tab-id`
            // handler fires refresh-sidebar right after set_active_tab_id below).
            tab_statuses.lock().unwrap().insert(
                tab_id.clone(),
                TabStatus {
                    host: format!("{}@{}", session.user, session.host),
                    state: 0,
                    ..Default::default()
                },
            );

            // Register tab + terminal state (SFTP fields start empty/loading).
            tabs_model.push(TabInfo {
                id: tab_id.clone().into(),
                title: tab_title.into(),
                kind: "terminal".into(),
                connected: false,
            });
            terminals_model.push(TerminalState {
                id: tab_id.clone().into(),
                status: t("连接中...", "Connecting...").into(),
                spans: ModelRc::from(std::rc::Rc::new(VecModel::<TermSpan>::default())),
                cursor_row: 0,
                cursor_col: 0,
                rows_used: 0,
                is_alt_screen: false,
                find_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                selection: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                sftp_path: "/".into(),
                sftp_entries: ModelRc::from(
                    std::rc::Rc::new(VecModel::<SftpEntry>::default()),
                ),
                sftp_status: t("SFTP 连接中...", "SFTP connecting...").into(),
                sftp_loading: true,
                sftp_tree_nodes: ModelRc::from(
                    std::rc::Rc::new(VecModel::<SftpTreeNode>::default()),
                ),
            });
            // Create vt100 parser for this tab (default 24×80; resized on first
            // terminal-resize callback). 5000-line scrollback is stored for
            // future scroll-navigation support.
            bufs.lock().unwrap().insert(
                tab_id.clone(),
                TermBuffer {
                    parser: vt100::Parser::new(24, 80, 5000),
                    find_query: String::new(),
                    sel: None,
                    history: Vec::new(),
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                },
            );
            // Start in cd-auto-follow mode (flag = false → follow cd).
            sftp_manual_nav.lock().unwrap().insert(tab_id.clone(), false);
            if let Some(w) = weak.upgrade() {
                w.set_active_tab_id(tab_id.clone().into());
            }

            // Spawn SSH shell worker.
            //
            // Pass the current best-known terminal dimensions so the remote PTY
            // is opened at (approximately) the right size. The resize callback
            // will fire again shortly and send an accurate window_change if
            // needed.
            let (initial_cols, initial_rows) = *last_term_size.lock().unwrap();
            let (handle, rx) = spawn_session(
                runtime.handle(),
                tab_id.clone(),
                session.clone(),
                initial_cols,
                initial_rows,
            );
            handles.borrow_mut().insert(tab_id.clone(), handle);

            // Spawn separate SFTP connection for the same session.
            // The SFTP worker pushes SessionEvent::SftpEntries / SftpStatus
            // back via the same receiver channel (rx) — no second receiver
            // needed because spawn_sftp accepts an UnboundedSender clone.
            let sftp_evt_tx = {
                // We need the sender half of the channel that rx drains from.
                // spawn_session doesn't expose it, so we rebuild: spawn_sftp
                // gets its own sender that injects events into the same stream
                // by accepting a clone of the existing UnboundedSender.
                // Actually we pass a *new* sender that was set up alongside rx.
                // Re-examine: spawn_session returns (handle, rx) where rx came
                // from mpsc::unbounded_channel inside spawn_session; we have no
                // access to tx.  So create a second channel just for SFTP events
                // and merge them in the pump thread below.
                let (sftp_tx, sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
                let sftp_handle = spawn_sftp(runtime.handle(), session, sftp_tx);
                sftp_handles.lock().unwrap().insert(tab_id.clone(), sftp_handle);
                sftp_rx
            };

            // --- Shell event pump (dedicated thread) ----------------------
            // Blocks on shell events; handles CwdChanged with 500 ms debounce.
            {
                let weak_inner = weak.clone();
                let bufs_thread = bufs.clone();
                let sftp_handles_pump = sftp_handles.clone();
                let sftp_manual_nav_pump = sftp_manual_nav.clone();
                let rt_pump = runtime.clone();
                let tab_id_pump = tab_id.clone();
                let statuses_pump = tab_statuses.clone();
                let local_pump = local_snap.clone();
                let net_pump = local_net_hist.clone();
                std::thread::spawn(move || {
                    let mut shell_rx = rx;
                    let mut cwd_debounce: Option<tokio::task::JoinHandle<()>> = None;
                    loop {
                        match shell_rx.blocking_recv() {
                            None => break,
                            Some(shell_evt) => {
                                if let SessionEvent::CwdChanged(ref cwd) = shell_evt {
                                    let is_manual = sftp_manual_nav_pump
                                        .lock()
                                        .ok()
                                        .and_then(|m| m.get(&tab_id_pump).copied())
                                        .unwrap_or(false);
                                    if !is_manual {
                                        if let Some(prev) = cwd_debounce.take() {
                                            prev.abort();
                                        }
                                        let cwd = cwd.clone();
                                        let sftp_h = sftp_handles_pump.clone();
                                        let tid = tab_id_pump.clone();
                                        cwd_debounce = Some(rt_pump.spawn(async move {
                                            tokio::time::sleep(
                                                std::time::Duration::from_millis(500),
                                            )
                                            .await;
                                            if let Ok(handles) = sftp_h.lock() {
                                                if let Some(h) = handles.get(&tid) {
                                                    h.list_dir(cwd);
                                                }
                                            }
                                        }));
                                    }
                                }
                                let weak_evt = weak_inner.clone();
                                let tid = tab_id_pump.clone();
                                let bufs_evt = bufs_thread.clone();
                                let st_evt = statuses_pump.clone();
                                let lc_evt = local_pump.clone();
                                let nh_evt = net_pump.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(win) = weak_evt.upgrade() {
                                        apply_session_event_to_window(
                                            &win, &tid, shell_evt, &bufs_evt,
                                            &st_evt, &lc_evt, &nh_evt,
                                        );
                                    }
                                });
                            }
                        }
                    }
                });
            }

            // --- SFTP event pump (separate thread) -------------------------
            // Never blocks on shell; dispatches SFTP events the moment they
            // arrive so tree/file-list updates are immediate even when the
            // terminal is idle.
            {
                let weak_sftp = weak.clone();
                let bufs_sftp = bufs.clone();
                let tab_id_sftp = tab_id.clone();
                let statuses_sftp = tab_statuses.clone();
                let local_sftp = local_snap.clone();
                let net_sftp = local_net_hist.clone();
                std::thread::spawn(move || {
                    let mut sftp_rx = sftp_evt_tx;
                    loop {
                        match sftp_rx.blocking_recv() {
                            None => break,
                            Some(sftp_evt) => {
                                let weak_s = weak_sftp.clone();
                                let tid = tab_id_sftp.clone();
                                let bufs_s = bufs_sftp.clone();
                                let st_s = statuses_sftp.clone();
                                let lc_s = local_sftp.clone();
                                let nh_s = net_sftp.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(win) = weak_s.upgrade() {
                                        apply_session_event_to_window(
                                            &win, &tid, sftp_evt, &bufs_s,
                                            &st_s, &lc_s, &nh_s,
                                        );
                                    }
                                });
                            }
                        }
                    }
                });
            }
        });
    }
}

type NetHist = Arc<Mutex<Vec<f32>>>;

/// Push a value into a fixed-length ring buffer (newest at the end).
fn push_ring(buf: &mut Vec<f32>, val: f32) {
    if buf.len() != NET_HISTORY_LEN {
        *buf = vec![0.0; NET_HISTORY_LEN];
    }
    buf.remove(0);
    buf.push(val);
}

/// Auto-scale a raw bytes/sec history to 0..1 against its own window peak so the
/// sparkline always uses the full height (like FinalShell's relative graph).
fn normalized_model(buf: &[f32]) -> ModelRc<f32> {
    let max = buf.iter().cloned().fold(1.0_f32, f32::max);
    let scaled: Vec<f32> = buf.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect();
    ModelRc::from(Rc::new(VecModel::from(scaled)))
}

/// Build the filesystem-usage model (path, "avail/total", used fraction).
fn disk_model(disks: &[(String, u64, u64)]) -> ModelRc<DiskInfo> {
    let rows: Vec<DiskInfo> = disks
        .iter()
        .map(|(mount, avail, total)| {
            let used = total.saturating_sub(*avail);
            let percent = if *total > 0 {
                used as f32 / *total as f32
            } else {
                0.0
            };
            DiskInfo {
                path: mount.clone().into(),
                detail: format!("{}/{}", format_size(*avail), format_size(*total)).into(),
                percent,
            }
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Find every (case-insensitive) occurrence of `query` across the currently
/// displayed rows and return highlight rectangles (char index == grid column).
fn compute_find_matches(rows: &[String], query: &str) -> Vec<TermMatch> {
    let mut out: Vec<TermMatch> = Vec::new();
    if query.is_empty() {
        return out;
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return out;
    }
    for (r, line) in rows.iter().enumerate() {
        let lower: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut i = 0usize;
        while i + q.len() <= lower.len() {
            if lower[i..i + q.len()] == q[..] {
                out.push(TermMatch {
                    row: r as i32,
                    col: i as i32,
                    len: q.len() as i32,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Order a selection so start ≤ end (by row, then column).
fn norm_sel(sr: u16, sc: u16, er: u16, ec: u16) -> (u16, u16, u16, u16) {
    if (sr, sc) <= (er, ec) {
        (sr, sc, er, ec)
    } else {
        (er, ec, sr, sc)
    }
}

/// Highlight rectangles for a linear (line-wrapping) selection.
fn selection_rects(sr: u16, sc: u16, er: u16, ec: u16, cols: u16) -> Vec<TermMatch> {
    let (sr, sc, er, ec) = norm_sel(sr, sc, er, ec);
    let mut out = Vec::new();
    if sr == er {
        let lo = sc.min(ec);
        let hi = sc.max(ec);
        out.push(TermMatch { row: sr as i32, col: lo as i32, len: (hi - lo + 1) as i32 });
    } else {
        out.push(TermMatch { row: sr as i32, col: sc as i32, len: (cols - sc) as i32 });
        for r in (sr + 1)..er {
            out.push(TermMatch { row: r as i32, col: 0, len: cols as i32 });
        }
        out.push(TermMatch { row: er as i32, col: 0, len: (ec + 1) as i32 });
    }
    out
}

/// Extract the selected text from the displayed rows (trailing spaces trimmed).
fn extract_selection(rows: &[String], sr: u16, sc: u16, er: u16, ec: u16) -> String {
    let (sr, sc, er, ec) = norm_sel(sr, sc, er, ec);
    let mut out = String::new();
    for r in sr..=er {
        let chars: Vec<char> = rows
            .get(r as usize)
            .map(|l| l.chars().collect())
            .unwrap_or_default();
        let (lo, hi) = if sr == er {
            (sc.min(ec), sc.max(ec))
        } else if r == sr {
            (sc, u16::MAX)
        } else if r == er {
            (0, ec)
        } else {
            (0, u16::MAX)
        };
        let lo = (lo as usize).min(chars.len());
        let hi = ((hi as usize).saturating_add(1)).min(chars.len()); // exclusive
        let seg: String = if lo < hi {
            chars[lo..hi].iter().collect()
        } else {
            String::new()
        };
        out.push_str(seg.trim_end());
        if r != er {
            out.push('\n');
        }
    }
    out
}

/// Recompute spans + cursor + find/selection highlights for one tab from its
/// current vt100 screen (respecting scrollback) and push them to the model.
/// Used by scroll + selection callbacks (Output has its own equivalent inline).
fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let data = {
        let mut map = bufs.lock().unwrap();
        let Some(buf) = map.get_mut(tab_id) else { return };
        let cols = buf.parser.screen().size().1;
        let b = buf.render(); // also refreshes buf.displayed_text
        let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
        let sel = match buf.sel {
            Some((sr, sc, er, ec)) => selection_rects(sr, sc, er, ec, cols),
            None => Vec::new(),
        };
        (b, matches, sel)
    };
    let (b, matches, sel) = data;
    let spans = ModelRc::from(Rc::new(VecModel::from(b.spans)));
    let fm = ModelRc::from(Rc::new(VecModel::from(matches)));
    let sm = ModelRc::from(Rc::new(VecModel::from(sel)));
    let (cr, cc, ru, alt) = (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
    set_terminal_row(win, tab_id, move |row| {
        row.spans = spans.clone();
        row.cursor_row = cr;
        row.cursor_col = cc;
        row.rows_used = ru;
        row.is_alt_screen = alt;
        row.find_matches = fm.clone();
        row.selection = sm.clone();
    });
}

/// Resolve which interface drives the top sparkline: the user's selection if it
/// still exists, otherwise the busiest (the list is sorted busiest-first).
/// Returns (name, rx_bps, tx_bps).
fn selected_iface(st: &TabStatus) -> (String, u64, u64) {
    if !st.selected_iface.is_empty() {
        if let Some(e) = st.net.iter().find(|e| e.0 == st.selected_iface) {
            return e.clone();
        }
    }
    st.net.first().cloned().unwrap_or_default()
}

/// Recompute the whole sidebar (status dot + CPU/mem/swap + dual network panel)
/// for whichever tab is active.  Welcome tab → local machine; a session tab →
/// that server.  The bottom network graph is always the local machine.
/// Must run on the Slint event loop thread.
fn refresh_sidebar(
    win: &AppWindow,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
) {
    let pct = |used: u64, total: u64| -> f32 {
        if total > 0 {
            used as f32 / total as f32
        } else {
            0.0
        }
    };
    let snap = local.lock().unwrap().clone();

    // --- Bottom network graph: always the local machine --------------------
    win.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    win.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    win.set_net_bot_history(normalized_model(&local_net_hist.lock().unwrap()));

    let set_top_local = |win: &AppWindow| {
        win.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
        win.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
        win.set_net_top_history(normalized_model(&local_net_hist.lock().unwrap()));
        win.set_net_show_selector(false);
        win.set_net_selected("".into());
        win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        // Non-connected tabs show the local machine's filesystems.
        win.set_disks(disk_model(&snap.disks));
    };
    let show_local_res = |win: &AppWindow| {
        win.set_resource_title(t("本机资源", "Local resources").into());
        win.set_cpu_percent(snap.cpu_percent);
        win.set_mem_percent(snap.mem_percent);
        win.set_swap_percent(snap.swap_percent);
        win.set_mem_detail(format_mem_mib(snap.mem_used_mib, snap.mem_total_mib).into());
        win.set_swap_detail(format_mem_mib(snap.swap_used_mib, snap.swap_total_mib).into());
    };
    let clear_stats = |win: &AppWindow| {
        win.set_cpu_percent(0.0);
        win.set_mem_percent(0.0);
        win.set_swap_percent(0.0);
        win.set_mem_detail("".into());
        win.set_swap_detail("".into());
    };

    let active = win.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().unwrap().get(&active).cloned()
    };

    match status {
        // A live session tab → remote resources + remote NIC on top.
        Some(st) if st.state == 1 => {
            win.set_conn_state(1);
            win.set_connection_state(st.host.clone().into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            win.set_cpu_percent(st.cpu);
            win.set_mem_percent(pct(st.mem_used_kib, st.mem_total_kib));
            win.set_swap_percent(pct(st.swap_used_kib, st.swap_total_kib));
            win.set_mem_detail(
                format_mem_mib(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
            );
            win.set_swap_detail(
                format_mem_mib(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            win.set_net_top_history(normalized_model(&st.net_hist));
            win.set_net_show_selector(!st.net.is_empty());
            win.set_net_selected(name.into());
            let ifaces: Vec<SharedString> =
                st.net.iter().map(|e| e.0.clone().into()).collect();
            win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::from(ifaces))));
            win.set_disks(disk_model(&st.disks));
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            win.set_conn_state(2);
            win.set_connection_state(format!("{} {}", st.host, t("已断开", "disconnected")).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
        }
        // Welcome tab (or unknown) → local machine top + bottom.
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            show_local_res(win);
            set_top_local(win);
        }
    }
}

/// Apply a session event to the live UI models. Must be called on the Slint
/// event loop thread.
fn apply_session_event_to_window(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    bufs: &TermBuffers,
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
    };

    match event {
        SessionEvent::Status(status) => {
            update_terminal(&|t| t.status = status.clone().into());
        }
        SessionEvent::Output(chunk) => {
            // Feed raw bytes into the vt100 parser. vt100 correctly handles
            // cursor movement, \r + line-redraw (readline), \x1b[K (erase to
            // EOL), alternate-screen switching, and all VT100/xterm sequences.
            // We then split the rendered screen at cursor_position() so Slint
            // can insert the blinking "█" at the exact cursor cell.
            let built = {
                let mut map = bufs.lock().unwrap();
                if let Some(buf) = map.get_mut(tab_id) {
                    // Capture scrolled-off lines into history, then render the
                    // current view (live or scrolled-back).
                    buf.ingest(chunk.as_bytes());
                    let cols = buf.parser.screen().size().1;
                    let b = buf.render(); // refreshes buf.displayed_text
                    let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
                    let sel = match buf.sel {
                        Some((sr, sc, er, ec)) => selection_rects(sr, sc, er, ec, cols),
                        None => Vec::new(),
                    };
                    Some((b, matches, sel))
                } else {
                    None
                }
            };
            if let Some((b, matches, sel)) = built {
                let spans_model: ModelRc<TermSpan> =
                    ModelRc::from(std::rc::Rc::new(VecModel::from(b.spans)));
                let matches_model: ModelRc<TermMatch> =
                    ModelRc::from(std::rc::Rc::new(VecModel::from(matches)));
                let sel_model: ModelRc<TermMatch> =
                    ModelRc::from(std::rc::Rc::new(VecModel::from(sel)));
                let (cur_row, cur_col, rows_used, is_alt) =
                    (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
                update_terminal(&|t| {
                    t.spans = spans_model.clone();
                    t.cursor_row = cur_row;
                    t.cursor_col = cur_col;
                    t.rows_used = rows_used;
                    t.is_alt_screen = is_alt;
                    t.find_matches = matches_model.clone();
                    t.selection = sel_model.clone();
                });
            }
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
            update_tab(&|t| t.connected = false);
            update_terminal(&|t| t.status = format!("{} — {reason}", crate::i18n::t("已断开", "Disconnected")).into());
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
        } => {
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.cpu = cpu_percent;
                st.mem_used_kib = mem_used_kib;
                st.mem_total_kib = mem_total_kib;
                st.swap_used_kib = swap_used_kib;
                st.swap_total_kib = swap_total_kib;
                st.net = net;
                st.disks = disks;
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
            let slint_entries: Vec<SftpEntry> = entries
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
                    modified: format_mtime(e.modified).into(),
                })
                .collect();
            let model = ModelRc::from(
                std::rc::Rc::new(VecModel::from(slint_entries)),
            );
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_entries = model.clone();
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpStatus(msg) => {
            update_terminal(&|t| t.sftp_status = msg.clone().into());
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
            msg: _,
        } => {
            let detail = match state {
                2 => t("失败", "Failed").to_string(),
                1 => t("已完成", "Done").to_string(),
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
    }
}

// ---------------------------------------------------------------------------
// Tab callbacks
// ---------------------------------------------------------------------------

fn wire_tab_callbacks(
    window: &AppWindow,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
) {
    // Selecting a tab is already applied inside the Slint callback; we just
    // need to keep the C++/Rust state in sync if needed.
    {
        window.on_tab_selected(move |_id: SharedString| {
            // No-op: AppWindow.active-tab-id is updated inline in the .slint.
        });
    }

    {
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_manual_nav = sftp_manual_nav.clone();
        window.on_tab_closed(move |id: SharedString| {
            let id = id.to_string();
            if id == "welcome" {
                return;
            }
            if let Some(handle) = handles.borrow_mut().remove(&id) {
                handle.close();
            }
            if let Some(sftp) = sftp_handles.lock().unwrap().remove(&id) {
                sftp.close();
            }
            sftp_manual_nav.lock().unwrap().remove(&id);
            bufs.lock().unwrap().remove(&id);

            // Remove from tabs + terminals models.
            let mut idx = None;
            for i in 0..tabs_model.row_count() {
                if tabs_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    idx = Some(i);
                    break;
                }
            }
            if let Some(i) = idx {
                tabs_model.remove(i);
            }
            let mut tidx = None;
            for i in 0..terminals_model.row_count() {
                if terminals_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    tidx = Some(i);
                    break;
                }
            }
            if let Some(i) = tidx {
                terminals_model.remove(i);
            }

            // If we closed the active tab, fall back to the welcome page.
            if let Some(w) = weak.upgrade() {
                if w.get_active_tab_id().as_str() == id {
                    w.set_active_tab_id("welcome".into());
                }
            }
        });
    }

    {
        let weak = window.as_weak();
        window.on_new_tab_clicked(move || {
            if let Some(w) = weak.upgrade() {
                w.set_active_tab_id("welcome".into());
            }
        });
    }
}

// ---------------------------------------------------------------------------
// SFTP callbacks
// ---------------------------------------------------------------------------

fn wire_sftp_callbacks(
    window: &AppWindow,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
) {
    // Navigate to a remote path (or ".." to go up one level).
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_manual_nav = sftp_manual_nav.clone();
        let weak = window.as_weak();
        window.on_sftp_navigate(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let resolved = if path.as_str() == ".." {
                let current = weak.upgrade().and_then(|w| {
                    let terminals_rc = w.get_terminals();
                    let terminals = terminals_rc
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    for i in 0..terminals.row_count() {
                        if let Some(row) = terminals.row_data(i) {
                            if row.id.as_str() == tab_id {
                                return Some(row.sftp_path.to_string());
                            }
                        }
                    }
                    None
                });
                parent_path(&current.unwrap_or_else(|| "/".to_string()))
            } else {
                path.to_string()
            };
            // Any manual navigation stops cd auto-follow.
            sftp_manual_nav.lock().unwrap().insert(tab_id.clone(), true);
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.list_dir(resolved);
                }
            }
        });
    }

    // Download a remote file.  If a download folder is preset in settings, save
    // straight there; otherwise fall back to a native folder picker.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download(move |tab_id: SharedString, remote_path: SharedString| {
            let tab_id = tab_id.to_string();
            let remote_path = remote_path.to_string();
            let preset = weak
                .upgrade()
                .map(|w| w.get_download_dir().to_string())
                .unwrap_or_default();
            if !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(&tab_id) {
                        h.download(remote_path, preset);
                    }
                }
                return;
            }
            let sftp_handles = sftp_handles.clone();
            std::thread::spawn(move || {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let local_dir = dir.to_string_lossy().to_string();
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            h.download(remote_path, local_dir);
                        }
                    }
                }
            });
        });
    }

    // Upload a local file into the current remote directory.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_upload_clicked(
            move |tab_id: SharedString, remote_dir: SharedString| {
                let tab_id = tab_id.to_string();
                let remote_dir = remote_dir.to_string();
                let sftp_handles = sftp_handles.clone();
                std::thread::spawn(move || {
                    if let Some(file) = rfd::FileDialog::new().pick_file() {
                        let local = file.to_string_lossy().to_string();
                        if let Ok(handles) = sftp_handles.lock() {
                            if let Some(h) = handles.get(&tab_id) {
                                h.upload(local, remote_dir);
                            }
                        }
                    }
                });
            },
        );
    }

    // Refresh the current directory listing.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_refresh(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.list_dir(path);
                }
            }
        });
    }

    // Toggle tree node expand/collapse and navigate to that directory.
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_manual_nav = sftp_manual_nav.clone();
        window.on_sftp_tree_expand(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            // Manual tree navigation stops cd auto-follow.
            sftp_manual_nav.lock().unwrap().insert(tab_id.clone(), true);
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.toggle_tree_node(path.clone());
                    h.list_dir(path);
                }
            }
        });
    }

    // Context menu → 删除 a remote file.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_delete(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.delete(path.to_string());
                }
            }
        });
    }

    // Context menu → 查看 (open read-only) / 编辑 (open + auto-reupload).
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_view(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), false);
                }
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), true);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Raw keystroke forwarding and PTY resize
// ---------------------------------------------------------------------------

fn wire_key_input(
    window: &AppWindow,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    last_term_size: Arc<Mutex<(u32, u32)>>,
) {
    // Forward each keystroke as raw bytes to the SSH PTY. The server's bash /
    // readline handles echo, history (↑↓), Tab completion, Ctrl+C, etc.
    {
        let handles = handles.clone();
        let bufs = bufs.clone();
        // Shared timestamp: the last time the Shift key alone was pressed
        // (key="", shift=true).  Used by the time-based Backspace filter below.
        let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> =
            Arc::new(Mutex::new(None));
        window.on_send_key(move |tab_id: SharedString, key: SharedString, ctrl: bool, alt: bool, shift: bool| {
            // Check whether the remote PTY switched to application cursor mode
            // (DECCKM, set by nano/vim via \x1b[?1h). In that mode the terminal
            // must send \x1bOA/B/C/D instead of \x1b[A/B/C/D.
            let app_cursor = {
                let mut map = bufs.lock().unwrap();
                match map.get_mut(tab_id.as_str()) {
                    Some(b) => {
                        // Typing snaps the view back to the live bottom so the
                        // user always sees what they're entering.
                        b.view_offset = 0;
                        b.parser.screen().application_cursor()
                    }
                    None => false,
                }
            };
            tracing::debug!(
                "send_key tab={} key={:?} ctrl={} alt={} shift={} app_cursor={}",
                tab_id, key.as_str(), ctrl, alt, shift, app_cursor
            );

            // ── Shift / Backspace 诊断日志 (info 级, 无需 RUST_LOG=debug) ─────
            // 每个 Shift 相关事件都打印 key 的 Unicode 码位，方便对比
            // 左Shift / 右Shift 是否产生不同的 key 字符串。
            if shift || key.as_str() == "\u{0008}" {
                let codepoints: Vec<String> = if key.as_str().is_empty() {
                    vec!["(empty)".to_string()]
                } else {
                    key.as_str().chars().map(|c| format!("U+{:04X}", c as u32)).collect()
                };
                let elapsed_ms = last_shift_time
                    .lock()
                    .unwrap()
                    .map(|t| format!("{}ms ago", t.elapsed().as_millis()))
                    .unwrap_or_else(|| "never".to_string());
                tracing::info!(
                    "[KEY_DIAG] key={} shift={} ctrl={} alt={} | last_shift={}",
                    codepoints.join(","), shift, ctrl, alt, elapsed_ms
                );
            }

            // ── Track lone-Shift presses for the time-based Backspace filter ──
            // Slint sends key="" (empty string) when a bare modifier key (Shift,
            // Ctrl, Alt) is pressed.  We record the timestamp whenever Shift
            // alone fires so the filter below can catch IME-injected Backspace
            // events even if they arrive with shift=false.
            if key.as_str().is_empty() && shift && !ctrl && !alt {
                *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                tracing::info!("[KEY_DIAG] lone-Shift recorded → timestamp saved");
            }

            // ── 拦截百度拼音注入的 Shift 标记字符（核心修复）────────────────────
            // 诊断日志证实，百度拼音通过 WH_KEYBOARD_LL 钩子，在 Shift 键按下时
            // 向消息队列注入一个 C0 控制字符，而非空字符串：
            //
            //   左 Shift → U+0015 (Ctrl+U / NAK), shift=true, ctrl=false
            //   右 Shift → U+0010 (Ctrl+P / DLE), shift=true, ctrl=false
            //              紧接着注入: U+0008 (Backspace), shift=false
            //
            // 这些字符绝对不应送入 PTY：
            //   0x15 (Ctrl+U) 在 bash/vim 中会清空当前输入行 → "左Shift替换字符"
            //   0x10 (Ctrl+P) 在 vim 中翻历史/触发补全     → "右Shift乱跳"
            //   0x08 (Backspace) 紧随其后                   → "右Shift删除字符"
            //
            // 合法独立 C0 键（Backspace=0x08, Tab=0x09, LF=0x0A, CR=0x0D,
            // ESC=0x1B）不受此过滤影响，由下方代码单独处理。
            //
            // 检测到 IME Shift 标记后，记录时间戳，让 Layer 2 在 1500ms 内
            // 拦截随后可能到来的 Backspace（右Shift场景，日志显示间隔约 914ms）。
            if !ctrl && !alt {
                if let Some(c) = key.as_str().chars().next() {
                    let cp = c as u32;
                    let is_standalone = matches!(cp, 0x08 | 0x09 | 0x0A | 0x0D | 0x1B);
                    if key.as_str().chars().count() == 1
                        && (0x01..=0x1f).contains(&cp)
                        && !is_standalone
                    {
                        *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                        tracing::info!(
                            "[KEY_DIAG] DROPPED IME C0 marker U+{:04X} (shift={}) → timestamp saved",
                            cp, shift
                        );
                        return;
                    }
                }
            }

            // ── Windows: filter synthetic Ctrl+char injections ──────────────
            // Some keyboards / IME drivers (e.g. Aula F99 + Baidu Pinyin)
            // inject a synthetic WM_CHAR 0x11 (Ctrl+Q) when Left Ctrl is
            // briefly tapped, WITHOUT sending a WM_KEYDOWN VK_Q beforehand.
            //
            // FinalShell avoids this because it builds Ctrl+letter from
            // WM_KEYDOWN (virtual-key codes).  Slint uses WM_CHAR, so it
            // sees the injected byte and forwards it straight to us.
            //
            // Fix: for C0 control chars (Ctrl+A…Ctrl+Z, i.e. 0x01–0x1A),
            // use GetKeyState — which returns the key state *as of the last
            // processed message*, not the live hardware state — to verify
            // the corresponding letter VK was actually queued as a keydown
            // before this WM_CHAR arrived.  If Q was never keyed down,
            // GetKeyState(VK_Q) = 0 → the event is synthetic → drop it.
            #[cfg(windows)]
            if ctrl {
                if let Some(ch) = key.as_str().chars().next() {
                    let cp = ch as u32;
                    // Always let Enter / Tab pass through regardless of Ctrl
                    // state.  These C0 codes (0x09 Tab, 0x0a LF, 0x0d CR) are
                    // "double-duty" keys: pressing Enter while Ctrl is still
                    // physically held (e.g. just after Ctrl+O in nano) generates
                    // Ctrl+M (0x0d) with ctrl=true — but GetKeyState(VK_M) is 0
                    // because the user never pressed M.  Without this exemption
                    // the filter would silently drop the Enter, making it
                    // impossible to confirm nano's "File Name to Write:" prompt.
                    let always_pass = matches!(cp, 0x09 | 0x0a | 0x0d);
                    if !always_pass
                        && key.as_str().chars().count() == 1
                        && (0x01..=0x1a).contains(&cp)
                        && !c0_letter_key_down(cp)
                    {
                        tracing::debug!(
                            "send_key: dropped synthetic Ctrl+{} \
                             (VK_{:02X} not down per GetKeyState)",
                            (0x40u8 + cp as u8) as char,
                            cp + 0x40
                        );
                        return;
                    }
                }
            }

            // ── Filter synthetic Backspace injected by Chinese IME ────────────
            // Baidu Pinyin (and similar Chinese IMEs) hooks the keyboard at the
            // driver level via WH_KEYBOARD_LL, below Win32's ImmDisableIME.
            // When the user presses Shift to switch from Chinese to English mode
            // while a pinyin syllable is in-flight, the IME:
            //   1. Cancels the composition (discards the syllable).
            //   2. Posts WM_KEYDOWN VK_BACK + WM_CHAR 0x08 to erase whatever
            //      character it had already forwarded to the app.
            //
            // Three-layer defence:
            //
            //   Layer 1 – shift=true guard.
            //     The synthetic Backspace arrives during Shift keydown, so
            //     GetKeyState(VK_SHIFT) is still "down" → Slint reports shift=true.
            //     Drop any Backspace (0x08) arriving while Shift is flagged.
            //
            //   Layer 2 – time-based guard.
            //     Baidu Pinyin posts WM_CHAR 0x08 asynchronously, so by the time
            //     the message is dequeued Shift may already read as "up"
            //     → shift=false defeats Layer 1.
            //     Mitigation: we recorded the timestamp when the Shift key alone
            //     was pressed (key="", shift=true) a few lines above.  Drop any
            //     Backspace arriving within 200 ms of that moment.
            //
            //   Layer 3 – GetKeyState guard (belt-and-suspenders).
            //     If VK_BACK is not actually "down" (i.e. no real WM_KEYDOWN
            //     VK_BACK was ever queued), the Backspace must be synthetic.
            if key.as_str() == "\u{0008}" && !ctrl && !alt {
                // Layer 1
                if shift {
                    tracing::info!("[KEY_DIAG] Backspace DROPPED by layer-1 (shift=true)");
                    return;
                }
                // Layer 2 — 时间窗口 1500ms
                // 日志显示百度拼音注入 U+0010(右Shift标记) 到 Backspace 之间
                // 间隔约 914ms，因此窗口设为 1500ms 以覆盖该场景。
                let (shift_just_pressed, elapsed_ms) = {
                    let guard = last_shift_time.lock().unwrap();
                    match *guard {
                        Some(t) => {
                            let ms = t.elapsed().as_millis();
                            (ms < 1500, ms)
                        }
                        None => (false, 0),
                    }
                };
                if shift_just_pressed {
                    tracing::info!(
                        "[KEY_DIAG] Backspace DROPPED by layer-2 ({}ms after IME Shift marker)",
                        elapsed_ms
                    );
                    return;
                }
                // Layer 3
                #[cfg(windows)]
                if !is_vk_back_down() {
                    tracing::info!("[KEY_DIAG] Backspace DROPPED by layer-3 (VK_BACK not down)");
                    return;
                }
                tracing::info!("[KEY_DIAG] Backspace PASSED all filters → sent to PTY");
            }

            let bytes = key_to_pty_bytes(key.as_str(), ctrl, alt, app_cursor);
            tracing::debug!(
                "send_key bytes={:02x?} handle_exists={}",
                bytes,
                handles.borrow().contains_key(tab_id.as_str()),
            );
            if !bytes.is_empty() {
                if let Some(handle) = handles.borrow().get(tab_id.as_str()) {
                    handle.send_raw(bytes);
                }
            }
        });
    }

    // Propagate PTY resize to the SSH worker and vt100 parser. Pixel
    // dimensions come from Slint; we approximate col/row counts using
    // Consolas 13px metrics.
    //
    // terminal_view.slint now passes the FocusScope height (not the full
    // TerminalView height), so the SFTP panel is already excluded.
    // Layout breakdown for the FocusScope:
    //   16 px  – bottom strip (TouchArea for focus-regain)
    //    8 px  – y-offset of the output Text element inside the Flickable
    // = 24 px  total vertical chrome within FocusScope
    //
    // Consolas 13 px renders at ≈ 8 px wide × 16 px tall per cell.
    {
        let handles = handles.clone();
        let bufs_resize = bufs.clone(); // keep bufs alive for the copy handler below
        // The Slint side now measures the real Consolas cell size (via a hidden
        // probe Text) and passes whole column/row counts directly, so there is
        // no pixel→cell guesswork here.  This keeps full-screen programs like
        // nano from over-counting rows and clipping their bottom shortcut bar.
        window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
            let cols = (cols_f as u32).max(10);
            let rows = (rows_f as u32).max(5);
            tracing::debug!(
                "terminal_resize tab={} cols={} rows={}",
                tab_id, cols, rows
            );
            // Keep the shared size up-to-date so future connections start
            // with the correct PTY dimensions.
            *last_term_size.lock().unwrap() = (cols, rows);
            if let Some(handle) = handles.borrow().get(tab_id.as_str()) {
                handle.resize(cols, rows);
            }
            if let Some(buf) = bufs_resize.lock().unwrap().get_mut(tab_id.as_str()) {
                buf.parser.set_size(rows as u16, cols as u16);
            }
        });
    }

    // Ctrl+Shift+C: copy current terminal screen to clipboard.
    {
        let bufs = bufs.clone();
        window.on_copy_terminal_text(move |tab_id: SharedString| {
            let text = {
                let map = bufs.lock().unwrap();
                match map.get(tab_id.as_str()) {
                    Some(buf) => {
                        // Copy the drag-selection when there is one, else the
                        // whole displayed screen.
                        match buf.sel {
                            Some((sr, sc, er, ec)) if (sr, sc) != (er, ec) => {
                                extract_selection(&buf.displayed_text, sr, sc, er, ec)
                            }
                            _ => buf.displayed_text.join("\n"),
                        }
                    }
                    None => String::new(),
                }
            };
            // Run the clipboard write on a dedicated OS thread.  arboard's
            // Windows backend opens the clipboard and pumps Win32 messages;
            // doing that on the Slint/winit event-loop thread re-enters the
            // message loop and dead-locks the whole UI.
            std::thread::spawn(move || {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                    Ok(()) => tracing::debug!("copy_terminal: clipboard updated"),
                    Err(e) => tracing::warn!("copy_terminal: clipboard error: {}", e),
                }
            });
        });
    }

    // Middle-click / Ctrl+Shift+V: paste clipboard text into PTY.
    {
        let handles = handles.clone();
        window.on_paste_from_clipboard(move |tab_id: SharedString| {
            // Clone the (Send) command sender for this tab so the clipboard read
            // can run off the UI thread.  Reading arboard on the event-loop
            // thread is what froze the app on middle-click / paste — see the
            // copy handler above for the deadlock explanation.
            let sender = handles
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone());
            let Some(sender) = sender else { return };
            std::thread::spawn(move || {
                match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    Ok(text) => {
                        let _ = sender.send(SessionCommand::RawInput(text.into_bytes()));
                    }
                    Err(e) => tracing::warn!("paste_from_clipboard: clipboard error: {}", e),
                }
            });
        });
    }

    // Context menu → 清空缓存: reset the local vt100 buffer (drops scrollback),
    // wipe the displayed screen, then nudge the remote to redraw a fresh prompt.
    {
        let bufs_clear = bufs.clone();
        let handles_clear = handles.clone();
        let weak = window.as_weak();
        window.on_clear_terminal(move |tab_id: SharedString| {
            let tid = tab_id.to_string();
            if let Some(buf) = bufs_clear.lock().unwrap().get_mut(&tid) {
                let (rows, cols) = buf.parser.screen().size();
                buf.parser = vt100::Parser::new(rows, cols, 5000);
                buf.find_query.clear();
                buf.history = Vec::new(); // recycle the session scrollback
                buf.prev = Vec::new();
                buf.view_offset = 0;
                buf.sel = None;
                buf.displayed_text = Vec::new();
            }
            if let Some(win) = weak.upgrade() {
                set_terminal_row(&win, &tid, |row| {
                    row.spans =
                        ModelRc::from(Rc::new(VecModel::<TermSpan>::default()));
                    row.find_matches =
                        ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.selection =
                        ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.cursor_row = 0;
                    row.cursor_col = 0;
                    row.rows_used = 0;
                });
            }
            if let Some(h) = handles_clear.borrow().get(&tid) {
                h.send_raw(vec![0x0c]); // Ctrl+L → shell clears + redraws prompt
            }
        });
    }

    // Context menu → 查找: store the query and recompute highlight rectangles.
    {
        let bufs_find = bufs.clone();
        let weak = window.as_weak();
        window.on_find_query_changed(move |tab_id: SharedString, query: SharedString| {
            let tid = tab_id.to_string();
            let q = query.to_string();
            let matches = {
                let mut map = bufs_find.lock().unwrap();
                if let Some(buf) = map.get_mut(&tid) {
                    buf.find_query = q.clone();
                    compute_find_matches(&buf.displayed_text, &q)
                } else {
                    Vec::new()
                }
            };
            if let Some(win) = weak.upgrade() {
                let model = ModelRc::from(Rc::new(VecModel::from(matches)));
                set_terminal_row(&win, &tid, |row| {
                    row.find_matches = model.clone();
                });
            }
        });
    }

    // Mouse-wheel → scroll the scrollback history.
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll(move |tab_id: SharedString, delta: i32| {
            let tid = tab_id.to_string();
            {
                let mut map = bufs_scroll.lock().unwrap();
                let Some(buf) = map.get_mut(&tid) else { return };
                // Scroll within our own session scrollback (history lines above
                // the live screen).  Offset 0 = live bottom.
                let max_off = buf.history.len() as i64;
                let cur = buf.view_offset as i64;
                buf.view_offset = (cur + delta as i64).clamp(0, max_off) as usize;
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_scroll, &tid);
            }
        });
    }

    // Drag-selection lifecycle.
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_start(move |tab_id: SharedString, row: i32, col: i32| {
            let tid = tab_id.to_string();
            {
                let mut map = bufs_sel.lock().unwrap();
                let Some(buf) = map.get_mut(&tid) else { return };
                let (rows, cols) = buf.parser.screen().size();
                let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
                let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
                buf.sel = Some((r, c, r, c));
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_update(move |tab_id: SharedString, row: i32, col: i32| {
            let tid = tab_id.to_string();
            {
                let mut map = bufs_sel.lock().unwrap();
                let Some(buf) = map.get_mut(&tid) else { return };
                let (rows, cols) = buf.parser.screen().size();
                let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
                let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
                if let Some((sr, sc, _, _)) = buf.sel {
                    buf.sel = Some((sr, sc, r, c));
                }
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_end(move |tab_id: SharedString| {
            let tid = tab_id.to_string();
            // Extract the selected text; a zero-area selection (a plain click)
            // is cleared instead of copied.
            let text = {
                let mut map = bufs_sel.lock().unwrap();
                let Some(buf) = map.get_mut(&tid) else { return };
                match buf.sel {
                    Some((sr, sc, er, ec)) if (sr, sc) != (er, ec) => {
                        Some(extract_selection(&buf.displayed_text, sr, sc, er, ec))
                    }
                    _ => {
                        buf.sel = None; // treat as click → clear selection
                        None
                    }
                }
            };
            match text {
                Some(t) if !t.is_empty() => {
                    // Auto-copy on release (select-to-copy, PuTTY style).
                    std::thread::spawn(move || {
                        let _ = arboard::Clipboard::new()
                            .and_then(|mut cb| cb.set_text(t));
                    });
                }
                _ => {}
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
    // Auto-scroll while drag-selecting past the visible top/bottom edge.  We
    // move the scrollback view by a couple of lines per tick and shift the
    // selection anchor by the same amount so it stays pinned to its content
    // while the end is parked at the edge row.
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_autoscroll(move |tab_id: SharedString, dir: i32| {
            let tid = tab_id.to_string();
            {
                let mut map = bufs_sel.lock().unwrap();
                let Some(buf) = map.get_mut(&tid) else { return };
                // No scrollback on the alternate screen (vim/btop own the view).
                if buf.parser.screen().alternate_screen() {
                    return;
                }
                let rows = buf.parser.screen().size().0;
                let last = rows.saturating_sub(1);
                let max_off = buf.history.len();
                let step = 2usize;
                let Some((sr, sc, _er, ec)) = buf.sel else { return };
                if dir < 0 {
                    // Mouse above the top → reveal older lines.
                    let new_off = (buf.view_offset + step).min(max_off);
                    let delta = new_off - buf.view_offset;
                    if delta == 0 {
                        return; // already at the oldest line
                    }
                    buf.view_offset = new_off;
                    let nsr = ((sr as usize) + delta).min(last as usize) as u16;
                    buf.sel = Some((nsr, sc, 0, ec));
                } else if dir > 0 {
                    // Mouse below the bottom → move toward the live tail.
                    let new_off = buf.view_offset.saturating_sub(step);
                    let delta = buf.view_offset - new_off;
                    if delta == 0 {
                        return; // already at the live bottom
                    }
                    buf.view_offset = new_off;
                    let nsr = (sr as i32 - delta as i32).max(0) as u16;
                    buf.sel = Some((nsr, sc, last, ec));
                }
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
}

/// Mutate the `TerminalState` whose id matches `tab_id` in the live model.
/// Must run on the Slint event loop thread.
fn set_terminal_row(win: &AppWindow, tab_id: &str, mutator: impl Fn(&mut TerminalState)) {
    let terminals = win.get_terminals();
    let Some(model) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                break;
            }
        }
    }
}

/// Convert a Slint `KeyEvent.text` + modifier flags into the byte sequence
/// that the remote PTY expects.
///
/// Slint uses Unicode Private Use Area (`\u{F700}`…) for special keys.
/// Regular printable characters and C0 control characters are passed as-is.
///
/// `app_cursor` mirrors the remote terminal's DECCKM mode (`\x1b[?1h/l`):
/// when true the four arrow keys must use SS3 sequences (`\x1bOA`…) instead
/// of the default CSI sequences (`\x1b[A`…).  Full-screen apps like nano and
/// vim set this mode on startup.
fn key_to_pty_bytes(key: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    // --- Special keys (Slint PUA code points) ------------------------------
    // Arrow keys: respect DECCKM application-cursor mode.
    let special: Option<&[u8]> = match key {
        "\u{F700}" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }), // Up
        "\u{F701}" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }), // Down
        "\u{F702}" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }), // Left
        "\u{F703}" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }), // Right
        "\u{F729}" => Some(b"\x1b[H"),   // Home
        "\u{F72B}" => Some(b"\x1b[F"),   // End
        "\u{F72C}" => Some(b"\x1b[5~"),  // PageUp
        "\u{F72D}" => Some(b"\x1b[6~"),  // PageDown
        "\u{F728}" => Some(b"\x1b[3~"),  // Delete (forward)
        "\u{F704}" => Some(b"\x1bOP"),   // F1
        "\u{F705}" => Some(b"\x1bOQ"),   // F2
        "\u{F706}" => Some(b"\x1bOR"),   // F3
        "\u{F707}" => Some(b"\x1bOS"),   // F4
        "\u{F708}" => Some(b"\x1b[15~"), // F5
        "\u{F709}" => Some(b"\x1b[17~"), // F6
        "\u{F70A}" => Some(b"\x1b[18~"), // F7
        "\u{F70B}" => Some(b"\x1b[19~"), // F8
        "\u{F70C}" => Some(b"\x1b[20~"), // F9
        "\u{F70D}" => Some(b"\x1b[21~"), // F10
        "\u{F70E}" => Some(b"\x1b[23~"), // F11
        "\u{F70F}" => Some(b"\x1b[24~"), // F12
        _ => None,
    };
    if let Some(seq) = special {
        return seq.to_vec();
    }

    // Slint sometimes sends `\u{0008}` for Backspace; terminals expect DEL.
    if key == "\u{0008}" {
        return vec![0x7f];
    }

    // Slint encodes Key::Return as "\n" (U+000A, LF).  Every real terminal
    // emulator (xterm, WezTerm, PuTTY …) sends 0x0D (CR) for Enter because
    // that is what a physical keyboard generates over a serial line.  bash/
    // readline happens to accept LF too, but ncurses apps in raw mode (nano,
    // vim command-line, passwd prompts …) strictly require CR to confirm input.
    // Ctrl+J (ctrl=true, "\n") intentionally stays 0x0A — it is a distinct
    // control character in some applications.
    if key == "\n" && !ctrl && !alt {
        return vec![0x0d];
    }

    // Empty text (e.g. the Ctrl/Shift/Alt key press itself) — nothing to send.
    if key.is_empty() {
        return vec![];
    }

    // --- Ctrl + letter: synthesise C0 control character --------------------
    // Two cases:
    //   A) Platform already encoded the control char in `key` (e.g. "\x18" for
    //      Ctrl+X on some Linux/macOS builds). Pass through directly.
    //   B) Platform sends the letter ("x") with modifiers.control=true.
    //      We synthesise the C0 code ourselves.
    if ctrl {
        // Case A: key is already a C0 control character (0x01..0x1F, not ESC).
        if let Some(c) = key.chars().next() {
            let cp = c as u32;
            if key.chars().count() == 1 && (0x01..=0x1f).contains(&cp) {
                return vec![cp as u8];
            }
        }
        // Case B: letter + ctrl modifier.
        if let Some(c) = key.chars().next() {
            if key.chars().count() == 1 {
                let upper = c.to_ascii_uppercase() as u8;
                let ctrl_char: Option<u8> = match upper {
                    b'A'..=b'Z' => Some(upper - b'A' + 1),      // Ctrl+A=\x01 … Ctrl+Z=\x1A
                    b'[' => Some(0x1b),                           // Ctrl+[ = ESC
                    b'\\' => Some(0x1c),
                    b']' => Some(0x1d),
                    b'^' => Some(0x1e),
                    b'_' => Some(0x1f),
                    b'@' => Some(0x00),
                    _ => None,
                };
                if let Some(byte) = ctrl_char {
                    return vec![byte];
                }
            }
        }
    }

    // --- Skip unknown Private Use Area code points -------------------------
    if key.chars().any(|c| (0xE000..=0xF8FF).contains(&(c as u32))) {
        return vec![];
    }

    // --- Alt + key: prefix with ESC ----------------------------------------
    if alt && !ctrl {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(key.as_bytes());
        return bytes;
    }

    // --- Everything else: send UTF-8 bytes as-is ---------------------------
    // This covers printable characters, \r (Enter), \t (Tab), \x1b (Escape),
    // and any C0 control chars the platform already encoded in `key`.
    key.as_bytes().to_vec()
}

/// Windows-only: returns `true` when the physical Backspace key (VK_BACK) is
/// currently "down" according to `GetKeyState`.
///
/// Used to distinguish real Backspace key presses from synthetic WM_CHAR 0x08
/// events injected by IME drivers (Baidu Pinyin, etc.) when they cancel an
/// in-flight composition.  For a real Backspace, WM_KEYDOWN VK_BACK precedes
/// WM_CHAR 0x08, so GetKeyState returns "down".  For an IME-synthesised
/// Backspace, no VK_BACK keydown was queued, so GetKeyState returns "up".
#[cfg(windows)]
fn is_vk_back_down() -> bool {
    #[allow(non_snake_case)]
    extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    const VK_BACK: i32 = 0x08;
    unsafe { (GetKeyState(VK_BACK) as u16) & 0x8000 != 0 }
}

/// Windows-only: returns `true` when the letter key for a C0 control code
/// is currently "down" according to `GetKeyState`.
///
/// `GetKeyState` is synchronised with the Windows message queue: its value
/// reflects the state as of the *last message processed by this thread*.
/// When we are called from within a `WM_CHAR` dispatch:
///
/// * **Real Ctrl+Q**: `WM_KEYDOWN VK_Q` was dequeued and processed just
///   before `WM_CHAR 0x11`, so `GetKeyState(VK_Q)` returns "down". ✓
/// * **Synthetic injection** (Aula F99 / Baidu Pinyin tap-Left-Ctrl):
///   the driver posts `WM_CHAR 0x11` directly — no `WM_KEYDOWN VK_Q` was
///   ever in the queue — so `GetKeyState(VK_Q)` returns "up". → dropped ✓
///
/// `cp` is the C0 code point (0x01 = Ctrl+A … 0x1A = Ctrl+Z).
/// Returns `true` (allow) for code points outside 0x01–0x1A (e.g. ESC).
#[cfg(windows)]
fn c0_letter_key_down(cp: u32) -> bool {
    if !(0x01..=0x1a).contains(&cp) {
        return true; // Not a Ctrl+letter — don't filter.
    }
    let vk = (cp + 0x40) as i32; // 0x01→0x41 ('A') … 0x11→0x51 ('Q') …
    #[allow(non_snake_case)]
    extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    unsafe { (GetKeyState(vk) as u16) & 0x8000 != 0 }
}

/// A coloured, cursor-annotated snapshot ready for the Slint terminal grid.
struct BuiltScreen {
    spans: Vec<TermSpan>,
    cursor_row: i32,
    cursor_col: i32,
    rows_used: i32,
    is_alt: bool,
}

/// One coloured run within a line (its grid row is assigned at render time).
#[derive(Clone)]
struct HistSpan {
    text: String,
    fg: slint::Color,
    bg: slint::Color,
    bold: bool,
    col: i32,
    cells: i32,
}

/// A rendered line: plain text (one char per cell, for find/selection) + runs.
type Line = (String, Vec<HistSpan>);

/// Per-session scrollback cap (recycled on clear / tab close).
const MAX_HISTORY: usize = 100_000;

/// Build one screen row into `(plain_text, coloured_runs)`.  `plain` carries one
/// char per cell (space for blanks) so a char index equals the grid column.
/// Effective (contents, fg, bg, bold) for one grid cell, applying reverse-video.
/// `contents` is always one display string (" " for a blank cell).
fn cell_attrs(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
) -> (String, vt100::Color, vt100::Color, bool) {
    match screen.cell(r, c) {
        Some(cell) => {
            let (mut fg, mut bg) = (cell.fgcolor(), cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let s = cell.contents();
            let s = if s.is_empty() { " ".to_string() } else { s };
            (s, fg, bg, cell.bold())
        }
        None => (
            " ".to_string(),
            vt100::Color::Default,
            vt100::Color::Default,
            false,
        ),
    }
}

fn build_row(screen: &vt100::Screen, r: u16, cols: u16) -> Line {
    let mut plain = String::with_capacity(cols as usize);
    let mut runs: Vec<HistSpan> = Vec::new();
    let mut c = 0u16;
    while c < cols {
        let (s, fg, bg, bold) = cell_attrs(screen, r, c);
        // Group consecutive cells that share fg + bg + bold into one run.  Unlike
        // before we keep blank cells *inside* a run (so a coloured bar made of
        // spaces still gets a background fill) and break only on attribute change.
        let start_col = c;
        let mut text = s.clone();
        plain.push_str(&s);
        c += 1;
        while c < cols {
            let (cs, cfg, cbg, cbold) = cell_attrs(screen, r, c);
            if cfg != fg || cbg != bg || cbold != bold {
                break;
            }
            plain.push_str(&cs);
            text.push_str(&cs);
            c += 1;
        }
        let cells = (c - start_col) as i32;
        let is_blank = text.chars().all(|ch| ch == ' ');
        let bg_default = matches!(bg, vt100::Color::Default);
        // Skip runs that contribute nothing visible: blank text *and* default bg.
        if is_blank && bg_default {
            continue;
        }
        runs.push(HistSpan {
            text,
            fg: vt_color_to_slint(fg, bold),
            bg: vt_bg_to_slint(bg),
            bold,
            col: start_col as i32,
            cells,
        });
    }
    (plain, runs)
}

/// Detect how many lines scrolled off the top between two screen snapshots by
/// finding the vertical shift `k` that best aligns `prev` onto `curr` (longest
/// top-anchored run of equal plain-text lines).  `k` lines left the top.
fn detect_scroll(prev: &[Line], curr: &[Line]) -> usize {
    let mut best_k = 0usize;
    let mut best_len = 0usize;
    for k in 0..prev.len() {
        let mut p = 0usize;
        while k + p < prev.len() && p < curr.len() && prev[k + p].0 == curr[p].0 {
            p += 1;
        }
        if p > best_len {
            best_len = p;
            best_k = k;
        }
    }
    best_k
}

impl TermBuffer {
    /// Feed bytes to vt100 and capture scrolled-off lines into history.
    ///
    /// We detect scroll by diffing the screen before/after a `process`, which
    /// can only recover up to one screen of shift per call.  A single large
    /// burst can scroll many screens at once, so we split the input at newline
    /// boundaries into batches of at most ~half a screen of lines and capture
    /// after each — that way no batch ever scrolls more than the diff can see,
    /// and nothing is lost.  (Splitting only on `\n` is safe: VT escape
    /// sequences never contain a newline.)
    fn ingest(&mut self, raw: &[u8]) {
        // Rewrite HVP (`ESC [ … f`) → CUP (`ESC [ … H`) so vt100 (which only
        // implements `H`) honours btop/htop's absolute cursor positioning.
        let bytes = self.rewrite_hvp(raw);
        let bytes = &bytes[..];
        let rows = self.parser.screen().size().0 as usize;
        let batch_lines = (rows / 2).max(1);
        let mut start = 0usize;
        let mut nl = 0usize;
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                nl += 1;
                if nl >= batch_lines {
                    self.ingest_chunk(&bytes[start..=i]);
                    start = i + 1;
                    nl = 0;
                }
            }
        }
        if start < bytes.len() {
            self.ingest_chunk(&bytes[start..]);
        }
    }

    /// Translate every CSI sequence terminated by `f` (HVP) into the identical
    /// sequence terminated by `H` (CUP).  The scanner state persists across
    /// calls, so a sequence split across read chunks is still handled.  Only the
    /// final byte of a CSI sequence is ever touched; text bytes pass through.
    fn rewrite_hvp(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            match self.csi_state {
                CsiState::Normal => {
                    if b == 0x1b {
                        self.csi_state = CsiState::Esc;
                    }
                    out.push(b);
                }
                CsiState::Esc => {
                    if b == b'[' {
                        self.csi_state = CsiState::Csi;
                    } else {
                        // Not a CSI (could be another ESC, OSC, etc.).  Re-arm on
                        // a fresh ESC, otherwise fall back to normal text.
                        self.csi_state = if b == 0x1b { CsiState::Esc } else { CsiState::Normal };
                    }
                    out.push(b);
                }
                CsiState::Csi => {
                    // Final bytes are 0x40..=0x7e; params/intermediates are
                    // 0x20..=0x3f.  Rewrite an `f` final into `H`.
                    if (0x40..=0x7e).contains(&b) {
                        out.push(if b == b'f' { b'H' } else { b });
                        self.csi_state = CsiState::Normal;
                    } else {
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    /// Process one bounded batch and capture any lines that scrolled off the top
    /// (skipped for alt-screen programs like vim/nano).
    fn ingest_chunk(&mut self, bytes: &[u8]) {
        // Detect full-screen-clear sequences *before* processing so we can
        // suppress history for programs that redraw without alt-screen (e.g.
        // btop configured with `alt-screen = false`).
        // We look for \033[H (cursor-home) and \033[2J / \033[J (erase display)
        // as indicators that the program is doing a full-screen refresh.
        let has_cursor_home   = bytes.windows(3).any(|w| w == b"\x1b[H");
        let has_erase_display = bytes.windows(4).any(|w| w == b"\x1b[2J")
                             || bytes.windows(3).any(|w| w == b"\x1b[J");
        let is_fullscreen_refresh = has_cursor_home && has_erase_display;

        self.parser.process(bytes);
        let (is_alt, rows, cols) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            (s.alternate_screen(), r, c)
        };
        if is_alt {
            // Snap to live view whenever we're on the alt screen — this
            // prevents old history (accumulated before alt-screen was entered)
            // from mixing with the full-screen program's output after a scroll.
            self.view_offset = 0;
            self.prev.clear();
            return;
        }
        if is_fullscreen_refresh {
            // Non-alt-screen full-screen refresh (btop, htop with alt disabled…).
            // Don't capture lines into history; they'd mix with the next frame.
            self.view_offset = 0;
            self.prev.clear();
            return;
        }
        let curr: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        if !self.prev.is_empty() {
            let k = detect_scroll(&self.prev, &curr);
            for line in self.prev.iter().take(k) {
                self.history.push(line.clone());
            }
            if self.history.len() > MAX_HISTORY {
                let drop = self.history.len() - MAX_HISTORY;
                self.history.drain(0..drop);
            }
        }
        self.prev = curr;
    }

    /// Render the terminal grid for the current scrollback `view_offset`
    /// (0 = live).  Caches the displayed plain text for find/selection.
    fn render(&mut self) -> BuiltScreen {
        let (is_alt, rows, cols, cur_row, cur_col) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            let (cr, cc) = s.cursor_position();
            (s.alternate_screen(), r, c, cr, cc)
        };

        // --- Live view (also alt-screen): render the current grid -----------
        if is_alt || self.view_offset == 0 {
            let mut spans = Vec::new();
            let mut displayed = Vec::with_capacity(rows as usize);
            let mut last_content = 0i32;
            let s = self.parser.screen();
            for r in 0..rows {
                let (plain, runs) = build_row(s, r, cols);
                if !runs.is_empty() {
                    last_content = r as i32;
                }
                for hs in runs {
                    spans.push(TermSpan {
                        text: hs.text.into(),
                        fg: hs.fg,
                        bg: hs.bg,
                        bold: hs.bold,
                        row: r as i32,
                        col: hs.col,
                        cells: hs.cells,
                    });
                }
                displayed.push(plain.trim_end().to_string());
            }
            self.displayed_text = displayed;
            let rows_used = if is_alt { rows as i32 } else { last_content + 1 };
            return BuiltScreen {
                spans,
                cursor_row: cur_row as i32,
                cursor_col: cur_col as i32,
                rows_used,
                is_alt,
            };
        }

        // --- Scrolled view: window into history ++ live content -------------
        let live: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        let live_used = live
            .iter()
            .rposition(|(_, r)| !r.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        let hist_len = self.history.len();
        let combined_len = hist_len + live_used;
        let win = rows as usize;
        let start = combined_len.saturating_sub(win + self.view_offset);
        let end = (start + win).min(combined_len);

        let mut spans = Vec::new();
        let mut displayed = Vec::with_capacity(win);
        for (d, idx) in (start..end).enumerate() {
            let line: &Line = if idx < hist_len {
                &self.history[idx]
            } else {
                &live[idx - hist_len]
            };
            for hs in &line.1 {
                spans.push(TermSpan {
                    text: hs.text.clone().into(),
                    fg: hs.fg,
                    bg: hs.bg,
                    bold: hs.bold,
                    row: d as i32,
                    col: hs.col,
                    cells: hs.cells,
                });
            }
            displayed.push(line.0.trim_end().to_string());
        }
        while displayed.len() < win {
            displayed.push(String::new());
        }
        self.displayed_text = displayed;
        BuiltScreen {
            spans,
            cursor_row: -1, // hide the live cursor while viewing history
            cursor_col: 0,
            rows_used: win as i32,
            is_alt: false,
        }
    }
}

/// Standard 16-colour ANSI palette (VS Code "Dark+" values — reads well on the
/// dark terminal background).
const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0 black
    (0xcd, 0x31, 0x31), // 1 red
    (0x0d, 0xbc, 0x79), // 2 green
    (0xe5, 0xe5, 0x10), // 3 yellow
    (0x24, 0x72, 0xc8), // 4 blue
    (0xbc, 0x3f, 0xbc), // 5 magenta
    (0x11, 0xa8, 0xcd), // 6 cyan
    (0xe5, 0xe5, 0xe5), // 7 white
    (0x66, 0x66, 0x66), // 8 bright black
    (0xf1, 0x4c, 0x4c), // 9 bright red
    (0x23, 0xd1, 0x8b), // 10 bright green
    (0xf5, 0xf5, 0x43), // 11 bright yellow
    (0x3b, 0x8e, 0xea), // 12 bright blue
    (0xd6, 0x70, 0xd6), // 13 bright magenta
    (0x29, 0xb8, 0xdb), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white
];

/// Convert a vt100 colour (+ bold) to a Slint colour.  Bold + a base colour
/// (0–7) maps to the bright variant (8–15), matching how terminals render
/// `ls --color` (e.g. bold-green executables, bold-blue directories).
fn vt_color_to_slint(color: vt100::Color, bold: bool) -> slint::Color {
    let (r, g, b) = match color {
        vt100::Color::Default => (0xd4, 0xd4, 0xd4), // Theme.term-fg
        vt100::Color::Idx(i) => idx_to_rgb(i, bold),
        vt100::Color::Rgb(r, g, b) => (r, g, b),
    };
    slint::Color::from_rgb_u8(r, g, b)
}

/// Convert a vt100 *background* colour to Slint.  The default background maps to
/// fully transparent so we don't paint a fill over the terminal's own bg (and
/// can cheaply skip drawing it).  Non-default backgrounds (btop/htop bars,
/// selected rows, meter fills) become opaque colours.
fn vt_bg_to_slint(color: vt100::Color) -> slint::Color {
    match color {
        vt100::Color::Default => slint::Color::from_argb_u8(0, 0, 0, 0), // transparent
        vt100::Color::Idx(i) => {
            let (r, g, b) = idx_to_rgb(i, false);
            slint::Color::from_rgb_u8(r, g, b)
        }
        vt100::Color::Rgb(r, g, b) => slint::Color::from_rgb_u8(r, g, b),
    }
}

/// Map an xterm-256 palette index to RGB (16 ANSI + 6×6×6 cube + grayscale).
fn idx_to_rgb(i: u8, bold: bool) -> (u8, u8, u8) {
    let i = if bold && i < 8 { i + 8 } else { i };
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = i - 16;
            let to = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            (to(n / 36), to((n % 36) / 6), to(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Return the parent directory of `path`.
/// "/a/b/c" → "/a/b", "/a" → "/", "/" → "/"
fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".to_string(),
    }
}
