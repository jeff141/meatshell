//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
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
    /// Current theme mode — propagated from the global dark-mode toggle.
    /// Stored here so the event-pump threads can render new output with the
    /// correct palette without needing a window reference.
    is_dark: bool,
    /// Drag selection in ABSOLUTE scrollback coordinates: each endpoint is a
    /// `(combined_row, col)` where `combined_row` indexes the virtual buffer of
    /// `history` lines followed by the live screen rows.  Absolute (rather than
    /// visible-window) coordinates keep the selection pinned to its content
    /// while the view auto-scrolls during a drag, so a top-to-bottom selection
    /// across more than one screen of scrollback copies every line (#18).
    /// `anchor` = where the drag began, `focus` = the moving end.
    sel_anchor: Option<(usize, u16)>,
    sel_focus: Option<(usize, u16)>,
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

use crate::config::{AuthMethod, ConfigStore, Secret, Session, SessionKind};
use crate::i18n::t;
use crate::sftp::{spawn_sftp, SftpHandle};
use crate::ssh::{
    format_mtime, format_size, spawn_session, ProcInfo, SessionCommand, SessionEvent,
    SessionHandle,
};
use crate::system::{format_bytes_per_sec, format_mem, SystemSampler, SystemSnapshot};

type SftpHandles = Arc<Mutex<HashMap<String, SftpHandle>>>;
/// Per-tab flag: once the user explicitly navigates via the SFTP tree or
/// toolbar, stop auto-syncing to the terminal's `cd` path.
/// Per-tab last cwd the SFTP panel followed (from OSC 7). Used to ignore the
/// OSC 7 every prompt re-emits at an unchanged directory; manual SFTP
/// navigation REMOVES the entry so the very next OSC 7 — same directory or
/// not — snaps the panel back to the shell's cwd (cd-follow never goes stale).
type SftpLastCwd = Arc<Mutex<HashMap<String, String>>>;

/// Per-tab connection status + latest remote resource sample, used to drive the
/// sidebar for whichever tab is active.  `Arc<Mutex>` because the SSH event-pump
/// threads update it before bouncing to the UI thread.
#[derive(Clone, Default)]
struct TabStatus {
    host: String,       // "root@192.168.100.2"
    session_id: String, // saved-session id, used to reconnect in place (#79)
    state: u8,          // 0 = connecting, 1 = connected, 2 = disconnected
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
    /// Top remote processes by CPU, for the process monitor popup (#23).
    procs: Vec<ProcInfo>,
}
type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
/// Last local-machine sample (shown on the welcome tab).
type LocalSnap = Arc<Mutex<SystemSnapshot>>;

// Slint generates types into this scope.
slint::include_modules!();

/// Number of samples kept for the sparkline.
const NET_HISTORY_LEN: usize = 60;

/// Embed the app icon PNG into the binary and set it as the X11 window icon.
///
/// On X11, the taskbar/dock icon for a running window comes from the
/// `_NET_WM_ICON` property, which winit sets via `Window::set_window_icon`.
/// When the app runs as a bare AppImage (or from a plain directory without
/// running install-linux.sh) there is no installed .desktop + icon, so the
/// dock falls back to a generic gear.  This call fixes that for X11 sessions.
///
/// On Wayland the dock icon is resolved by the compositor from the XDG
/// app-id → .desktop file mapping; `set_window_icon` is a no-op there, so
/// Wayland users still need AppImageLauncher or install-linux.sh for the
/// dock icon.  The `icon:` property in app.slint handles the in-title-bar
/// icon on both backends without any runtime work.
///
/// Windows gets its icon from the `.ico` embedded by winresource at link
/// time; macOS from the app bundle — neither path needs runtime decoding.
#[cfg(target_os = "linux")]
fn set_window_icon(window: &AppWindow) {
    use i_slint_backend_winit::winit::window::Icon;
    const ICON_PNG: &[u8] = include_bytes!("../assets/icon@512.png");
    let Ok(img) = image::load_from_memory(ICON_PNG) else { return };
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let Ok(icon) = Icon::from_rgba(rgba.into_raw(), w, h) else { return };
    window.window().with_winit_window(|ww| ww.set_window_icon(Some(icon)));
}

pub fn run() -> Result<()> {
    // --- Runtime + store -------------------------------------------------
    let runtime = Arc::new(
        Runtime::new().context("failed to start tokio runtime")?,
    );
    let store = Rc::new(RefCell::new(
        ConfigStore::load().context("failed to load config")?,
    ));
    // Reachable from the Slint-thread event handler for recording terminal
    // commands into history (#113).
    HISTORY_STORE.with(|s| *s.borrow_mut() = Some(store.clone()));

    // Per-tab SSH handles (shell only; lives on Slint thread via Rc).
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Per-tab SFTP handles — Arc<Mutex> so the event-pump OS thread and the
    // Slint UI thread can both post SftpCommands.
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    // Per-tab cwd the SFTP panel last followed (see SftpLastCwd).
    let sftp_last_cwd: SftpLastCwd = Arc::new(Mutex::new(HashMap::new()));

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

    // Show the crate version (from Cargo.toml at compile time) in the sidebar,
    // so the footer never drifts out of sync with the actual build.
    window.set_app_version(env!("CARGO_PKG_VERSION").into());

    // Set the window icon from the PNG embedded in the binary so the dock
    // shows the correct icon even without a system-installed .desktop entry
    // (e.g. AppImage without AppImageLauncher, or plain binary in ~/bin).
    #[cfg(target_os = "linux")]
    set_window_icon(&window);

    // The window defaults to frameless + custom title bar (#119). macOS keeps
    // its native decorations, so turn the custom bar off there.
    #[cfg(target_os = "macos")]
    window.set_custom_titlebar(false);

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    // Apply the saved (or system-detected) theme.
    // "dark" / "light" → use that directly; "system" or unset → ask the OS;
    // OS unknown → fall back to dark.
    {
        let is_dark = match store.borrow().theme_pref() {
            "light" => false,
            "dark"  => true,
            _       => match dark_light::detect() {
                dark_light::Mode::Light   => false,
                dark_light::Mode::Dark    => true,
                dark_light::Mode::Default => true, // undetectable → dark
            },
        };
        window.set_dark_mode(is_dark);
    }

    // Apply the saved terminal font (Interface settings). An empty family keeps
    // the built-in default; the size always applies (defaults to 13).
    {
        let s = store.borrow();
        let fam = s.font_family().to_string();
        if !fam.is_empty() {
            window.set_term_font_family(fam.into());
        }
        window.set_term_font_size(s.font_size() as f32);
    }
    // Editable inputs (e.g. the SFTP path bar) need a CJK-capable font: the
    // embedded mono font has no Chinese glyphs and native TextInput doesn't
    // glyph-fallback like Text does, so typed Chinese would render as tofu (#54).
    #[cfg(target_os = "windows")]
    window.set_ui_font_family("Microsoft YaHei UI".into());
    #[cfg(target_os = "macos")]
    window.set_ui_font_family("PingFang SC".into());
    // Linux: leave the Slint default (Noto Sans CJK is typically installed).
    // Populate the Interface font picker with installed monospace families.
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(system_monospace_fonts()))));

    // Command bar (#55): seed quick commands + history from the config.
    window.set_quick_commands(quick_cmd_model(&store.borrow()));
    window.set_command_history(history_model(&store.borrow()));

    // Interface setting: SFTP follows the terminal's cd. The shell event pumps
    // read this AtomicBool on every CwdChanged, so toggling applies live to
    // already-open sessions too.
    let sftp_follow_cd = Arc::new(std::sync::atomic::AtomicBool::new(
        store.borrow().sftp_follow_cd(),
    ));
    window.set_sftp_follow_cd(store.borrow().sftp_follow_cd());
    {
        let store = store.clone();
        let flag = sftp_follow_cd.clone();
        window.on_set_sftp_follow_cd(move |follow| {
            flag.store(follow, std::sync::atomic::Ordering::Relaxed);
            let mut s = store.borrow_mut();
            s.set_sftp_follow_cd(follow);
            let _ = s.save();
        });
    }

    // Interface setting: always ask where to save on download (#87). Read live
    // by the download handler from the window property, so just set + persist.
    window.set_download_always_ask(store.borrow().download_always_ask());
    {
        let store = store.clone();
        window.on_set_download_always_ask(move |ask| {
            let mut s = store.borrow_mut();
            s.set_download_always_ask(ask);
            let _ = s.save();
        });
    }

    // Interface setting: collapse the sidebars by default (#78). Seed the
    // checkboxes, apply the collapsed state once at startup, and persist toggles.
    {
        let s = store.borrow();
        let collapse_sidebar = s.collapse_sidebar_default();
        let collapse_sftp = s.collapse_sftp_default();
        window.set_collapse_sidebar_default(collapse_sidebar);
        window.set_collapse_sftp_default(collapse_sftp);
        if collapse_sidebar {
            window.set_sidebar_collapsed(true);
        }
        if collapse_sftp {
            window.set_sftp_collapsed(true);
            window.set_sftp_saved_height(220.0);
            window.set_sftp_panel_height(30.0);
        }
    }
    {
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sidebar_default(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sftp_default(v);
            let _ = s.save();
        });
    }

    // Session-sync upload setting (#sync). Persisted; only has effect while the
    // session-sync toggle is on. Read live from the window in the upload handler.
    window.set_sync_upload_enabled(store.borrow().sync_upload());
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_sync_upload(v);
            let _ = s.save();
        });
    }

    // Interface settings: apply + persist the terminal font family / size.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font(move |family: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_family(family);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_size(move |size: i32| {
            {
                let mut s = store.borrow_mut();
                s.set_font_size(size as u32);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_size(size as f32);
            }
        });
    }

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
        sftp_last_cwd.clone(),
        tab_statuses.clone(),
        local_snap.clone(),
        local_net_hist.clone(),
        sftp_follow_cd.clone(),
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

    // Theme toggle: flip dark ↔ light, persist the preference, and re-render
    // every open terminal with the new ANSI palette so historical output is
    // also recoloured (not just new output).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_theme = bufs.clone();
        window.on_toggle_theme(move || {
            let Some(w) = weak.upgrade() else { return };
            let next_dark = !w.get_dark_mode();
            w.set_dark_mode(next_dark);
            // Propagate new palette to all open terminal buffers.
            {
                let mut map = bufs_theme.lock().unwrap();
                for buf in map.values_mut() {
                    buf.is_dark = next_dark;
                }
            }
            // Re-render every visible terminal so colours update immediately.
            let tab_ids: Vec<String> = {
                let map = bufs_theme.lock().unwrap();
                map.keys().cloned().collect()
            };
            for tid in tab_ids {
                rebuild_tab_display(&w, &bufs_theme, &tid);
            }
            let pref = if next_dark { "dark" } else { "light" };
            let mut s = store.borrow_mut();
            s.set_theme_pref(pref.to_string());
            let _ = s.save();
        });
    }

    // Host-key confirmation dialog (#109-5): the user trusts or rejects the
    // presented server key; the decision fans back out to the blocked SSH/SFTP
    // handler(s) and the next queued prompt (if any) is shown.
    {
        let weak = window.as_weak();
        window.on_hostkey_accept(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_hostkey_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, false);
            }
        });
    }

    // Connect-time credential prompt (#110): the user supplies the missing
    // username/password (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        window.on_cred_accept(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_cred(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_cred_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_cred(&w, false);
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
    // Default to the user's Downloads folder so files land somewhere sensible
    // without a prompt; only fall back to "ask every time" if we can't locate it
    // (#85). Persist it on first run so the setting reflects the real path.
    if store.borrow().download_dir().is_empty() {
        if let Some(dl) = directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(|p| p.to_string_lossy().to_string()))
        {
            let mut s = store.borrow_mut();
            s.set_download_dir(dl);
            let _ = s.save();
        }
    }
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

    // --- In-app update check (#48) -----------------------------------------
    // "Download" on the banner opens the latest-release page in the browser.
    window.on_open_update_url(move || {
        let url = "https://github.com/jeff141/meatshell/releases/latest";
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(url).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    });
    // Query the GitHub releases API on a background thread; if a newer version
    // exists, flip the banner on. Best-effort: any network/parse error is
    // silently ignored and the app keeps working on the current version.
    {
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let body = match ureq::get(
                "https://api.github.com/repos/jeff141/meatshell/releases/latest",
            )
            .set("User-Agent", "meatshell-update-check")
            .timeout(std::time::Duration::from_secs(8))
            .call()
            {
                Ok(resp) => resp.into_string().unwrap_or_default(),
                Err(_) => return,
            };
            let json: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => return,
            };
            let tag = json["tag_name"].as_str().unwrap_or("").to_string();
            let newer = matches!(
                (parse_version(&tag), parse_version(env!("CARGO_PKG_VERSION"))),
                (Some(latest), Some(cur)) if latest > cur
            );
            if !newer {
                return;
            }
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.set_update_version(tag.into());
                w.set_update_available(true);
            });
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
        sftp_last_cwd.clone(),
    );
    wire_sftp_callbacks(&window, sftp_handles.clone(), sftp_last_cwd.clone());
    wire_key_input(
        &window,
        handles.clone(),
        bufs.clone(),
        last_term_size.clone(),
        store.clone(),
        ConnectCtx {
            weak: window.as_weak(),
            runtime: runtime.clone(),
            handles: handles.clone(),
            sftp_handles: sftp_handles.clone(),
            sftp_last_cwd: sftp_last_cwd.clone(),
            bufs: bufs.clone(),
            tab_statuses: tab_statuses.clone(),
            local_snap: local_snap.clone(),
            local_net_hist: local_net_hist.clone(),
            last_term_size: last_term_size.clone(),
            sftp_follow_cd: sftp_follow_cd.clone(),
        },
    );

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
        let close_handles = handles.clone();
        window.window().on_winit_window_event(move |_w, event| {
            match event {
                WEvent::DroppedFile(path) => {
                    if let Some(win) = weak.upgrade() {
                        handle_file_drop(&win, &sh, path.to_string_lossy().to_string());
                    }
                }
                WEvent::Resized(_) => {
                    // Keep the maximize/restore icon (and resize-edge gating) in
                    // sync when the OS changes the window state (#119).
                    if let Some(win) = weak.upgrade() {
                        let maxed = win
                            .window()
                            .with_winit_window(|ww| ww.is_maximized())
                            .unwrap_or(false);
                        win.set_window_maximized(maxed);
                    }
                }
                WEvent::CloseRequested => {
                    // Confirm before closing if there are open session tabs (#88),
                    // so a stray double-click on the title-bar icon / X / Alt+F4
                    // doesn't silently drop live sessions. The confirm dialog's
                    // "Close" calls quit_event_loop to actually exit.
                    if !close_handles.borrow().is_empty() {
                        if let Some(win) = weak.upgrade() {
                            win.set_confirm_close_open(true);
                        }
                        return EventResult::PreventDefault;
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }
    // Confirm-close dialog "Close" → actually quit the event loop (#88).
    window.on_confirm_close_yes(|| {
        let _ = slint::quit_event_loop();
    });

    // --- Custom title-bar window controls (#119) --------------------------
    {
        let weak = window.as_weak();
        window.on_win_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_maximize_toggle(move || {
            if let Some(w) = weak.upgrade() {
                let now = w.window().with_winit_window(|ww| {
                    let m = !ww.is_maximized();
                    ww.set_maximized(m);
                    m
                });
                if let Some(m) = now {
                    w.set_window_maximized(m);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let close_handles = handles.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if close_handles.borrow().is_empty() {
                    let _ = slint::quit_event_loop();
                } else {
                    w.set_confirm_close_open(true);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = window.as_weak();
        window.on_win_resize(move |dir: i32| {
            if let Some(w) = weak.upgrade() {
                let d = match dir {
                    0 => ResizeDirection::North,
                    1 => ResizeDirection::South,
                    2 => ResizeDirection::East,
                    3 => ResizeDirection::West,
                    4 => ResizeDirection::NorthEast,
                    5 => ResizeDirection::NorthWest,
                    6 => ResizeDirection::SouthEast,
                    _ => ResizeDirection::SouthWest,
                };
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(d);
                });
            }
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
    // When the SFTP panel is minimised (toolbar only, ~30px tall) there is no
    // usable file-list area — silently ignore the drop.
    if win.get_sftp_collapsed() {
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

    // File-list box (logical): the SFTP tree pane is always 160px; the sidebar
    // collapses to 0px when hidden.  The 1px accounts for the vertical
    // separator between tree and file list.
    let sidebar_w = if win.get_sidebar_collapsed() { 0.0 } else { 220.0 };
    let zone_left = sidebar_w + 161.0; // sidebar + tree(160) + sep(1)
    let zone_top = h_logical - h_sftp + 51.0;   // toolbar(30)+col-hdr(20)+sep(1)
    let zone_bottom = h_logical - 18.0;          // above SFTP status bar
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
    // Session-sync (#sync): when both toggles are on, also mirror the drop to
    // every other online session — each into *its own* current SFTP dir. This
    // matches the upload button's behaviour (drag-and-drop is a separate path).
    let sync = win.get_sync_input() && win.get_sync_upload_enabled();
    let other_dirs = if sync { terminal_sftp_paths(win) } else { HashMap::new() };
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(&active) {
            h.upload(path.clone(), dir);
        }
        if sync {
            for (id, h) in handles.iter() {
                if id == &active {
                    continue;
                }
                if let Some(d) = other_dirs.get(id).filter(|d| !d.is_empty()) {
                    h.upload(path.clone(), d.clone());
                }
            }
        }
    }
    // Pop the transfers panel so upload progress is visible (mirrors the
    // behaviour already in place for downloads).
    win.set_download_open(true);
}

#[cfg(not(windows))]
fn handle_file_drop(_win: &AppWindow, _sftp_handles: &SftpHandles, _path: String) {}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

fn sync_sessions_to_model(store: &ConfigStore, model: &VecModel<SessionInfo>) {
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
    sftp_last_cwd: SftpLastCwd,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
) {
    // Working set of port forwards (#56) for the session being created/edited.
    // The forward add/delete callbacks mutate it; saving reads it into
    // Session.forwards; opening the dialog (new/edit) resets it.
    let edit_forwards: Rc<RefCell<Vec<crate::config::PortForward>>> =
        Rc::new(RefCell::new(Vec::new()));

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            ef_new.borrow_mut().clear();
            w.set_dialog_forwards(forward_model(&[]));
            let empty = Session::new_empty();
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
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() { "root".into() } else { h.user },
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
            let Some(session) = store.get(&id) else { return; };
            *ef_edit.borrow_mut() = session.forwards.clone();
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&session.forwards));
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
                let (proxy_type, proxy_hostport) = split_proxy(&session.proxy);
                w.set_dialog_proxy_type(proxy_type.into());
                w.set_dialog_proxy_hostport(proxy_hostport.into());
                w.set_dialog_group(session.group.clone().into());
                w.set_dialog_kind(session.kind.as_str().into());
                w.set_dialog_serial_port(session.serial_port.clone().into());
                w.set_dialog_baud(session.baud_rate.to_string().into());
                w.set_dialog_data_bits(session.data_bits.to_string().into());
                w.set_dialog_stop_bits(session.stop_bits.to_string().into());
                w.set_dialog_parity(session.parity.clone().into());
                w.set_dialog_flow(session.flow_control.clone().into());
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
                private_key_path: draft.private_key_path.to_string().replace('\\', "/"),
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
                forwards: edit_forwards.borrow().clone(),
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

    // Add a port forward to the session being edited (#56).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_add_forward(
            move |kind: SharedString,
                  bind_addr: SharedString,
                  bind_port: i32,
                  host: SharedString,
                  host_port: i32| {
                let kind = kind.to_string();
                // Local/remote need a target host; dynamic doesn't.
                if bind_port <= 0 || bind_port > 65535 {
                    return;
                }
                if kind != "dynamic" && (host.trim().is_empty() || host_port <= 0) {
                    return;
                }
                ef.borrow_mut().push(crate::config::PortForward {
                    kind,
                    bind_addr: bind_addr.trim().to_string(),
                    bind_port: bind_port as u16,
                    host: host.trim().to_string(),
                    host_port: host_port.max(0) as u16,
                });
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_forwards(forward_model(&ef.borrow()));
                }
            },
        );
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
        let handles = handles.clone();
        let bufs = bufs.clone();
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
            let session = match store.borrow().get(&id).cloned() {
                Some(s) => s,
                None => return,
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
                    session_id: id.clone(),
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
                sftp_status: if has_sftp {
                    t("SFTP 连接中...", "SFTP connecting...").into()
                } else {
                    t("此会话类型不支持 SFTP", "SFTP not available for this session").into()
                },
                sftp_loading: has_sftp,
                sftp_tree_nodes: ModelRc::from(
                    std::rc::Rc::new(VecModel::<SftpTreeNode>::default()),
                ),
            });
            // Create vt100 parser for this tab (default 24×80; resized on first
            // terminal-resize callback). 5000-line scrollback is stored for
            // future scroll-navigation support.
            let is_dark_now = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            bufs.lock().unwrap().insert(
                tab_id.clone(),
                TermBuffer {
                    parser: vt100::Parser::new(24, 80, 5000),
                    find_query: String::new(),
                    is_dark: is_dark_now,
                    sel_anchor: None,
                    sel_focus: None,
                    history: Vec::new(),
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                },
            );
            // No followed-cwd yet: the first OSC 7 always triggers a follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            if let Some(w) = weak.upgrade() {
                w.set_active_tab_id(tab_id.clone().into());
            }

            // Spawn the shell (+ SFTP) workers and their event-pump threads.
            // Shared with in-place reconnect (#79) via start_session_in_tab.
            let ctx = ConnectCtx {
                weak: weak.clone(),
                runtime: runtime.clone(),
                handles: handles.clone(),
                sftp_handles: sftp_handles.clone(),
                sftp_last_cwd: sftp_last_cwd.clone(),
                bufs: bufs.clone(),
                tab_statuses: tab_statuses.clone(),
                local_snap: local_snap.clone(),
                local_net_hist: local_net_hist.clone(),
                last_term_size: last_term_size.clone(),
                sftp_follow_cd: sftp_follow_cd.clone(),
            };
            start_session_in_tab(&tab_id, session, &ctx);
        });
    }
}

type NetHist = Arc<Mutex<Vec<f32>>>;

/// Shared connection dependencies for `start_session_in_tab`. All fields are
/// cheap clones (Arc / Weak / Rc), so connect and in-place reconnect can both
/// build one and spawn workers for a tab (#79).
struct ConnectCtx {
    weak: slint::Weak<AppWindow>,
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
    bufs: TermBuffers,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    last_term_size: Arc<Mutex<(u32, u32)>>,
    /// Interface setting: SFTP panel follows the terminal's cd (OSC 7).
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
}

/// Spawn the shell (+ SFTP) workers and their event-pump threads for an
/// already-registered tab. Used by the initial connect and by in-place
/// reconnect (#79); the tab/terminal/parser must already exist.
fn start_session_in_tab(tab_id: &str, session: Session, ctx: &ConnectCtx) {
    let has_sftp = session.kind == SessionKind::Ssh;
    let (initial_cols, initial_rows) = *ctx.last_term_size.lock().unwrap();
    let (handle, rx) = match session.kind {
        SessionKind::Ssh => spawn_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
        SessionKind::Serial => crate::serial::spawn_serial_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
        ),
        SessionKind::Telnet => crate::telnet::spawn_telnet_session(
            ctx.runtime.handle(),
            tab_id.to_string(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
    };
    ctx.handles.borrow_mut().insert(tab_id.to_string(), handle);

    // Separate SFTP connection for the same session (SSH only).
    let sftp_evt_tx = if has_sftp {
        let (sftp_tx, sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let sftp_handle = spawn_sftp(ctx.runtime.handle(), session, sftp_tx);
        ctx.sftp_handles
            .lock()
            .unwrap()
            .insert(tab_id.to_string(), sftp_handle);
        Some(sftp_rx)
    } else {
        None
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
        std::thread::spawn(move || {
            let mut shell_rx = rx;
            let mut cwd_debounce: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                match shell_rx.blocking_recv() {
                    None => break,
                    Some(shell_evt) => {
                        if let SessionEvent::CwdChanged(ref cwd) = shell_evt {
                            // Shared map (not a thread-local) so manual SFTP
                            // navigation can clear the entry — then the very
                            // next OSC 7, same directory or not, snaps the
                            // panel back to the shell's cwd. Unchanged repeats
                            // (every prompt re-emits OSC 7) are ignored (#59).
                            let changed = match sftp_last_cwd_pump.lock() {
                                Ok(mut m) => {
                                    m.insert(tab_id_pump.clone(), cwd.clone())
                                        .as_deref()
                                        != Some(cwd.as_str())
                                }
                                Err(_) => false,
                            };
                            // Swallow the event entirely when follow-cd is off:
                            // forwarding it would set sftp_loading without any
                            // ListDir to clear it (the #59 stuck-"loading" trap).
                            if !changed
                                || !follow_cd_pump
                                    .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                continue;
                            }
                            if let Some(prev) = cwd_debounce.take() {
                                prev.abort();
                            }
                            let cwd = cwd.clone();
                            let sftp_h = sftp_handles_pump.clone();
                            let tid = tab_id_pump.clone();
                            cwd_debounce = Some(rt_pump.spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                if let Ok(handles) = sftp_h.lock() {
                                    if let Some(h) = handles.get(&tid) {
                                        h.list_dir(cwd);
                                    }
                                }
                            }));
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
                                    &win, &tid, shell_evt, &bufs_evt, &st_evt, &lc_evt, &nh_evt,
                                );
                            }
                        });
                    }
                }
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
                                    &win, &tid, sftp_evt, &bufs_s, &st_s, &lc_s, &nh_s,
                                );
                            }
                        });
                    }
                }
            }
        });
    }
}

/// Map of tab-id → the SFTP panel's current path, read from the terminals
/// model. Used as the per-session fallback dir for session-sync uploads.
fn terminal_sftp_paths(w: &AppWindow) -> HashMap<String, String> {
    use slint::Model as _;
    let mut out = HashMap::new();
    let model = w.get_terminals();
    if let Some(terminals) = model.as_any().downcast_ref::<VecModel<TerminalState>>() {
        for i in 0..terminals.row_count() {
            if let Some(row) = terminals.row_data(i) {
                out.insert(row.id.to_string(), row.sftp_path.to_string());
            }
        }
    }
    out
}

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

/// Build the process-monitor model for the popup (#23). `cpu`/`mem` are
/// pre-formatted to one decimal; `cpu_frac` (0..1) drives the row's load bar.
fn proc_model(procs: &[ProcInfo]) -> ModelRc<ProcRow> {
    let rows: Vec<ProcRow> = procs
        .iter()
        .map(|p| ProcRow {
            pid: p.pid.to_string().into(),
            user: p.user.clone().into(),
            cpu: format!("{:.1}", p.cpu).into(),
            mem: format!("{:.1}", p.mem).into(),
            command: p.command.clone().into(),
            cpu_frac: (p.cpu / 100.0).clamp(0.0, 1.0),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Build the quick-command model for the command bar + manage dialog (#55).
fn quick_cmd_model(store: &ConfigStore) -> ModelRc<QuickCmd> {
    let rows: Vec<QuickCmd> = store
        .quick_commands()
        .iter()
        .map(|q| QuickCmd {
            name: q.name.clone().into(),
            command: q.command.clone().into(),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Build the port-forward list model for the session dialog (#56). Each row is
/// a one-line human summary (`-L 127.0.0.1:8080 → host:80`).
fn forward_model(forwards: &[crate::config::PortForward]) -> ModelRc<PortFwd> {
    let rows: Vec<PortFwd> = forwards
        .iter()
        .map(|f| {
            let bind = if f.bind_addr.trim().is_empty() {
                "127.0.0.1"
            } else {
                f.bind_addr.trim()
            };
            let summary = match f.kind.as_str() {
                "local" => format!("-L {}:{} → {}:{}", bind, f.bind_port, f.host, f.host_port),
                "remote" => format!("-R {}:{} → {}:{}", bind, f.bind_port, f.host, f.host_port),
                "dynamic" => format!("-D {}:{} (SOCKS5)", bind, f.bind_port),
                _ => String::new(),
            };
            PortFwd {
                kind: f.kind.clone().into(),
                summary: summary.into(),
            }
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Build the command-history model in storage order (oldest first, newest
/// last). The dropdown shows the most-recently-used command at the bottom
/// (nearest the input) and ↑ recalls it first (#55, #113).
fn history_model(store: &ConfigStore) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .map(|s| s.clone().into())
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
        let sel = buf.selection_rects_visible(cols);
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
        win.set_mem_detail(format_mem(snap.mem_used_mib, snap.mem_total_mib).into());
        win.set_swap_detail(format_mem(snap.swap_used_mib, snap.swap_total_mib).into());
    };
    let clear_stats = |win: &AppWindow| {
        win.set_cpu_percent(0.0);
        win.set_mem_percent(0.0);
        win.set_swap_percent(0.0);
        win.set_mem_detail("".into());
        win.set_swap_detail("".into());
    };

    // Process monitor (#23) only applies to a live remote session; default to
    // hidden/empty and let the connected branch below fill it in.
    win.set_proc_available(false);
    win.set_proc_list(ModelRc::from(Rc::new(VecModel::<ProcRow>::default())));

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
                format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
            );
            win.set_swap_detail(
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
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
            win.set_proc_available(true);
            win.set_proc_list(proc_model(&st.procs));
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
                    let sel = buf.selection_rects_visible(cols);
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
                statuses,
                local,
                local_net_hist,
            );
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
            procs,
        } => {
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.cpu = cpu_percent;
                st.mem_used_kib = mem_used_kib;
                st.mem_total_kib = mem_total_kib;
                st.swap_used_kib = swap_used_kib;
                st.swap_total_kib = swap_total_kib;
                st.net = net;
                st.disks = disks;
                st.procs = procs;
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
                    mode: (e.mode & 0o7777) as i32,
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
                2 => if msg.is_empty() { t("失败", "Failed").to_string() } else { msg },
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
            enqueue_cred_prompt(win, session_id, host, user, need_user, need_password, responder);
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
    static HISTORY_STORE: RefCell<Option<Rc<RefCell<ConfigStore>>>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Host-key confirmation (#109-5)
// ---------------------------------------------------------------------------

/// One queued host-key prompt. Multiple connections to the *same* host:port
/// (e.g. the shell and its SFTP channel racing on first connect) collapse into
/// a single dialog whose answer fans out to every waiting `responder`.
struct PendingHostKey {
    host: String,
    port: u16,
    changed: bool,
    title: String,
    message: String,
    detail: String,
    confirm_label: String,
    responders: Vec<crate::ssh::HostKeyResponder>,
}

thread_local! {
    /// Prompts awaiting a decision; the front one is shown. Lives on the Slint
    /// event-loop thread (all access is from there).
    static HOSTKEY_QUEUE: RefCell<VecDeque<PendingHostKey>> = RefCell::new(VecDeque::new());
    /// host:port → decision, remembered for this run so a duplicate prompt
    /// (second connection to the same host) is answered without a new dialog.
    static HOSTKEY_DECIDED: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

/// Localized title / message / detail / confirm-label for the host-key dialog.
fn hostkey_dialog_text(
    host: &str,
    port: u16,
    key_type: &str,
    fingerprint: &str,
    changed: bool,
) -> (String, String, String, String) {
    let detail = format!("{host}:{port}  ({key_type})\n{fingerprint}");
    if changed {
        (
            crate::i18n::t("⚠ 主机密钥已改变", "⚠ Host key changed").to_string(),
            crate::i18n::t(
                "该主机的密钥与之前记录的不一致,可能存在中间人攻击。仅当你确知服务器密钥已更换时才继续。",
                "This host's key differs from the one stored earlier — this could be a man-in-the-middle attack. Only continue if you know the server's key really changed.",
            )
            .to_string(),
            detail,
            crate::i18n::t("仍然信任", "Trust anyway").to_string(),
        )
    } else {
        (
            crate::i18n::t("未知主机", "Unknown host").to_string(),
            crate::i18n::t(
                "首次连接该主机。请核对下面的密钥指纹,确认无误后再信任并连接。",
                "First time connecting to this host. Verify the key fingerprint below before you trust and connect.",
            )
            .to_string(),
            detail,
            crate::i18n::t("信任并连接", "Trust & connect").to_string(),
        )
    }
}

/// Queue a host-key prompt: answer immediately if already decided this run,
/// merge into an existing pending entry for the same host, otherwise enqueue
/// (and show it now if nothing else is up).
fn enqueue_hostkey_prompt(
    win: &AppWindow,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
    changed: bool,
    responder: crate::ssh::HostKeyResponder,
) {
    let id = format!("{host}:{port}");
    if let Some(ans) = HOSTKEY_DECIDED.with(|d| d.borrow().get(&id).copied()) {
        responder.respond(ans);
        return;
    }
    let show_now = HOSTKEY_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.host == host && p.port == port) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        let (title, message, detail, confirm_label) =
            hostkey_dialog_text(&host, port, &key_type, &fingerprint, changed);
        q.push_back(PendingHostKey {
            host,
            port,
            changed,
            title,
            message,
            detail,
            confirm_label,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_hostkey(win);
    }
}

/// Push the front pending prompt's details into the window and open the dialog.
fn show_front_hostkey(win: &AppWindow) {
    HOSTKEY_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_hostkey_changed(p.changed);
            win.set_hostkey_title(p.title.clone().into());
            win.set_hostkey_message(p.message.clone().into());
            win.set_hostkey_detail(p.detail.clone().into());
            win.set_hostkey_confirm_label(p.confirm_label.clone().into());
            win.set_hostkey_prompt_open(true);
        }
    });
}

/// Apply the user's decision to the front prompt, then show the next one (or
/// close the dialog if the queue is now empty).
fn resolve_front_hostkey(win: &AppWindow, accept: bool) {
    let has_next = HOSTKEY_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            HOSTKEY_DECIDED.with(|d| {
                d.borrow_mut().insert(format!("{}:{}", p.host, p.port), accept);
            });
            for r in &p.responders {
                r.respond(accept);
            }
        }
        !q.is_empty()
    });
    if has_next {
        show_front_hostkey(win);
    } else {
        win.set_hostkey_prompt_open(false);
    }
}

// ---------------------------------------------------------------------------
// Connect-time credential prompt (#110)
// ---------------------------------------------------------------------------

/// One queued credential prompt. Connections to the same session (shell + its
/// SFTP channel) collapse into a single dialog whose answer fans out to each
/// waiting responder.
struct PendingCred {
    session_id: String,
    host: String,
    user: String,
    need_user: bool,
    need_password: bool,
    responders: Vec<crate::ssh::CredentialResponder>,
}

thread_local! {
    static CRED_QUEUE: RefCell<VecDeque<PendingCred>> = RefCell::new(VecDeque::new());
    /// session id → the answer given this run (`None` = cancelled), so a second
    /// connection for the same session is answered without re-prompting.
    static CRED_DECIDED: RefCell<HashMap<String, Option<crate::ssh::CredentialReply>>> =
        RefCell::new(HashMap::new());
}

/// Queue a credential prompt: answer immediately if already decided this run,
/// merge into an existing pending entry for the same session, otherwise enqueue
/// (and show it now if nothing else is up).
fn enqueue_cred_prompt(
    win: &AppWindow,
    session_id: String,
    host: String,
    user: String,
    need_user: bool,
    need_password: bool,
    responder: crate::ssh::CredentialResponder,
) {
    if let Some(reply) = CRED_DECIDED.with(|d| d.borrow().get(&session_id).cloned()) {
        responder.respond(reply);
        return;
    }
    let show_now = CRED_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.session_id == session_id) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        q.push_back(PendingCred {
            session_id,
            host,
            user,
            need_user,
            need_password,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_cred(win);
    }
}

/// Populate the credential dialog from the front prompt and open it.
fn show_front_cred(win: &AppWindow) {
    CRED_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_cred_host(p.host.clone().into());
            win.set_cred_need_user(p.need_user);
            win.set_cred_need_password(p.need_password);
            win.set_cred_user(p.user.clone().into());
            win.set_cred_password("".into());
            win.set_cred_remember(false);
            win.set_cred_prompt_open(true);
        }
    });
}

/// Apply the user's answer to the front credential prompt (or cancel), persist
/// it when "remember" is checked, then show the next prompt or close.
fn resolve_front_cred(win: &AppWindow, accept: bool) {
    let reply: Option<crate::ssh::CredentialReply> = if accept {
        Some((
            win.get_cred_user().to_string(),
            win.get_cred_password().to_string(),
            win.get_cred_remember(),
        ))
    } else {
        None
    };
    let has_next = CRED_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            CRED_DECIDED.with(|d| {
                d.borrow_mut().insert(p.session_id.clone(), reply.clone());
            });
            if let Some((ref u, ref pw, true)) = reply {
                persist_credentials(&p.session_id, u, pw, p.need_user, p.need_password);
            }
            for r in &p.responders {
                r.respond(reply.clone());
            }
        }
        !q.is_empty()
    });
    // Don't leave the typed password lingering in the UI property.
    win.set_cred_password("".into());
    if has_next {
        show_front_cred(win);
    } else {
        win.set_cred_prompt_open(false);
    }
}

/// Persist newly-entered credentials onto the saved session (#110, "remember").
fn persist_credentials(
    session_id: &str,
    user: &str,
    password: &str,
    set_user: bool,
    set_password: bool,
) {
    HISTORY_STORE.with(|s| {
        if let Some(store) = s.borrow().as_ref() {
            let mut st = store.borrow_mut();
            if let Some(mut sess) = st.get(session_id).cloned() {
                if set_user && !user.trim().is_empty() {
                    sess.user = user.trim().to_string();
                }
                if set_password {
                    sess.password = crate::config::Secret::new(password.to_string());
                }
                st.upsert(sess);
                let _ = st.save();
            }
        }
    });
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
    sftp_last_cwd: SftpLastCwd,
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
        let sftp_last_cwd = sftp_last_cwd.clone();
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
            sftp_last_cwd.lock().unwrap().remove(&id);
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
    sftp_last_cwd: SftpLastCwd,
) {
    // Navigate to a remote path (or ".." to go up one level).
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let weak = window.as_weak();
        window.on_sftp_navigate(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            // A pasted path may carry trailing whitespace / newline (#54).
            let path = path.trim();
            let resolved = if path == ".." {
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
            // Forget the followed cwd so the next OSC 7 — even at an unchanged
            // directory — snaps the panel back to the shell's cwd; manual
            // navigation never permanently disables cd-follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
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
            // "Always ask" (#87) forces the folder picker, ignoring the preset.
            let (preset, always_ask) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_download_dir().to_string(),
                        w.get_download_always_ask(),
                    )
                })
                .unwrap_or_default();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(&tab_id) {
                        h.download(remote_path, preset);
                        // Pop the transfers panel so progress is visible (user
                        // request: any download opens the download popup).
                        if let Some(w) = weak.upgrade() {
                            w.set_download_open(true);
                        }
                    }
                }
                return;
            }
            let sftp_handles = sftp_handles.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let local_dir = dir.to_string_lossy().to_string();
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            h.download(remote_path, local_dir);
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(|w| w.set_download_open(true));
                }
            });
        });
    }

    // Upload a local file into the current remote directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_upload_clicked(
            move |tab_id: SharedString, remote_dir: SharedString, folder: bool| {
                let tab_id = tab_id.to_string();
                let remote_dir = remote_dir.to_string();
                let sftp_handles = sftp_handles.clone();
                // Session-sync upload (#sync): when both the sync toggle and the
                // "sync upload" setting are on, mirror the upload to every other
                // online session — each into *that session's own* current SFTP
                // directory (paths differ between sessions, e.g. /home/jeff vs
                // /home/root, so the active session's path can't be reused).
                // Gather targets on the UI thread (Slint models aren't Send).
                let sync_targets: Vec<(String, String)> = weak
                    .upgrade()
                    .filter(|w| w.get_sync_input() && w.get_sync_upload_enabled())
                    .map(|w| {
                        let paths = terminal_sftp_paths(&w);
                        let handles = sftp_handles.lock().ok();
                        handles
                            .iter()
                            .flat_map(|h| h.keys())
                            .filter(|id| *id != &tab_id)
                            .filter_map(|id| paths.get(id).map(|dir| (id.clone(), dir.clone())))
                            .filter(|(_, dir)| !dir.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let weak_for_thread = weak.clone();
                std::thread::spawn(move || {
                    // The remote SFTP upload handles a file or a whole directory;
                    // only the local picker differs (#85). Folder uploads one dir;
                    // file mode allows selecting several at once.
                    let locals: Vec<String> = if folder {
                        rfd::FileDialog::new()
                            .pick_folder()
                            .map(|p| vec![p.to_string_lossy().to_string()])
                            .unwrap_or_default()
                    } else {
                        rfd::FileDialog::new()
                            .pick_files()
                            .map(|v| {
                                v.into_iter()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    if locals.is_empty() {
                        return;
                    }
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            for local in &locals {
                                h.upload(local.clone(), remote_dir.clone());
                            }
                        }
                        // Mirror to the other online sessions, each into its own
                        // current SFTP directory.
                        for (id, dir) in &sync_targets {
                            if let Some(h) = handles.get(id) {
                                for local in &locals {
                                    h.upload(local.clone(), dir.clone());
                                }
                            }
                        }
                    }
                    // Pop the transfers panel so upload progress is visible.
                    let _ = weak_for_thread.upgrade_in_event_loop(|w| w.set_download_open(true));
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
        let sftp_last_cwd = sftp_last_cwd.clone();
        window.on_sftp_tree_expand(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            // Forget the followed cwd (see on_sftp_navigate): tree navigation
            // must never permanently disable cd-follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.toggle_tree_node(path.clone());
                    h.list_dir(path);
                }
            }
        });
    }

    // Context menu → 删除 a remote file. The irreversible-delete confirmation
    // (#28) is handled by the in-app ConfirmDialog in the UI layer, so by the
    // time this fires the user has already confirmed.
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

    // Context menu → 查看 (read-only) / 编辑 (editable). Both load the file's
    // text into the built-in editor instead of an external app (#70).
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_view(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.read_text(path.to_string(), false);
                }
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.read_text(path.to_string(), true);
                }
            }
        });
    }
    // Open / edit with an external program (#81): download to a temp file and
    // hand it to the OS default app. Edit mode watches the temp copy and
    // re-uploads on every change.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_open_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), false);
                }
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), true);
                }
            }
        });
    }

    // Context-menu extensions (#69): one prompt dialog covers rename / chmod /
    // mkdir / touch; copy-path goes straight to the system clipboard.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_prompt_submit(
            move |tab_id: SharedString,
                  kind: SharedString,
                  target: SharedString,
                  value: SharedString| {
                let value = value.to_string();
                let value = value.trim();
                if value.is_empty() {
                    return;
                }
                let target = target.to_string();
                let handles = match sftp_handles.lock() {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let Some(h) = handles.get(tab_id.as_str()) else {
                    return;
                };
                match kind.as_str() {
                    "rename" => {
                        let to = format!(
                            "{}/{}",
                            parent_path(&target).trim_end_matches('/'),
                            value
                        );
                        h.rename(target, to);
                    }
                    "mkdir" => {
                        h.mkdir(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    "touch" => {
                        h.touch(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    _ => {}
                }
            },
        );
    }
    {
        window.on_sftp_copy_path(move |path: SharedString| {
            clipboard_set_text(path.to_string());
        });
    }

    // Visual chmod dialog (#84): decompose the current mode into nine bools on
    // open, recompose on apply (Slint has no bitwise ops).
    {
        let weak = window.as_weak();
        window.on_sftp_chmod_open(
            move |tab: SharedString, path: SharedString, name: SharedString, mode: i32| {
                let Some(w) = weak.upgrade() else { return };
                let m = mode as u32;
                w.set_chmod_tab(tab);
                w.set_chmod_path(path);
                w.set_chmod_name(name);
                w.set_chmod_or(m & 0o400 != 0);
                w.set_chmod_ow(m & 0o200 != 0);
                w.set_chmod_ox(m & 0o100 != 0);
                w.set_chmod_gr(m & 0o040 != 0);
                w.set_chmod_gw(m & 0o020 != 0);
                w.set_chmod_gx(m & 0o010 != 0);
                w.set_chmod_tr(m & 0o004 != 0);
                w.set_chmod_tw(m & 0o002 != 0);
                w.set_chmod_tx(m & 0o001 != 0);
                w.set_chmod_open(true);
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_chmod_apply(move || {
            let Some(w) = weak.upgrade() else { return };
            let mode = (w.get_chmod_or() as u32) << 8
                | (w.get_chmod_ow() as u32) << 7
                | (w.get_chmod_ox() as u32) << 6
                | (w.get_chmod_gr() as u32) << 5
                | (w.get_chmod_gw() as u32) << 4
                | (w.get_chmod_gx() as u32) << 3
                | (w.get_chmod_tr() as u32) << 2
                | (w.get_chmod_tw() as u32) << 1
                | (w.get_chmod_tx() as u32);
            let path = w.get_chmod_path().to_string();
            let tab = w.get_chmod_tab().to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab) {
                    h.chmod(path, mode);
                }
            }
        });
    }

    // Rebuild the editor's line-number gutter after each edit (#81). The text
    // comes straight from the TextInput so we don't re-read the property.
    {
        let weak = window.as_weak();
        window.on_editor_recount(move |text: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_editor_line_numbers(line_numbers_for(text.as_str()).into());
            }
        });
    }

    // Built-in editor: save (Ctrl+S / button) writes the text back to the
    // remote file (#70). Read-only (view) sessions never save.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_save_file(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() {
                return;
            }
            let path = w.get_editor_path().to_string();
            let content = w.get_editor_content().to_string();
            let tab_id = w.get_active_tab_id().to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.write_text(path, content);
                }
            }
            w.set_editor_dirty(false);
        });
    }
    // Close the editor; in edit mode upload first if there are unsaved edits.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_close_editor(move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.get_editor_readonly() && w.get_editor_dirty() {
                let path = w.get_editor_path().to_string();
                let content = w.get_editor_content().to_string();
                let tab_id = w.get_active_tab_id().to_string();
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(&tab_id) {
                        h.write_text(path, content);
                    }
                }
            }
            w.set_editor_open(false);
            w.set_editor_dirty(false);
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
    store: Rc<RefCell<ConfigStore>>,
    ctx: ConnectCtx,
) {
    // --- Command bar (#55): run command + quick-command management ---------
    {
        let handles_rc = handles.clone();
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_run_command(move |tab_id: SharedString, cmd: SharedString, to_all: bool| {
            let line = cmd.trim_end().to_string();
            if line.is_empty() {
                return;
            }
            let mut bytes = line.clone().into_bytes();
            bytes.push(b'\n');
            {
                let h = handles_rc.borrow();
                if to_all {
                    for handle in h.values() {
                        handle.send_raw(bytes.clone());
                    }
                } else if let Some(handle) = h.get(tab_id.as_str()) {
                    handle.send_raw(bytes);
                }
            }
            {
                let mut s = store_rc.borrow_mut();
                s.push_command_history(line);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_command_history(history_model(&store_rc.borrow()));
            }
        });
    }
    // Copy a history command to the clipboard (#96).
    {
        window.on_copy_text(move |text: SharedString| {
            let t = text.to_string();
            std::thread::spawn(move || clipboard_set_text(t));
        });
    }
    // Delete a history entry (#96). The model is in storage order now (#113),
    // so the row index maps straight through.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_delete_history(move |i: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let idx = i as usize;
                if idx < s.command_history().len() {
                    s.remove_command_history(idx);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_command_history(history_model(&store_rc.borrow()));
            }
        });
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_add_quick_command(move |name: SharedString, command: SharedString| {
            let name = name.trim().to_string();
            let command = command.to_string();
            if name.is_empty() || command.trim().is_empty() {
                return;
            }
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                v.push(crate::config::QuickCommand { name, command });
                s.set_quick_commands(v);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow()));
            }
        });
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_delete_quick_command(move |index: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                let i = index as usize;
                if i < v.len() {
                    v.remove(i);
                }
                s.set_quick_commands(v);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow()));
            }
        });
    }

    // Session sync / broadcast input: when on, a keystroke in any terminal is
    // mirrored to every online session (Xshell-style; #78 pt.4). Read on the hot
    // keystroke path, so use an AtomicBool rather than a window-property lookup.
    let sync_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = sync_input.clone();
        window.on_set_sync_input(move |on| {
            flag.store(on, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // Forward each keystroke as raw bytes to the SSH PTY. The server's bash /
    // readline handles echo, history (↑↓), Tab completion, Ctrl+C, etc.
    {
        let handles = handles.clone();
        let bufs = bufs.clone();
        let sync_input = sync_input.clone();
        // Shared timestamp: the last time the Shift key alone was pressed
        // (key="", shift=true).  Used by the time-based Backspace filter below.
        let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> =
            Arc::new(Mutex::new(None));
        window.on_send_key(move |tab_id: SharedString, key: SharedString, ctrl: bool, alt: bool, shift: bool| {
            // ── Enter on a disconnected tab → reconnect in place (#79) ──────
            // FinalShell-style: the tab shows "连接已断开,按 Enter 重新连接";
            // pressing Enter re-spawns the shell + SFTP workers in the SAME tab
            // with a fresh screen instead of forcing the user to open a new one.
            if key.as_str() == "\n" && !ctrl && !alt {
                let dead_session = {
                    let statuses = ctx.tab_statuses.lock().unwrap();
                    statuses
                        .get(tab_id.as_str())
                        .filter(|st| st.state == 2)
                        .map(|st| st.session_id.clone())
                };
                if let Some(session_id) = dead_session {
                    let Some(session) = store.borrow().get(&session_id).cloned() else {
                        return;
                    };
                    // Drop the dead shell/SFTP handles for this tab.
                    ctx.handles.borrow_mut().remove(tab_id.as_str());
                    if let Some(h) =
                        ctx.sftp_handles.lock().unwrap().remove(tab_id.as_str())
                    {
                        h.close();
                    }
                    // Fresh screen: new parser, cleared history/selection.
                    {
                        let mut map = ctx.bufs.lock().unwrap();
                        if let Some(b) = map.get_mut(tab_id.as_str()) {
                            let (rows, cols) = b.parser.screen().size();
                            b.parser = vt100::Parser::new(rows, cols, 5000);
                            b.history.clear();
                            b.prev.clear();
                            b.displayed_text.clear();
                            b.view_offset = 0;
                            b.sel_anchor = None;
                            b.sel_focus = None;
                        }
                    }
                    if let Some(st) =
                        ctx.tab_statuses.lock().unwrap().get_mut(tab_id.as_str())
                    {
                        st.state = 0;
                    }
                    // Fresh session: the first OSC 7 after reconnect follows.
                    ctx.sftp_last_cwd.lock().unwrap().remove(tab_id.as_str());
                    if let Some(w) = ctx.weak.upgrade() {
                        set_terminal_row(&w, tab_id.as_str(), |t| {
                            t.status =
                                crate::i18n::t("重连中...", "Reconnecting...").into();
                        });
                    }
                    start_session_in_tab(tab_id.as_str(), session, &ctx);
                    return;
                }
            }
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
            // Never log the raw key string — it can be a password character
            // (#15). redact_key keeps control codes but masks printable text.
            tracing::debug!(
                "send_key tab={} key={} ctrl={} alt={} shift={} app_cursor={}",
                tab_id, redact_key(key.as_str()), ctrl, alt, shift, app_cursor
            );

            // ── Shift / Backspace 诊断日志 (info 级, 无需 RUST_LOG=debug) ─────
            // 每个 Shift 相关事件都打印 key 的 Unicode 码位，方便对比
            // 左Shift / 右Shift 是否产生不同的 key 字符串。
            if shift || key.as_str() == "\u{0008}" {
                // INFO level (no RUST_LOG needed) — must not leak the key text.
                // redact_key reveals only control code points (the IME markers
                // this diagnostic cares about), masking any printable char that
                // could be part of a Shift-typed password symbol (#15).
                let codepoints = redact_key(key.as_str());
                let elapsed_ms = last_shift_time
                    .lock()
                    .unwrap()
                    .map(|t| format!("{}ms ago", t.elapsed().as_millis()))
                    .unwrap_or_else(|| "never".to_string());
                tracing::info!(
                    "[KEY_DIAG] key={} shift={} ctrl={} alt={} | last_shift={}",
                    codepoints, shift, ctrl, alt, elapsed_ms
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
            // Log only the length — never the keystroke bytes, which can be
            // password characters (#15).
            tracing::debug!(
                "send_key len={} handle_exists={}",
                bytes.len(),
                handles.borrow().contains_key(tab_id.as_str()),
            );
            if !bytes.is_empty() {
                let h = handles.borrow();
                if sync_input.load(std::sync::atomic::Ordering::Relaxed) {
                    // Broadcast the same bytes to every online session (#78 pt.4).
                    for handle in h.values() {
                        handle.send_raw(bytes.clone());
                    }
                } else if let Some(handle) = h.get(tab_id.as_str()) {
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
                let (old_rows, old_cols) = buf.parser.screen().size();
                let new_rows = rows as u16;
                // Shrinking the grid (e.g. dragging the SFTP panel up) makes
                // vt100's set_size truncate rows from the BOTTOM — silently
                // dropping the most recent output + prompt (#18).  To keep the
                // bottom (recent) rows we scroll the screen up first, but only
                // by as much as is needed to keep the CURSOR on-screen: the rows
                // *below* the cursor are unused blank space and can be truncated
                // for free.  Scrolling by the full delta instead would push real
                // content off the top into scrollback whenever the screen wasn't
                // full — e.g. a fresh shell with a few prompt lines — leaving a
                // blank grid with the cursor stranded at the top, and rapid
                // up/down dragging would repeat that until the prompt was gone.
                // Skipped on the alternate screen (vim/btop own their buffer).
                if new_rows < old_rows && !buf.parser.screen().alternate_screen() {
                    let (cursor_row, _) = buf.parser.screen().cursor_position();
                    // Rows that must scroll off the top to keep the cursor in view.
                    let scroll = (cursor_row + 1).saturating_sub(new_rows);
                    if scroll > 0 {
                        let saved: Vec<Line> = {
                            let s = buf.parser.screen();
                            (0..scroll).map(|r| build_row(s, r, old_cols)).collect()
                        };
                        for line in saved {
                            buf.history.push(line);
                        }
                        if buf.history.len() > MAX_HISTORY {
                            let drop = buf.history.len() - MAX_HISTORY;
                            buf.history.drain(0..drop);
                        }
                        buf.parser.process(format!("\x1b[{scroll}S").as_bytes());
                    }
                }
                buf.parser.set_size(new_rows, cols as u16);
                // The pre/post-resize screens differ in size+content; drop the
                // scroll-detection snapshot so the next output isn't mis-read as
                // a scroll (which would double-capture lines).
                buf.prev.clear();
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
                        let sel = buf.extract_selection_text();
                        if sel.is_empty() {
                            buf.displayed_text.join("\n")
                        } else {
                            sel
                        }
                    }
                    None => String::new(),
                }
            };
            // Run the clipboard write on a dedicated OS thread.  arboard's
            // Windows backend opens the clipboard and pumps Win32 messages;
            // doing that on the Slint/winit event-loop thread re-enters the
            // message loop and dead-locks the whole UI.
            std::thread::spawn(move || clipboard_set_text(text));
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
                        // Normalise line endings to a single CR so multi-line and
                        // backslash-continued commands paste correctly (see the
                        // function doc for the failure mode this prevents).
                        let bytes = normalize_pasted_newlines(&text).into_bytes();
                        let _ = sender.send(SessionCommand::RawInput(bytes));
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
                buf.sel_anchor = None;
                buf.sel_focus = None;
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
                // Anchor + focus in absolute scrollback coordinates.
                let abs = buf.vis_to_abs(r);
                buf.sel_anchor = Some((abs, c));
                buf.sel_focus = Some((abs, c));
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
                if buf.sel_anchor.is_some() {
                    let abs = buf.vis_to_abs(r);
                    buf.sel_focus = Some((abs, c));
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
                let extracted = buf.extract_selection_text();
                if extracted.is_empty() {
                    // Zero-area selection (a plain click) → clear it.
                    buf.sel_anchor = None;
                    buf.sel_focus = None;
                    None
                } else {
                    Some(extracted)
                }
            };
            match text {
                Some(t) if !t.is_empty() => {
                    // Auto-copy on release (select-to-copy, PuTTY style).
                    std::thread::spawn(move || clipboard_set_text(t));
                }
                _ => {}
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
    // Auto-scroll while drag-selecting past the visible top/bottom edge.  The
    // anchor is in absolute coordinates so it stays pinned no matter how far the
    // view moves; we only advance the scrollback view and re-point the focus at
    // the absolute row now sitting on the edge the mouse is parked against.
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
                if buf.sel_anchor.is_none() {
                    return;
                }
                let rows = buf.parser.screen().size().0;
                let last = rows.saturating_sub(1);
                let max_off = buf.history.len();
                let step = 2usize;
                // Keep the focus column the user last dragged to.
                let focus_col = buf.sel_focus.map(|f| f.1).unwrap_or(0);
                let edge_vis = if dir < 0 {
                    // Mouse above the top → reveal older lines.
                    let new_off = (buf.view_offset + step).min(max_off);
                    if new_off == buf.view_offset {
                        return; // already at the oldest line
                    }
                    buf.view_offset = new_off;
                    0u16
                } else if dir > 0 {
                    // Mouse below the bottom → move toward the live tail.
                    let new_off = buf.view_offset.saturating_sub(step);
                    if new_off == buf.view_offset {
                        return; // already at the live bottom
                    }
                    buf.view_offset = new_off;
                    last
                } else {
                    return;
                };
                let abs = buf.vis_to_abs(edge_vis);
                buf.sel_focus = Some((abs, focus_col));
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
/// Render a key string for diagnostic logs WITHOUT leaking its content (#15).
///
/// Any printable character could be a password character, so we never emit it.
/// Only C0/C1 control code points (Backspace, Esc, the IME-injected 0x10/0x15
/// markers, …) are revealed — those are exactly what the Shift/Backspace IME
/// diagnostics need and are never password material. Printable characters are
/// collapsed to a count, so the logs stay useful without exposing keystrokes.
fn redact_key(key: &str) -> String {
    if key.is_empty() {
        return "(empty)".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut printable = 0usize;
    for c in key.chars() {
        let cp = c as u32;
        if cp < 0x20 || (0x7f..=0x9f).contains(&cp) {
            parts.push(format!("U+{cp:04X}"));
        } else {
            printable += 1;
        }
    }
    if printable > 0 {
        parts.push(format!("<{printable} printable redacted>"));
    }
    parts.join(",")
}

/// `app_cursor` mirrors the remote terminal's DECCKM mode (`\x1b[?1h/l`):
/// when true the four arrow keys must use SS3 sequences (`\x1bOA`…) instead
/// of the default CSI sequences (`\x1b[A`…).  Full-screen apps like nano and
/// vim set this mode on startup.
/// Build the editor's line-number gutter text: "1\n2\n…\nN", one number per line
/// of `content`, matching its (newline-separated) line count (#81).
fn line_numbers_for(content: &str) -> String {
    use std::fmt::Write;
    let lines = content.split('\n').count().max(1);
    let mut s = String::with_capacity(lines * 4);
    for i in 1..=lines {
        if i > 1 {
            s.push('\n');
        }
        let _ = write!(s, "{i}");
    }
    s
}

/// Write `text` to the system clipboard. Call from a dedicated thread, never the
/// UI thread (arboard pumps the Win32 message loop / blocks).
///
/// On Linux the clipboard selection only persists while the owning client stays
/// alive, so we use arboard's `set().wait()`, which blocks this thread until
/// another app takes ownership — otherwise the copied text vanishes the moment
/// the `Clipboard` handle is dropped. Combined with the `wayland-data-control`
/// feature this is also what makes copy work on Wayland sessions (issue #47).
fn clipboard_set_text(text: String) {
    #[cfg(target_os = "linux")]
    let result = {
        use arboard::SetExtLinux as _;
        arboard::Clipboard::new().and_then(|mut cb| cb.set().wait().text(text))
    };
    #[cfg(not(target_os = "linux"))]
    let result = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
    if let Err(e) = result {
        tracing::warn!("clipboard set_text error: {}", e);
    }
}

/// Enumerate installed monospace font families for the Interface font picker.
/// Terminals want fixed-width fonts, so non-monospace families are filtered out.
fn system_monospace_fonts() -> Vec<slint::SharedString> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut names: Vec<String> = db
        .faces()
        .filter(|f| f.monospaced)
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .collect();
    names.sort();
    names.dedup();
    // Surface the built-in glyph-complete font first so it's selectable and the
    // default selection is shown — it isn't a system face so fontdb won't list it
    // (#114).
    names.retain(|n| n != "Meatshell Mono");
    let mut out = vec![slint::SharedString::from("Meatshell Mono")];
    out.extend(names.into_iter().map(slint::SharedString::from));
    out
}

/// Split a stored proxy URL into `(type, host:port)` for the session dialog.
///
/// `""` → `("none", "")`. Recognises `socks5`/`socks5h`/`socks` and
/// `http`/`https` scheme prefixes. A value without a (recognised) scheme is
/// treated as SOCKS5, matching proxy.rs's parse default, so older configs that
/// stored a bare `host:port` keep working.
/// Parse a "vX.Y.Z" / "X.Y.Z" tag into a comparable tuple, or None if it isn't
/// a three-part numeric version. A pre-release suffix on the patch (e.g.
/// "3-rc1") is tolerated by taking its leading digits (#48).
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

fn split_proxy(url: &str) -> (String, String) {
    let s = url.trim();
    if s.is_empty() {
        return ("none".to_string(), String::new());
    }
    let lower = s.to_ascii_lowercase();
    for p in ["http://", "https://"] {
        if lower.starts_with(p) {
            return ("http".to_string(), s[p.len()..].trim_end_matches('/').to_string());
        }
    }
    for p in ["socks5h://", "socks5://", "socks://"] {
        if lower.starts_with(p) {
            return ("socks5".to_string(), s[p.len()..].trim_end_matches('/').to_string());
        }
    }
    ("socks5".to_string(), s.trim_end_matches('/').to_string())
}

/// Normalise pasted text's line endings to a single CR (0x0d) — what a terminal
/// expects for Enter.
///
/// The clipboard may hold CRLF (Windows) or LF line breaks. Sending those to the
/// PTY verbatim makes the remote shell see *two* line breaks per line (CR then
/// LF), which prematurely ends a `\`-continued line: pasting
/// `sudo apt install \<newline>  docker-ce` would run `sudo apt install` with no
/// package and drop the rest. Collapsing every CRLF/LF to one CR fixes it.
fn normalize_pasted_newlines(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

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

    // --- Bare modifier keys: never forward to the PTY (issue #43) -----------
    // Slint encodes a lone modifier keypress not as "" but as a C0 code point:
    //   Shift=0x10 Ctrl=0x11 Alt=0x12 AltGr=0x13 CapsLock=0x14
    //   ShiftR=0x15 CtrlR=0x16 Meta=0x17 MetaR=0x18
    // Pressing Alt by itself (e.g. to Alt+Tab away) arrives here as key=0x12
    // with alt=true. Without this guard it would fall through to the Alt branch
    // below, get an ESC (0x1b) prefix, and bash/readline would treat the ESC as
    // Meta and discard the line the user was typing — the "Alt clears the
    // command" bug.
    //
    // The `!ctrl` guard is deliberate: a real Ctrl+P..Ctrl+X is encoded by some
    // Linux/macOS builds directly as the same C0 bytes (0x10..0x18) but with
    // ctrl=true (handled by the Ctrl branch just below), so we must NOT swallow
    // those. A lone modifier never carries ctrl=true except bare Ctrl/CtrlR
    // themselves, which are harmless to pass through as today.
    if !ctrl {
        if let Some(c) = key.chars().next() {
            let cp = c as u32;
            if key.chars().count() == 1 && (0x10..=0x18).contains(&cp) {
                return vec![];
            }
        }
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
/// Colours are stored as raw vt100::Color so the palette (dark vs. light)
/// can be applied at render time rather than at history-capture time.
/// This lets a theme switch retroactively recolour the entire scrollback.
#[derive(Clone)]
struct HistSpan {
    text: String,
    fg: vt100::Color,
    bg: vt100::Color,
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
) -> (String, vt100::Color, vt100::Color, bool, bool) {
    match screen.cell(r, c) {
        Some(cell) => {
            let (mut fg, mut bg) = (cell.fgcolor(), cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let s = cell.contents();
            // A CJK / wide glyph spans two cells; vt100 reports the 2nd as a
            // blank continuation. Emit nothing for it — the wide glyph already
            // covers both cells, so substituting a space would push the rest of
            // the line (and the cursor) out of alignment (#60). Genuinely empty
            // cells still become a space.
            let s = if cell.is_wide_continuation() {
                String::new()
            } else if s.is_empty() {
                " ".to_string()
            } else {
                s
            };
            (s, fg, bg, cell.bold(), cell.is_wide())
        }
        None => (
            " ".to_string(),
            vt100::Color::Default,
            vt100::Color::Default,
            false,
            false,
        ),
    }
}

fn build_row(screen: &vt100::Screen, r: u16, cols: u16) -> Line {
    let mut plain = String::with_capacity(cols as usize);
    let mut runs: Vec<HistSpan> = Vec::new();
    let mut c = 0u16;
    while c < cols {
        let (s, fg, bg, bold, wide) = cell_attrs(screen, r, c);
        // A wide (CJK) glyph gets its OWN span occupying exactly its two grid
        // cells, so the UI can box + centre + clip it on the monospace grid.
        // Otherwise a run of CJK rendered with a proportional CJK font drifts off
        // the grid — the trailing `/`, `$` or cursor overlaps or gaps the glyph
        // (CJK advance != 2×the Latin cell width).
        if wide {
            plain.push_str(&s);
            runs.push(HistSpan {
                text: s,
                fg,
                bg,
                bold,
                col: c as i32,
                cells: 2,
            });
            c += 2; // skip the wide-continuation cell
            continue;
        }
        // Group consecutive *narrow* cells that share fg + bg + bold into one run.
        // We keep blank cells *inside* a run (so a coloured bar made of spaces
        // still gets a background fill) and break on attribute change or a wide
        // cell (which starts its own span above).
        let start_col = c;
        let mut text = s.clone();
        plain.push_str(&s);
        c += 1;
        while c < cols {
            let (cs, cfg, cbg, cbold, cwide) = cell_attrs(screen, r, c);
            if cwide || cfg != fg || cbg != bg || cbold != bold {
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
            fg, // raw vt100::Color — converted at render time with the live palette
            bg,
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
    // ---- Absolute-coordinate selection helpers (#18 follow-up) -------------
    //
    // The "combined" buffer is `history` (oldest first) followed by the live
    // screen rows.  A visible window of `rows` rows looks at a slice of it whose
    // top index depends on whether we're at the live bottom or scrolled up.

    /// Live screen rows plus the count of non-blank ones at the top.
    fn live_rows(&self) -> (Vec<Line>, usize) {
        let s = self.parser.screen();
        let (rows, cols) = s.size();
        let live: Vec<Line> = (0..rows).map(|r| build_row(s, r, cols)).collect();
        let used = live
            .iter()
            .rposition(|(_, runs)| !runs.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        (live, used)
    }

    /// Absolute combined-row index of the top visible row for the current view.
    fn view_top_abs(&self, _live_used: usize) -> usize {
        let rows = self.parser.screen().size().0 as usize;
        let hist_len = self.history.len();
        if self.view_offset == 0 {
            // Live view: visible row 0 is live screen row 0 = combined[hist_len].
            hist_len
        } else {
            // Include the screen's full row count (trailing blanks too) so this
            // mapping matches render()'s scroll window — keeping the live and
            // scrolled views continuous after a shrink/grow (#119-followup).
            let combined_len = hist_len + rows;
            combined_len.saturating_sub(rows + self.view_offset)
        }
    }

    /// Map a visible row (0..rows) to its absolute combined-row index.
    fn vis_to_abs(&self, vis_row: u16) -> usize {
        let (_, live_used) = self.live_rows();
        self.view_top_abs(live_used) + vis_row as usize
    }

    /// Highlight rectangles for the current selection, clipped to the visible
    /// window of the current view.
    fn selection_rects_visible(&self, cols: u16) -> Vec<TermMatch> {
        let (Some((ar, ac)), Some((fr, fc))) = (self.sel_anchor, self.sel_focus) else {
            return Vec::new();
        };
        let (lo_r, lo_c, hi_r, hi_c) = if (ar, ac) <= (fr, fc) {
            (ar, ac, fr, fc)
        } else {
            (fr, fc, ar, ac)
        };
        if (lo_r, lo_c) == (hi_r, hi_c) {
            return Vec::new();
        }
        let (_, live_used) = self.live_rows();
        let top = self.view_top_abs(live_used);
        let rows = self.parser.screen().size().0;
        let mut out = Vec::new();
        for vis in 0..rows {
            let abs = top + vis as usize;
            if abs < lo_r || abs > hi_r {
                continue;
            }
            let (c0, c1) = if abs == lo_r && abs == hi_r {
                (lo_c.min(hi_c), lo_c.max(hi_c))
            } else if abs == lo_r {
                (lo_c, cols.saturating_sub(1))
            } else if abs == hi_r {
                (0, hi_c)
            } else {
                (0, cols.saturating_sub(1))
            };
            out.push(TermMatch {
                row: vis as i32,
                col: c0 as i32,
                len: (c1.saturating_sub(c0) + 1) as i32,
            });
        }
        out
    }

    /// Extract the selected text from the combined buffer (whole selection,
    /// even the parts currently scrolled out of view).
    fn extract_selection_text(&self) -> String {
        let (Some((ar, ac)), Some((fr, fc))) = (self.sel_anchor, self.sel_focus) else {
            return String::new();
        };
        let (lo_r, lo_c, hi_r, hi_c) = if (ar, ac) <= (fr, fc) {
            (ar, ac, fr, fc)
        } else {
            (fr, fc, ar, ac)
        };
        let (live, live_used) = self.live_rows();
        let hist_len = self.history.len();
        let combined_len = hist_len + live_used;
        // Clamp into real content so a focus parked on a blank row below the
        // prompt doesn't emit trailing empty lines.
        let hi_r = hi_r.min(combined_len.saturating_sub(1));
        let mut out = String::new();
        for r in lo_r..=hi_r {
            let line: &str = if r < hist_len {
                &self.history[r].0
            } else if r - hist_len < live.len() {
                &live[r - hist_len].0
            } else {
                ""
            };
            let chars: Vec<char> = line.chars().collect();
            let (c0, c1) = if r == lo_r && r == hi_r {
                (lo_c.min(hi_c), lo_c.max(hi_c))
            } else if r == lo_r {
                (lo_c, u16::MAX)
            } else if r == hi_r {
                (0, hi_c)
            } else {
                (0, u16::MAX)
            };
            let c0 = (c0 as usize).min(chars.len());
            let c1 = ((c1 as usize).saturating_add(1)).min(chars.len());
            let seg: String = if c0 < c1 {
                chars[c0..c1].iter().collect()
            } else {
                String::new()
            };
            out.push_str(seg.trim_end());
            if r != hi_r {
                out.push('\n');
            }
        }
        out
    }

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
                        cjk: contains_cjk(&hs.text),
                        text: hs.text.into(),
                        fg: vt_color_to_slint(hs.fg, hs.bold, self.is_dark),
                        bg: vt_bg_to_slint(hs.bg, self.is_dark),
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
        let hist_len = self.history.len();
        // Include the screen's trailing blank rows in the scroll range so this
        // scrolled view stays continuous with the live view (view_offset 0).
        // Trimming to only the used rows made the two views misalign after a
        // shrink-then-grow (dragging the SFTP panel over the terminal and back),
        // so scrolling back jumped at the bottom instead of moving line-by-line
        // (#119-followup).
        let combined_len = hist_len + live.len();
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
                    fg: vt_color_to_slint(hs.fg, hs.bold, self.is_dark),
                    bg: vt_bg_to_slint(hs.bg, self.is_dark),
                    bold: hs.bold,
                    row: d as i32,
                    col: hs.col,
                    cells: hs.cells,
                    cjk: contains_cjk(&hs.text),
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

/// True if a terminal span contains any CJK character — ideograph, kana, or
/// (crucially) CJK punctuation like 、。，. The mono terminal font has no CJK
/// glyphs and Slint's per-script fallback tofu's *isolated* CJK punctuation
/// (it renders fine only when adjacent to a Han char), so these spans are drawn
/// with the CJK-capable UI font instead (#54). Box-drawing / powerline glyphs
/// are deliberately excluded so they keep the aligned monospace font.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x2E80..=0x2EFF       // CJK radicals
            | 0x3000..=0x303F     // CJK symbols & punctuation (、。「」…)
            | 0x3040..=0x30FF     // hiragana + katakana
            | 0x3100..=0x312F     // bopomofo
            | 0x3400..=0x4DBF     // CJK ext A
            | 0x4E00..=0x9FFF     // CJK unified ideographs
            | 0xF900..=0xFAFF     // CJK compatibility ideographs
            | 0xFF00..=0xFFEF     // fullwidth / halfwidth forms (，！？：；)
            | 0x20000..=0x2FA1F)  // CJK ext B–F + compat supplement
    })
}

/// 16-colour ANSI palette for **dark** terminals (VS Code "Dark+" values).
const ANSI16_DARK: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0  black
    (0xcd, 0x31, 0x31), // 1  red
    (0x0d, 0xbc, 0x79), // 2  green
    (0xe5, 0xe5, 0x10), // 3  yellow
    (0x24, 0x72, 0xc8), // 4  blue
    (0xbc, 0x3f, 0xbc), // 5  magenta
    (0x11, 0xa8, 0xcd), // 6  cyan
    (0xe5, 0xe5, 0xe5), // 7  white        (light grey on dark bg)
    (0x66, 0x66, 0x66), // 8  bright black
    (0xf1, 0x4c, 0x4c), // 9  bright red
    (0x23, 0xd1, 0x8b), // 10 bright green
    (0xf5, 0xf5, 0x43), // 11 bright yellow
    (0x3b, 0x8e, 0xea), // 12 bright blue
    (0xd6, 0x70, 0xd6), // 13 bright magenta
    (0x29, 0xb8, 0xdb), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white
];

/// 16-colour ANSI palette for **light** terminal **foreground** (text) use.
///
/// On a near-white (#fafafa) background, the standard "white" (slot 7) and
/// "bright white" (slot 15) are nearly invisible.  We remap them to dark greys
/// so `ls`, `git` and other tools that use colour 7 for regular text stay
/// perfectly readable.  Saturated hues are darkened for contrast.
const ANSI16_LIGHT: [(u8, u8, u8); 16] = [
    (0x1c, 0x1c, 0x1e), // 0  black        → Apple near-black
    (0xc0, 0x39, 0x2b), // 1  red
    (0x1a, 0x7f, 0x37), // 2  green        → darker for white bg
    (0x85, 0x64, 0x04), // 3  yellow       → dark amber, readable
    (0x04, 0x51, 0xa5), // 4  blue         → VS Code light blue
    (0x80, 0x00, 0x80), // 5  magenta
    (0x0e, 0x72, 0x5c), // 6  cyan         → darker teal
    (0x3a, 0x3a, 0x3c), // 7  white        → dark grey (was 0xe5e5e5, near-invisible)
    (0x55, 0x55, 0x55), // 8  bright black
    (0xe7, 0x4c, 0x3c), // 9  bright red
    (0x27, 0xae, 0x60), // 10 bright green
    (0xd4, 0xac, 0x0d), // 11 bright yellow
    (0x2e, 0x86, 0xc1), // 12 bright blue
    (0x9b, 0x59, 0xb6), // 13 bright magenta
    (0x1a, 0xbc, 0x9c), // 14 bright cyan
    (0x2c, 0x2c, 0x2e), // 15 bright white → dark (was 0xffffff, near-invisible)
];

/// 16-colour ANSI palette for **light** terminal **background** (fill) use.
///
/// When TUI programs (btop, htop, vim) paint cell backgrounds in light mode,
/// each colour maps to a light-tinted variant so the overall UI feels light.
/// "Black" (slot 0) becomes a very light grey rather than near-black, so
/// dark-background TUI apps naturally inherit a light appearance.  Foreground
/// text always uses `ANSI16_LIGHT` so readability is unaffected.
const ANSI16_LIGHT_BG: [(u8, u8, u8); 16] = [
    (0xe8, 0xe8, 0xed), // 0  black        → Apple system-grey-6 (very light)
    (0xff, 0xd5, 0xd5), // 1  red          → light rose
    (0xd5, 0xf5, 0xd5), // 2  green        → light mint
    (0xff, 0xf8, 0xd5), // 3  yellow       → light cream
    (0xd5, 0xe8, 0xf8), // 4  blue         → light sky
    (0xf5, 0xd5, 0xf5), // 5  magenta      → light lilac
    (0xd5, 0xf5, 0xf8), // 6  cyan         → light aqua
    (0xf5, 0xf5, 0xf7), // 7  white        → Apple bg (near-white)
    (0xd1, 0xd1, 0xd6), // 8  bright black → Apple system-grey-4
    (0xff, 0xbe, 0xbe), // 9  bright red   → light salmon
    (0xbe, 0xf5, 0xbe), // 10 bright green
    (0xf5, 0xf5, 0xbe), // 11 bright yellow
    (0xbe, 0xdd, 0xff), // 12 bright blue  → light periwinkle
    (0xf0, 0xbe, 0xff), // 13 bright magenta → light violet
    (0xbe, 0xf5, 0xff), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white → white
];

/// Convert a vt100 foreground colour (+ bold) to a Slint colour.
/// Bold + a base colour (0–7) maps to the bright variant (8–15), matching
/// how terminals render `ls --color` (bold-green executables, bold-blue dirs).
///
/// In light mode, true-colour RGB foregrounds that are light (HSL lightness
/// ≥ 0.55) are darkened so they remain readable on a near-white background.
fn vt_color_to_slint(color: vt100::Color, bold: bool, is_dark: bool) -> slint::Color {
    let (r, g, b) = match color {
        vt100::Color::Default => {
            if is_dark { (0xd4, 0xd4, 0xd4) } else { (0x2d, 0x2d, 0x2f) }
        }
        vt100::Color::Idx(i) => idx_to_rgb(i, bold, is_dark),
        vt100::Color::Rgb(r, g, b) => {
            if is_dark { (r, g, b) } else { darken_light_fg(r, g, b) }
        }
    };
    slint::Color::from_rgb_u8(r, g, b)
}

/// In light mode, remap light true-colour foregrounds to dark so they are
/// readable on a near-white background.  Colours already dark (L < 0.55)
/// pass through unchanged.
fn darken_light_fg(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(r, g, b);
    if l < 0.55 {
        return (r, g, b);
    }
    // L=0.55 → 0.40 (readable dark grey), L=1.0 (white) → ~0.15 (near-black).
    let new_l = (0.40 - (l - 0.55) * 0.56).max(0.10);
    hsl_to_rgb(h, s, new_l)
}

/// Convert a vt100 *background* colour to Slint.  The default background maps
/// to fully transparent so we don't paint a fill over the terminal's own bg.
/// Non-default backgrounds (btop/htop bars, selected rows) become opaque.
///
/// In light mode:
/// - ANSI 16 colours use `ANSI16_LIGHT_BG` (light pastels).
/// - True-colour RGB backgrounds that are dark (HSL lightness < 0.45) are
///   remapped to light pastels so programs like btop feel light-themed.
fn vt_bg_to_slint(color: vt100::Color, is_dark: bool) -> slint::Color {
    match color {
        vt100::Color::Default => slint::Color::from_argb_u8(0, 0, 0, 0), // transparent
        vt100::Color::Idx(i) => {
            let (r, g, b) = idx_to_rgb_bg(i, is_dark);
            slint::Color::from_rgb_u8(r, g, b)
        }
        vt100::Color::Rgb(r, g, b) => {
            if is_dark {
                slint::Color::from_rgb_u8(r, g, b)
            } else {
                let (nr, ng, nb) = lighten_dark_bg(r, g, b);
                slint::Color::from_rgb_u8(nr, ng, nb)
            }
        }
    }
}

/// In light mode, remap dark true-colour backgrounds to light pastels.
/// Colours whose HSL lightness is already ≥ 0.45 pass through unchanged
/// (the program chose a light colour deliberately).
fn lighten_dark_bg(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(r, g, b);
    if l >= 0.45 {
        return (r, g, b);
    }
    // Remap: darkest (l≈0) → very light (l≈0.92); l=0.45 → l≈0.84.
    // Reduce saturation to pastel so colours don't look garish on white.
    let new_l = 0.92 - l * 0.18;
    let new_s = (s * 0.35).min(0.25);
    hsl_to_rgb(h, new_s, new_l)
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < 1e-6 {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < 1e-6 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s < 1e-6 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| -> f32 {
        if t < 0.0 { t += 1.0; }
        if t > 1.0 { t -= 1.0; }
        if t < 1.0 / 6.0 { return p + (q - p) * 6.0 * t; }
        if t < 0.5 { return q; }
        if t < 2.0 / 3.0 { return p + (q - p) * (2.0 / 3.0 - t) * 6.0; }
        p
    };
    (
        (hue(h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue(h) * 255.0).round() as u8,
        (hue(h - 1.0 / 3.0) * 255.0).round() as u8,
    )
}

/// Map an xterm-256 palette index to RGB (16 ANSI + 6×6×6 cube + grayscale).
fn idx_to_rgb(i: u8, bold: bool, is_dark: bool) -> (u8, u8, u8) {
    let i = if bold && i < 8 { i + 8 } else { i };
    let palette = if is_dark { &ANSI16_DARK } else { &ANSI16_LIGHT };
    match i {
        0..=15 => palette[i as usize],
        16..=231 => {
            let n = i - 16;
            let to = |v: u8| -> u8 {
                if v == 0 { 0 } else { 55 + v * 40 }
            };
            (to(n / 36), to((n % 36) / 6), to(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Same as [`idx_to_rgb`] but for **background** fills in light mode: the 16
/// ANSI base colours use `ANSI16_LIGHT_BG` (light pastels) so TUI program
/// backgrounds feel light.  256-colour cube / grayscale are used as-is.
fn idx_to_rgb_bg(i: u8, is_dark: bool) -> (u8, u8, u8) {
    if !is_dark && i < 16 {
        return ANSI16_LIGHT_BG[i as usize];
    }
    idx_to_rgb(i, false, is_dark)
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

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn bare_alt_is_not_forwarded() {
        // Slint sends Alt-alone as key=0x12 with alt=true. It must produce no
        // bytes — otherwise it becomes ESC+0x12 and clears the input (issue #43).
        assert_eq!(key_to_pty_bytes("\u{0012}", false, true, false), Vec::<u8>::new());
    }

    #[test]
    fn bare_modifier_codes_are_dropped() {
        // Shift..MetaR (0x10..=0x18) pressed alone (ctrl=false) → nothing sent.
        for cp in 0x10u32..=0x18 {
            let s = char::from_u32(cp).unwrap().to_string();
            assert_eq!(
                key_to_pty_bytes(&s, false, false, false),
                Vec::<u8>::new(),
                "code point {:#04x} should be dropped",
                cp
            );
        }
    }

    #[test]
    fn ctrl_letter_c0_still_passes() {
        // A real Ctrl+R encoded as the C0 byte 0x12 with ctrl=true must still be
        // forwarded — the !ctrl guard keeps the #43 fix from breaking it.
        assert_eq!(key_to_pty_bytes("\u{0012}", true, false, false), vec![0x12]);
        // Ctrl+X as C0 0x18.
        assert_eq!(key_to_pty_bytes("\u{0018}", true, false, false), vec![0x18]);
    }

    #[test]
    fn alt_letter_still_sends_esc_prefix() {
        // Alt+a (a real Meta combo) must still send ESC + 'a'.
        assert_eq!(key_to_pty_bytes("a", false, true, false), vec![0x1b, b'a']);
    }

    #[test]
    fn split_proxy_recognises_schemes() {
        assert_eq!(split_proxy(""), ("none".into(), "".into()));
        assert_eq!(
            split_proxy("http://10.0.0.1:1022"),
            ("http".into(), "10.0.0.1:1022".into())
        );
        assert_eq!(
            split_proxy("socks5://127.0.0.1:1080"),
            ("socks5".into(), "127.0.0.1:1080".into())
        );
        // user:pass survive in the host:port part.
        assert_eq!(
            split_proxy("http://u:p@host:8080"),
            ("http".into(), "u:p@host:8080".into())
        );
        // bare host:port (legacy) → treated as socks5.
        assert_eq!(
            split_proxy("127.0.0.1:1080"),
            ("socks5".into(), "127.0.0.1:1080".into())
        );
    }

    #[test]
    fn paste_normalizes_newlines_to_cr() {
        // CRLF (Windows clipboard) and LF both collapse to a single CR so a
        // backslash-continued multi-line command pastes intact.
        assert_eq!(
            normalize_pasted_newlines("sudo apt install \\\r\n  docker-ce"),
            "sudo apt install \\\r  docker-ce"
        );
        assert_eq!(normalize_pasted_newlines("a\nb\nc"), "a\rb\rc");
        // A lone CR is left as-is; no doubling.
        assert_eq!(normalize_pasted_newlines("a\rb"), "a\rb");
        // No newlines → unchanged.
        assert_eq!(normalize_pasted_newlines("echo hi"), "echo hi");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn hist_line(s: &str) -> Line {
        (s.to_string(), Vec::new())
    }

    /// A TermBuffer whose live screen (rows×cols) shows `live_lines`, with the
    /// given `history` above it, viewed at `view_offset` (0 = live bottom).
    fn make_buf(
        rows: u16,
        cols: u16,
        history: &[&str],
        live_lines: &[&str],
        view_offset: usize,
    ) -> TermBuffer {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(live_lines.join("\r\n").as_bytes());
        TermBuffer {
            parser,
            find_query: String::new(),
            is_dark: false,
            sel_anchor: None,
            sel_focus: None,
            history: history.iter().map(|s| hist_line(s)).collect(),
            prev: Vec::new(),
            view_offset,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        }
    }

    #[test]
    fn vis_to_abs_maps_live_and_scrolled_consistently() {
        // history H0..H2 (3 lines), live LIVE0/LIVE1 → combined len 5.
        let live = make_buf(5, 20, &["H0", "H1", "H2"], &["LIVE0", "LIVE1"], 0);
        assert_eq!(live.vis_to_abs(0), 3, "live row 0 is first live line");
        assert_eq!(live.vis_to_abs(1), 4);

        // Scrolled to the very top (offset = history len).
        let top = make_buf(5, 20, &["H0", "H1", "H2"], &["LIVE0", "LIVE1"], 3);
        assert_eq!(top.vis_to_abs(0), 0, "top row 0 is oldest history line");
        assert_eq!(top.vis_to_abs(2), 2);
        assert_eq!(top.vis_to_abs(3), 3, "row 3 crosses into live content");
    }

    #[test]
    fn extract_spans_history_and_live() {
        let mut buf = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], 3);
        buf.sel_anchor = Some((0, 0)); // top of history
        buf.sel_focus = Some((4, 19)); // end of last live line
        assert_eq!(
            buf.extract_selection_text(),
            "HIST0\nHIST1\nHIST2\nLIVE0\nLIVE1"
        );
    }

    #[test]
    fn extract_is_view_independent() {
        // The same absolute selection copies identically whether the view is
        // scrolled to the top or sitting at the live bottom — this is the whole
        // point of the fix (a top-to-bottom selection survives auto-scrolling).
        let sel = |off| {
            let mut b = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], off);
            b.sel_anchor = Some((0, 0));
            b.sel_focus = Some((4, 19));
            b.extract_selection_text()
        };
        assert_eq!(sel(3), sel(0));
        assert_eq!(sel(3), "HIST0\nHIST1\nHIST2\nLIVE0\nLIVE1");
    }

    #[test]
    fn highlight_clipped_to_current_view() {
        // Scrolled to the top: a history selection is on-screen and highlighted.
        let mut top = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], 3);
        top.sel_anchor = Some((0, 2));
        top.sel_focus = Some((2, 4));
        let rects = top.selection_rects_visible(20);
        assert_eq!(rects.len(), 3, "rows 0,1,2 (the 3 history lines) highlighted");
        assert_eq!(rects[0].row, 0);
        assert_eq!(rects[2].row, 2);

        // At the live bottom the same history selection is scrolled off → none.
        let mut live = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], 0);
        live.sel_anchor = Some((0, 2));
        live.sel_focus = Some((2, 4));
        assert!(live.selection_rects_visible(20).is_empty());
    }
}
