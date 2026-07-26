//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// How much of the byte stream we retain per tab for resize-reflow (#169).
pub(crate) const RAW_CAP: usize = 2 * 1024 * 1024;

/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// Keeps a single UI callback from spending hundreds of ms in vt100 ingest.
const OUTPUT_MERGE_BYTE_CAP: usize = 64 * 1024;

/// Max UI renders per second for a tab under sustained output (#209).
pub(crate) const RENDER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

pub(crate) fn term_buf(bufs: &TermBuffers, tab_id: &str) -> Option<TermBufferHandle> {
    bufs.lock().unwrap().get(tab_id).cloned()
}

pub(crate) fn with_term_buf<R>(
    bufs: &TermBuffers,
    tab_id: &str,
    f: impl FnOnce(&mut TermBuffer) -> R,
) -> Option<R> {
    let h = term_buf(bufs, tab_id)?;
    let mut guard = h.lock().unwrap();
    Some(f(&mut guard))
}

fn ingest_terminal_output(bufs: &TermBuffers, tab_id: &str, chunk: &[u8]) {
    if let Some(h) = term_buf(bufs, tab_id) {
        h.lock().unwrap().ingest(chunk);
    }
}

use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;

use crate::config::{ConfigStore, OutputHighlightRule};
use crate::i18n::t;
use crate::resource::{LocalSnap, NetHist, TabStatus, TabStatuses};
use crate::sftp::{SftpHandles, SftpLastCwd};
use crate::ssh::{format_mtime, format_size, test_session_auth, SessionHandle};
use crate::terminal::{
    CsiState, OutputHighlightPreset, RenderGates, TabRenderGate,
    TermBuffer, TermBufferHandle, TermBuffers,
};
use crate::resource::system::{SystemSampler, SystemSnapshot};
use crate::ui::*;

mod context;
pub(crate) use context::*;

mod platform;
pub(crate) use platform::*;

mod util;
pub(crate) use util::*;

mod layout;
pub(crate) use layout::*;

mod sftp;
pub(crate) use sftp::*;

mod system;
pub(crate) use system::*;

mod theme;
pub(crate) use theme::*;

mod models;
pub(crate) use models::*;

mod render;
pub(crate) use render::*;

mod input;
pub(crate) use input::*;

mod session;
pub(crate) use session::*;

mod auth;
pub(crate) use auth::*;

mod webdav;
pub(crate) use webdav::*;

mod tabs;
pub(crate) use tabs::*;

/// Number of samples kept for the sparkline.
const NET_HISTORY_LEN: usize = 60;

pub fn run() -> Result<()> {
    // Load the renderer preference before creating any Slint window. Reuse the
    // same store for the rest of the app so startup does not read the config
    // twice merely to select a backend (#280).
    let config = ConfigStore::load().context("failed to load config")?;

    // Windows frameless-window attributes must be fixed before the first Slint
    // window is created; doing it afterwards leaves some Win10 machines with an
    // invisible frame that shifts mouse hit testing (#193).
    #[cfg(windows)]
    setup_windows_platform(config.renderer_mode());

    // Immersive native title bar on macOS (must precede the first window).
    #[cfg(target_os = "macos")]
    setup_macos_platform();

    // --- Runtime + store -------------------------------------------------
    let runtime = Arc::new(Runtime::new().context("failed to start tokio runtime")?);
    let store = Rc::new(RefCell::new(config));
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
    let render_gates: RenderGates = Arc::new(Mutex::new(HashMap::new()));

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
    // Slint applies preferred-width/height while the native window is being
    // created. Do not treat those startup Resized events as user adjustments;
    // otherwise they overwrite the persisted size before restoration (#278).
    let window_size_tracking_ready = Rc::new(Cell::new(false));
    let pending_window_size_restore = Rc::new(Cell::new(None::<(f32, f32)>));

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

    // --- Detachable process monitor window (#23) -----------------------------
    // The process table is its own top-level OS window so it can be dragged
    // outside the main window (or onto a second monitor). Both windows render
    // the *same* VecModel, so the table stays live wherever it's parked; closing
    // it just hides it, so reopening is instant.
    let proc_rows_model: Rc<VecModel<ProcRow>> = Rc::new(VecModel::default());
    window.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_metrics_model: Rc<VecModel<SysMetricRow>> = Rc::new(VecModel::default());
    let sys_net_rows_model: Rc<VecModel<SysNetRow>> = Rc::new(VecModel::default());
    let sys_disks_model: Rc<VecModel<DiskInfo>> = Rc::new(VecModel::default());
    let sys_overview_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_gpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_usage_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_memory_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_swap_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_network_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_filesystem_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    window.set_sys_metrics(ModelRc::from(sys_metrics_model.clone()));
    window.set_sys_net_rows(ModelRc::from(sys_net_rows_model.clone()));
    window.set_sys_disks(ModelRc::from(sys_disks_model.clone()));
    window.set_sys_overview_rows(ModelRc::from(sys_overview_model.clone()));
    window.set_sys_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    window.set_sys_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    window.set_sys_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    window.set_sys_memory_rows(ModelRc::from(sys_memory_model.clone()));
    window.set_sys_swap_rows(ModelRc::from(sys_swap_model.clone()));
    window.set_sys_network_rows(ModelRc::from(sys_network_model.clone()));
    window.set_sys_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    let proc_win = ProcWindow::new().context("failed to build process window")?;
    proc_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    proc_win.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_win = SystemInfoWindow::new().context("failed to build system info window")?;
    sys_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    sys_win.set_metrics(ModelRc::from(sys_metrics_model.clone()));
    sys_win.set_nets(ModelRc::from(sys_net_rows_model.clone()));
    sys_win.set_disks(ModelRc::from(sys_disks_model.clone()));
    sys_win.set_overview_rows(ModelRc::from(sys_overview_model.clone()));
    sys_win.set_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    sys_win.set_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    sys_win.set_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    sys_win.set_memory_rows(ModelRc::from(sys_memory_model.clone()));
    sys_win.set_swap_rows(ModelRc::from(sys_swap_model.clone()));
    sys_win.set_network_rows(ModelRc::from(sys_network_model.clone()));
    sys_win.set_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    {
        // ✕ hides the window (data keeps flowing into the shared model).
        let weak = proc_win.as_weak();
        proc_win.on_close(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        proc_win.on_copy_pid(move |pid: SharedString| {
            let text = pid.to_string();
            std::thread::spawn(move || clipboard_set_text(text));
        });
    }
    {
        // Frameless titlebar drag, via winit on the process window's own handle.
        let weak = proc_win.as_weak();
        proc_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // Bottom-right resize grip.
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = proc_win.as_weak();
        proc_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // The sidebar "Processes" button shows / focuses the window.
        let win_weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        window.on_open_processes(move || {
            let (Some(main), Some(pw)) = (win_weak.upgrade(), proc_weak.upgrade()) else {
                return;
            };
            pw.set_host(main.get_connection_state());
            sync_proc_theme(&main, &pw);
            let _ = pw.show();
            place_process_window(&main, &pw);
            pw.window().with_winit_window(|ww| ww.focus_window());
        });
    }
    {
        let weak = sys_win.as_weak();
        sys_win.on_close(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let weak = sys_win.as_weak();
        sys_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = sys_win.as_weak();
        sys_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        let win_weak = window.as_weak();
        let sys_weak = sys_win.as_weak();
        window.on_open_system_info(move || {
            let (Some(main), Some(sw)) = (win_weak.upgrade(), sys_weak.upgrade()) else {
                return;
            };
            // Detailed system information is remote-only. Keep this guard even
            // though the sidebar hides/disables its affordance when unavailable.
            if !main.get_system_info_available() {
                return;
            }
            sw.set_host(main.get_conn_host());
            sw.set_connection_state(main.get_connection_state());
            sw.set_resource_title(main.get_resource_title());
            sync_system_info_theme(&main, &sw);
            let _ = sw.show();
            place_system_info_window(&main, &sw);
            sw.window().with_winit_window(|ww| ww.focus_window());
        });
    }

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
        let is_dark = theme_pref_is_dark(&store.borrow());
        window.set_dark_mode(is_dark);
    }
    // On macOS, app shortcuts use Cmd (⌘) so physical Ctrl stays free for the
    // shell (#158); on Windows/Linux they stay Ctrl-based.
    window.set_is_mac(cfg!(target_os = "macos"));
    window.set_is_windows(cfg!(windows));

    // Apply the saved terminal font (Interface settings). An empty family keeps
    // the built-in default; the size always applies (defaults to 13).
    {
        let s = store.borrow();
        let fam = s.font_family().to_string();
        if !fam.is_empty() {
            window.set_term_font_family(fam.into());
        }
        window.set_term_font_size(s.font_size() as f32);
        window.set_term_font_bold(s.terminal_bold());
        window.set_term_cursor_style(s.terminal_cursor_style().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            window.set_term_cursor_color_hex(s.terminal_cursor_color().into());
            window.set_term_cursor_color(color);
        }
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        window.set_panel_font(s.panel_font() as f32 / 100.0); // settings-panel font scale
        window.set_renderer_mode(s.renderer_mode().into());
    }

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = store.borrow().wallpaper().to_string();
        apply_wallpaper(&window, &store.borrow(), &bufs, &id);
    }
    // Editable inputs (e.g. the SFTP path bar) need a CJK-capable font: the
    // embedded mono font has no Chinese glyphs and native TextInput doesn't
    // glyph-fallback like Text does, so typed Chinese would render as tofu (#54).
    //
    // We must NOT hard-code one system font name: on macOS 26 (Tahoe) fontdb
    // failed to register "PingFang SC", so the UI default font resolved to nothing
    // and *all* text vanished (#129) — icons survived only because they use an
    // embedded font. Instead probe what fontdb actually loaded and pick the first
    // resolvable CJK family, falling back to the embedded "Meatshell Mono" so the
    // window is never fully blank even when the system font DB is unreadable.
    window.set_ui_font_family(resolve_ui_font_family());
    // Populate the Interface font picker with installed monospace families.
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(
        system_monospace_fonts(),
    ))));

    // Command bar (#55): seed quick commands + history from the config. Groups
    // start collapsed by default (#55).
    window.set_quick_commands(quick_cmd_model(
        &store.borrow(),
        &all_quick_group_names(&store.borrow()),
    ));
    window.set_command_history(history_model(&store.borrow()));
    window.set_history_view(history_view_model(&store.borrow(), "")); // #101

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
        let sidebar_dock = s.sidebar_dock();
        let welcome_as_sidebar = s.welcome_as_sidebar();
        let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
        let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
        let quick_panel_collapsed = s.quick_panel_collapsed();
        let quick_panel_dock = s.quick_panel_dock();
        let welcome_sidebar_dock = s.welcome_sidebar_dock();
        let mut sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
        let mut welcome_collapsed = s.welcome_collapsed().unwrap_or(false);
        if welcome_as_sidebar
            && sidebar_dock == welcome_sidebar_dock
            && !sidebar_collapsed
            && !welcome_collapsed
        {
            sidebar_collapsed = true;
        }
        if quick_panel_open && !quick_panel_collapsed {
            if sidebar_dock == quick_panel_dock {
                sidebar_collapsed = true;
            }
            if welcome_as_sidebar && welcome_sidebar_dock == quick_panel_dock {
                welcome_collapsed = true;
            }
        }
        window.set_collapse_sidebar_default(collapse_sidebar);
        window.set_collapse_sftp_default(collapse_sftp);
        // Restore the persisted panel docking layout (#dock).
        window.set_sidebar_width(s.sidebar_width());
        window.set_sidebar_height(s.sidebar_height());
        window.set_sidebar_dock(sidebar_dock.into());
        window.set_sftp_panel_width(s.sftp_panel_width());
        window.set_sftp_panel_height(s.sftp_panel_height());
        window.set_sftp_dock(s.sftp_dock().into());
        window.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
        window.set_quick_panel_open(quick_panel_open);
        window.set_quick_panel_collapsed(quick_panel_collapsed);
        window.set_quick_panel_width(s.quick_panel_width());
        window.set_quick_panel_height(s.quick_panel_height());
        window.set_quick_panel_dock(quick_panel_dock.into());
        window.set_welcome_as_sidebar(welcome_as_sidebar);
        window.set_welcome_sidebar_width(s.welcome_sidebar_width());
        window.set_welcome_sidebar_dock(welcome_sidebar_dock.into());
        window.set_welcome_collapsed(welcome_collapsed);
        window.set_sidebar_collapsed(sidebar_collapsed);
        window.set_wallpaper_overlay(s.wallpaper_overlay());
        window.set_update_check_enabled(s.update_check_enabled()); // #184
        if collapse_sftp {
            window.set_sftp_collapsed(true);
            window.set_sftp_saved_height(s.sftp_panel_height());
        }
        // Capture the user's preferred size. The first native Resized event
        // drives restoration below; this is deterministic and avoids guessing
        // how long Slint/window-manager initialization takes (#278).
        let (ww, wh) = s.window_size();
        let preferred = (ww > 0.0 && wh > 0.0).then_some((ww, wh));
        pending_window_size_restore.set(preferred);
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
        window.on_set_quick_commands_as_sidebar(move |v| {
            let mut s = store.borrow_mut();
            s.set_quick_commands_as_sidebar(v);
            let _ = s.save();
        });
    }
    {
        // Toggle the startup new-version check (#184). Takes effect next launch
        // for the check itself; the banner just won't appear once it's off.
        let store = store.clone();
        window.on_set_update_check_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_update_check_enabled(v);
            let _ = s.save();
        });
    }
    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        window.on_set_renderer_mode(move |mode: SharedString| {
            let mut s = store.borrow_mut();
            s.set_renderer_mode(mode.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_sidebar_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_sidebar_collapsed(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_dock(move |dock| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_dock(dock.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_welcome_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_welcome_collapsed(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_wallpaper_overlay(move |v| {
            let mut s = store.borrow_mut();
            s.set_wallpaper_overlay(v);
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

    // WebDAV config sync (#185): manual upload/download of the portable session
    // export JSON. It is intentionally not automatic on startup.
    {
        let s = store.borrow();
        window.set_webdav_enabled(s.webdav_enabled());
        window.set_webdav_url(s.webdav_url().into());
        window.set_webdav_username(s.webdav_username().into());
        window.set_webdav_password(s.webdav_password().into());
        window.set_webdav_remote_path(s.webdav_remote_path().into());
        window.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
        window.set_webdav_status(String::new().into());
    }
    {
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.to_string(),
                    username.to_string(),
                    password.to_string(),
                    remote_path.to_string(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_upload(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = store.borrow().export_json().and_then(|(json, count)| {
                webdav_put_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                    json,
                )
                .map(|_| count)
            });
            let msg = match res {
                Ok(n) => format!("{} {}", t("已上传连接", "uploaded connections"), n),
                Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            let Some(color) = parse_hex_color(value.as_str()) else {
                return false;
            };
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(value.as_str()) {
                    return false;
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_color(color);
            }
            true
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_add_output_highlight_rule(
            move |pattern: SharedString,
                  is_regex,
                  case_sensitive,
                  whole_line,
                  color: SharedString| {
                let pattern = pattern.trim().to_string();
                let validation = validate_output_highlight_rule(&pattern, is_regex, case_sensitive);
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                if let Err(message) = validation {
                    w.set_output_highlight_rule_status(message.into());
                    return false;
                }
                if store.borrow().output_highlight_rules().len() >= 128 {
                    w.set_output_highlight_rule_status(
                        t("自定义规则最多 128 条", "Custom rules are limited to 128").into(),
                    );
                    return false;
                }
                {
                    let mut s = store.borrow_mut();
                    s.add_output_highlight_rule(OutputHighlightRule {
                        pattern,
                        regex: is_regex,
                        case_sensitive,
                        whole_line,
                        color: color.to_string(),
                        enabled: true,
                    });
                    let _ = s.save();
                    w.set_output_highlight_rules(output_highlight_rule_model(&s));
                    apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                }
                w.set_output_highlight_rule_status("".into());
                true
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_remove_output_highlight_rule(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.remove_output_highlight_rule(index.max(0) as usize);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
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
    // Output highlighting: persist the switch/preset and immediately rebuild
    // every open terminal, including scrollback captured before the change.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
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
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_bold(bold);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_style(move |style: SharedString| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_cursor_style(style.to_string());
                let normalized = s.terminal_cursor_style().to_string();
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
    // Global UI scale (#100): persist the percent and apply it live.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_scale(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 200);
            {
                let mut s = store.borrow_mut();
                s.set_ui_scale(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_scale(clamped as f32 / 100.0);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_panel_font(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 160);
            {
                let mut s = store.borrow_mut();
                s.set_panel_font(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_panel_font(clamped as f32 / 100.0);
            }
        });
    }

    // Wallpaper: pick a built-in / none, or open the file dialog for a custom one.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_set_wallpaper(move |id: SharedString| {
            let id = id.to_string();
            if let Some(w) = weak.upgrade() {
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id);
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            }
            let mut s = store.borrow_mut();
            s.set_wallpaper(id);
            let _ = s.save();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_pick_wallpaper_file(move || {
            let picked = rfd::FileDialog::new()
                .set_title("选择壁纸 / Choose wallpaper")
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            if let Some(path) = picked {
                let id = path.to_string_lossy().to_string();
                if let Some(w) = weak.upgrade() {
                    apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id);
                    if let Some(p) = proc_weak.upgrade() {
                        sync_proc_theme(&w, &p);
                    }
                }
                let mut s = store.borrow_mut();
                s.set_wallpaper(id);
                let _ = s.save();
            }
        });
    }

    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions_model.clone()));
    sync_sessions_to_model(&store.borrow(), &sessions_model);
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = webdav_get_json(
                &url,
                &remote_path,
                &username,
                &password,
                accept_invalid_certs,
            )
            .and_then(|json| store.borrow_mut().import_json(&json));
            let msg = match res {
                Ok((added, skipped)) => {
                    sync_sessions_to_model(&store.borrow(), &sessions_model);
                    format!(
                        "{} {}, {} {}",
                        t("已导入", "imported"),
                        added,
                        t("跳过", "skipped"),
                        skipped
                    )
                }
                Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title_len: tab_title_len(&t("新标签页", "New tab")),
        title: t("新标签页", "New tab").into(),
        kind: "welcome".into(),
        connected: false,
    });
    window.set_tabs(ModelRc::from(tabs_model.clone()));
    window.set_active_tab_id("welcome".into());

    let terminals_model: Rc<VecModel<TerminalState>> = Rc::new(VecModel::default());
    window.set_terminals(ModelRc::from(terminals_model.clone()));

    // Split-pane layout tree (v0.5). Starts as a single pane owning the welcome
    // tab; tab opens/closes/moves mutate it and re-flatten into the `panes`
    // model. `content_size` is the pane-area px size reported from Slint.
    // In welcome-as-sidebar mode the session list lives in a left panel, so the
    // layout starts empty (no "welcome" tab); otherwise it owns the welcome tab.
    let welcome_sidebar = store.borrow().welcome_as_sidebar();
    let layout: Rc<RefCell<crate::layout::Layout>> = Rc::new(RefCell::new(if welcome_sidebar {
        crate::layout::Layout::new(Vec::new(), String::new())
    } else {
        crate::layout::Layout::new(vec!["welcome".into()], "welcome".into())
    }));
    let content_size: Rc<std::cell::Cell<(f32, f32)>> =
        Rc::new(std::cell::Cell::new((1200.0, 800.0)));
    // Persistent pane / splitter models. refresh_panes updates these IN PLACE so
    // the rendered `for pane` / `for sp` elements are reused (terminals survive,
    // and the splitter keeps its pointer-grab during a drag).
    let panes_model: Rc<VecModel<PaneInfo>> = Rc::new(VecModel::default());
    window.set_panes(ModelRc::from(panes_model.clone()));
    let splitters_model: Rc<VecModel<SplitterInfo>> = Rc::new(VecModel::default());
    window.set_splitters(ModelRc::from(splitters_model.clone()));
    refresh_panes(
        &window,
        &layout.borrow(),
        content_size.get(),
        &tabs_model,
        &panes_model,
        &splitters_model,
    );
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_content_resized(move |w: f32, h: f32| {
            content_size.set((w, h));
            if let Some(win) = weak.upgrade() {
                refresh_panes(
                    &win,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
    // Toggle welcome-as-sidebar at runtime: persist, then move the welcome tab in
    // or out of the split-tree (sidebar mode = no welcome tab) and re-flatten.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_set_welcome_as_sidebar(move |v| {
            {
                let mut s = store.borrow_mut();
                s.set_welcome_as_sidebar(v);
                let _ = s.save();
            }
            {
                let mut lay = layout.borrow_mut();
                if v {
                    lay.remove_tab("welcome");
                } else if lay.leaf_of_tab("welcome").is_none() {
                    lay.add_tab("welcome".into());
                }
            }
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
        });
    }
    // Per-session SFTP state: collapse + sizes live in each tab's TerminalState so
    // split panes / other tabs each keep their own (resizing/collapsing one no
    // longer bleeds onto the rest) (#v0.5).
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_collapsed(move |tab_id: SharedString, v: bool| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_collapsed = v);
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_height = v);
            // Mirror to the global default so it persists (saved on close) and
            // seeds new sessions; other open tabs use their own field, unaffected.
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_height(v);
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_width(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_width = v);
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_width(v);
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_saved_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_saved_height = v);
        });
    }

    // Per-tab connection status + remote resources, the latest local sample,
    // and the local machine's network history (bottom sparkline).
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));

    {
        let proc_weak = proc_win.as_weak();
        let handles = handles.clone();
        let statuses = tab_statuses.clone();
        let runtime = runtime.clone();
        proc_win.on_terminate_process(move |tab_id: SharedString, pid: SharedString, password: SharedString| {
            let tab_id = tab_id.to_string();
            let Ok(pid) = pid.parse::<u32>() else {
                set_process_action_error(&proc_weak, t("无效的 PID", "Invalid PID"));
                return;
            };

            // Re-check the source tab, PID, and owner against the latest sample;
            // the main window may have switched tabs since the menu was opened.
            let ownership = {
                let states = statuses.lock().unwrap();
                states.get(&tab_id).map_or_else(
                    || Err(t("当前会话不可用", "The current session is unavailable")),
                    |status| status.procs.iter().find(|p| p.pid == pid)
                        .map(|process| process_needs_root(&status.user, &process.user))
                        .ok_or_else(|| t("进程已退出", "The process has already exited")),
                )
            };
            let needs_root = match ownership {
                Ok(value) => value,
                Err(message) => {
                    set_process_action_error(&proc_weak, message);
                    return;
                }
            };
            if needs_root && password.is_empty() {
                set_process_action_error(
                    &proc_weak,
                    t("请输入管理员（sudo）密码", "Enter the administrator (sudo) password"),
                );
                return;
            }

            let root_password = needs_root.then(|| crate::config::Secret::new(password.to_string()));
            let response = handles.borrow().get(&tab_id)
                .map(|handle| handle.kill_process(pid, root_password));
            let Some(response) = response else {
                set_process_action_error(&proc_weak, t("SSH 会话不可用", "The SSH session is unavailable"));
                return;
            };

            let done_weak = proc_weak.clone();
            runtime.spawn(async move {
                let result = response.await.unwrap_or_else(|_| crate::ssh::ProcessKillResult {
                    success: false,
                    message: t("SSH 会话已关闭", "The SSH session has closed").to_string(),
                });
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(pw) = done_weak.upgrade() {
                        pw.set_action_busy(false);
                        pw.set_action_error(!result.success);
                        pw.set_action_status(result.message.into());
                    }
                });
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
    {
        // Cancel a transfer by id. The id is a UUID unique across sessions, so we
        // broadcast to every SFTP handle — only the owning one has it registered
        // and will act on it (#100).
        let sftp_handles = sftp_handles.clone();
        window.on_cancel_transfer(move |id: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                for h in handles.values() {
                    h.cancel_transfer(id.to_string());
                }
            }
        });
    }

    // --- Construct shared context ----------------------------------------
    let ctx = Rc::new(AppContext {
        store: store.clone(),
        runtime: runtime.clone(),
        handles: handles.clone(),
        sftp_handles: sftp_handles.clone(),
        sftp_last_cwd: sftp_last_cwd.clone(),
        bufs: bufs.clone(),
        render_gates: render_gates.clone(),
        last_term_size: last_term_size.clone(),
        main_window: window.as_weak(),
        proc_window: proc_win.as_weak(),
        sys_window: sys_win.as_weak(),
        sessions_model: sessions_model.clone(),
        tabs_model: tabs_model.clone(),
        terminals_model: terminals_model.clone(),
        layout: layout.clone(),
        content_size: content_size.clone(),
        panes_model: panes_model.clone(),
        splitters_model: splitters_model.clone(),
        transfers_model: transfers_model.clone(),
        tab_statuses: tab_statuses.clone(),
        local_snap: local_snap.clone(),
        local_net_hist: local_net_hist.clone(),
        sftp_follow_cd: sftp_follow_cd.clone(),
    });

    // --- Wire callbacks --------------------------------------------------
    wire_session_callbacks(&window, ctx.clone());

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
                        row.title_len = tab_title_len(&t("新标签页", "New tab"));
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
        let proc_weak = proc_win.as_weak();
        window.on_toggle_theme(move || {
            let Some(w) = weak.upgrade() else { return };
            let next_dark = !w.get_dark_mode();
            // Flip theme + every terminal buffer + re-render (shared with wallpaper).
            apply_dark_mode(&w, &bufs_theme, next_dark);
            // Mirror the flip onto the detached process window (its Theme global
            // is a separate instance) so an open process window follows.
            if let Some(p) = proc_weak.upgrade() {
                sync_proc_theme(&w, &p);
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

    // MFA / keyboard-interactive prompt (#86-MFA): the user enters the
    // verification code (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        window.on_mfa_submit(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mfa_cancel(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, false);
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
    // The open-source link in the About dialog opens the project page.
    window.on_open_repo(move || {
        let url = "https://github.com/jeff141/meatshell";
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
    // Skipped entirely when the user turned the check off (#184).
    if store.borrow().update_check_enabled() {
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let body =
                match ureq::get("https://api.github.com/repos/jeff141/meatshell/releases/latest")
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

    // Open-source libraries shown in the About popup.
    {
        let libs: Vec<SharedString> = [
            t("Slint — 图形界面框架 (GUI)", "Slint — GUI framework"),
            t(
                "russh / russh-keys — SSH 协议实现",
                "russh / russh-keys — SSH protocol",
            ),
            t(
                "russh-sftp — SFTP 文件传输",
                "russh-sftp — SFTP file transfer",
            ),
            t("ssh-key — SSH 密钥解析", "ssh-key — SSH key parsing"),
            t("tokio — 异步运行时", "tokio — async runtime"),
            t(
                "vt100 — 终端 (VT100/xterm) 解析",
                "vt100 — terminal (VT100/xterm) parser",
            ),
            t(
                "sysinfo — 本机资源采集",
                "sysinfo — local resource sampling",
            ),
            t(
                "serde / serde_json — 配置序列化",
                "serde / serde_json — config serialization",
            ),
            t("arboard — 系统剪贴板", "arboard — system clipboard"),
            t("rfd — 原生文件对话框", "rfd — native file dialogs"),
            t(
                "directories — 配置目录定位",
                "directories — config dir lookup",
            ),
            t("chrono — 日期时间处理", "chrono — date/time handling"),
            t("uuid — 唯一标识符", "uuid — unique identifiers"),
            t(
                "anyhow / thiserror — 错误处理",
                "anyhow / thiserror — error handling",
            ),
            t(
                "tracing / tracing-subscriber — 日志",
                "tracing / tracing-subscriber — logging",
            ),
            t(
                "futures / async-trait — 异步辅助",
                "futures / async-trait — async helpers",
            ),
            t("rand — 随机数", "rand — randomness"),
            t(
                "winresource — Windows 图标/资源嵌入",
                "winresource — Windows icon/resource embedding",
            ),
        ]
        .iter()
        .map(|s| (*s).into())
        .collect();
        window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(libs))));
    }

    wire_tab_callbacks(&window, ctx.clone());
    wire_sftp_callbacks(&window, ctx.clone());
    wire_key_input(&window, ctx.clone());

    // --- Window activity, for idle-CPU throttling (#127) ----------------
    // Idle terminals shouldn't burn CPU: pause the sampler when the window is
    // minimized / occluded, throttle it when it's merely unfocused, and stop the
    // cursor blink whenever the window isn't focused (mirrors what Tabby / Windows
    // Terminal do). The winit event handler below updates this; the blink reads
    // Theme.window-focused.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum WinActivity {
        Active,     // focused & visible → full rate
        Background, // visible but unfocused → throttled
        Hidden,     // minimized / occluded → paused
    }
    let activity = Rc::new(std::cell::Cell::new(WinActivity::Active));
    // Once the user confirms shutdown, every subsequent native/custom close
    // request must pass through without reopening the modal. Windows Installer
    // and Restart Manager may issue more than one close request while replacing
    // the executable (#267).
    let exit_confirmed = Rc::new(Cell::new(false));

    // --- System sampler (1 Hz) ------------------------------------------
    let sampler = Rc::new(Mutex::new(SystemSampler::new()));
    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_statuses = tab_statuses.clone();
    let tick_local = local_snap.clone();
    let tick_net = local_net_hist.clone();
    let tick_activity = activity.clone();
    let mut bg_tick = 0u32;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        SystemSampler::recommended_interval(),
        move || {
            // Skip the (non-trivial) sysinfo refresh + sidebar repaint when no one
            // is looking, and back off to ~5 s when the window is in the background.
            match tick_activity.get() {
                WinActivity::Hidden => return,
                WinActivity::Background => {
                    bg_tick = bg_tick.wrapping_add(1);
                    if bg_tick % 5 != 0 {
                        return;
                    }
                }
                WinActivity::Active => {}
            }
            let snap = {
                let mut s = tick_sampler.lock().expect("sampler poisoned");
                s.sample()
            };
            // Append the raw local throughput to the bottom-graph ring buffer
            // (normalisation happens at display time so the graph auto-scales).
            push_ring(&mut tick_net.lock().unwrap(), snap.net_bytes_per_sec as f32);
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
        use i_slint_backend_winit::winit::event::{MouseScrollDelta, WindowEvent as WEvent};
        use i_slint_backend_winit::EventResult;
        let weak = window.as_weak();
        let sh = sftp_handles.clone();
        let wheel_bufs = bufs.clone();
        let close_handles = handles.clone();
        let ev_store = store.clone();
        let ev_activity = activity.clone();
        let ev_exit_confirmed = exit_confirmed.clone();
        let ev_window_size_tracking_ready = window_size_tracking_ready.clone();
        let ev_pending_window_size_restore = pending_window_size_restore.clone();
        let mut last_cursor_logical: Option<(f32, f32)> = None;
        let mut macos_wheel_accum = 0.0_f32;
        // Track the inputs that make up WinActivity; recompute on each change.
        let mut focused = true;
        let mut minimized = false;
        let mut occluded = false;
        // Apply the Win11 rounded-corner hint once, on the first event (the HWND
        // reliably exists by then, unlike a pre-run timer) (#166).
        let mut chrome_done = false;
        window.window().on_winit_window_event(move |slint_window, event| {
            if !chrome_done {
                chrome_done = true;
                if let Some(win) = weak.upgrade() {
                    apply_window_chrome(win.window());
                }
            }
            // Recompute window activity, push it to the shared cell, and update
            // Theme.window-focused (gates the cursor blink) (#127).
            let apply_activity = |focused: bool, minimized: bool, occluded: bool| {
                let act = if minimized || occluded {
                    WinActivity::Hidden
                } else if focused {
                    WinActivity::Active
                } else {
                    WinActivity::Background
                };
                let prev = ev_activity.get();
                ev_activity.set(act);
                if let Some(win) = weak.upgrade() {
                    win.set_window_focused(act == WinActivity::Active);
                    if prev == WinActivity::Hidden && act != WinActivity::Hidden {
                        win.set_terminal_restore_cover(true);
                        let weak2 = weak.clone();
                        slint::Timer::single_shot(
                            std::time::Duration::from_millis(120),
                            move || {
                                if let Some(w) = weak2.upgrade() {
                                    w.set_terminal_restore_cover(false);
                                }
                            },
                        );
                    }
                }
            };
            match event {
                #[cfg(target_os = "windows")]
                WEvent::KeyboardInput { event, .. } => {
                    // Microsoft IME can relabel a Ctrl key-up as Process while
                    // retaining the physical Ctrl scan code. Slint drops Process,
                    // so deliver the missing modifier release directly.
                    if let Some(side) = windows_process_ctrl_release(
                        event.state,
                        &event.logical_key,
                        &event.physical_key,
                    ) {
                        let key = match side {
                            CtrlKeySide::Left => slint::platform::Key::Control,
                            CtrlKeySide::Right => slint::platform::Key::ControlR,
                        };
                        slint_window.dispatch_event(
                            slint::platform::WindowEvent::KeyReleased { text: key.into() },
                        );
                        tracing::debug!(
                            "restored Windows IME Process-key Ctrl release side={side:?}"
                        );
                        return EventResult::PreventDefault;
                    }
                }
                #[cfg(target_os = "windows")]
                WEvent::Ime(i_slint_backend_winit::winit::event::Ime::Disabled) => {
                    // Windows emits Ime::Disabled when a composition ends, including
                    // while switching between Chinese and English input methods. The
                    // Slint winit backend intentionally ignores this notification, so
                    // after several switches the native input context can remain
                    // detached and every TextInput appears to stop accepting keys
                    // (#236). Re-associate the window with its current default IME;
                    // the focused Slint TextInput keeps owning text input as before.
                    slint_window.with_winit_window(|window| window.set_ime_allowed(true));
                }
                WEvent::DroppedFile(path) => {
                    if let Some(win) = weak.upgrade() {
                        handle_file_drop(&win, &sh, path.clone());
                    }
                }
                WEvent::CursorMoved { position, .. } => {
                    if let Some(win) = weak.upgrade() {
                        let scale = win.window().scale_factor().max(0.01) as f64;
                        let p = position.to_logical::<f64>(scale);
                        last_cursor_logical = Some((p.x as f32, p.y as f32));
                    }
                }
                WEvent::MouseWheel { delta, .. } if cfg!(target_os = "macos") => {
                    let Some((x, y)) = last_cursor_logical else {
                        return EventResult::Propagate;
                    };
                    let Some(win) = weak.upgrade() else {
                        return EventResult::Propagate;
                    };
                    let wheel_lines = match delta {
                        MouseScrollDelta::LineDelta(_, dy) => dy * 3.0,
                        MouseScrollDelta::PixelDelta(p) => {
                            let scale = win.window().scale_factor().max(0.01) as f64;
                            let p = p.to_logical::<f64>(scale);
                            p.y as f32 / 18.0
                        }
                    };
                    if wheel_lines.abs() < f32::EPSILON {
                        return EventResult::Propagate;
                    }
                    macos_wheel_accum += wheel_lines;
                    let whole = macos_wheel_accum.trunc() as i32;
                    if whole == 0 {
                        return EventResult::Propagate;
                    }
                    macos_wheel_accum -= whole as f32;
                    if handle_macos_terminal_wheel(&win, &wheel_bufs, x, y, whole) {
                        return EventResult::PreventDefault;
                    }
                }
                WEvent::Focused(f) => {
                    focused = *f;
                    apply_activity(focused, minimized, occluded);
                    if *f {
                        #[cfg(target_os = "windows")]
                        slint_window.with_winit_window(|window| window.set_ime_allowed(true));

                        // Some window managers deliver the first Resized event
                        // before the native window belongs to a monitor. Focus
                        // is a reliable second opportunity to seed restoration;
                        // request_inner_size will produce the Resized event that
                        // verifies the native window actually reached the target.
                        if !ev_window_size_tracking_ready.get() {
                            if let Some(win) = weak.upgrade() {
                                if is_wayland_window(&win.window()) {
                                    ev_pending_window_size_restore.set(None);
                                    ev_window_size_tracking_ready.set(true);
                                    tracing::info!(
                                        "[WINDOW_SIZE] skipped persisted-size restore on Wayland"
                                    );
                                } else if let Some(preferred) =
                                    ev_pending_window_size_restore.get()
                                {
                                    if let Some(target) = clamp_window_size_to_monitor(
                                        &win.window(),
                                        Some(preferred),
                                    ) {
                                        tracing::info!(
                                            "[WINDOW_SIZE] focus retry saved={:.0}x{:.0} \
                                             target={:.0}x{:.0}",
                                            preferred.0,
                                            preferred.1,
                                            target.0,
                                            target.1,
                                        );
                                    }
                                }
                            }
                        }
                        refresh_revealed_main_window(weak.clone());
                    }
                }
                WEvent::Occluded(o) => {
                    occluded = *o;
                    apply_activity(focused, minimized, occluded);
                    if !*o {
                        refresh_revealed_main_window(weak.clone());
                    }
                }
                WEvent::ScaleFactorChanged { .. } => {
                    // Moving a maximized frameless window between mixed-DPI
                    // monitors can leave Win11 reporting "maximized" while the
                    // native rectangle/render surface still has the old size.
                    refresh_revealed_main_window(weak.clone());
                }
                WEvent::Resized(size) => {
                    // A 0-sized resize is how Windows reports a minimize; track it
                    // so we pause the sampler while minimized (#127).
                    minimized = size.width == 0 || size.height == 0;
                    apply_activity(focused, minimized, occluded);
                    // Keep the maximize/restore icon (and resize-edge gating) in
                    // sync when the OS changes the window state (#119).
                    if let Some(win) = weak.upgrade() {
                        let maxed = win
                            .window()
                            .with_winit_window(|ww| ww.is_maximized())
                            .unwrap_or(false);
                        win.set_window_maximized(maxed);
                        if !ev_window_size_tracking_ready.get()
                            && is_wayland_window(&win.window())
                        {
                            // The configure size in this event is authoritative
                            // on Wayland. Accept and persist that actual size;
                            // never chase the advisory saved size (#286).
                            ev_pending_window_size_restore.set(None);
                            ev_window_size_tracking_ready.set(true);
                            tracing::info!(
                                "[WINDOW_SIZE] accepted compositor size {}x{} on Wayland",
                                size.width,
                                size.height
                            );
                        }
                        if !ev_window_size_tracking_ready.get() {
                            if let Some(preferred) = ev_pending_window_size_restore.get() {
                                let scale = win.window().scale_factor().max(0.01);
                                let actual =
                                    (size.width as f32 / scale, size.height as f32 / scale);
                                if let Some(target) =
                                    clamp_window_size_to_monitor(&win.window(), Some(preferred))
                                {
                                    tracing::info!(
                                        "[WINDOW_SIZE] restore requested saved={:.0}x{:.0} \
                                         target={:.0}x{:.0} actual={:.0}x{:.0} scale={:.2}",
                                        preferred.0,
                                        preferred.1,
                                        target.0,
                                        target.1,
                                        actual.0,
                                        actual.1,
                                        scale,
                                    );
                                    if (actual.0 - target.0).abs() <= 2.0
                                        && (actual.1 - target.1).abs() <= 2.0
                                    {
                                        ev_pending_window_size_restore.set(None);
                                        ev_window_size_tracking_ready.set(true);
                                        tracing::info!(
                                            "[WINDOW_SIZE] restore settled at {:.0}x{:.0}",
                                            actual.0,
                                            actual.1
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        "[WINDOW_SIZE] restore deferred: no monitor available \
                                         saved={:.0}x{:.0}",
                                        preferred.0,
                                        preferred.1,
                                    );
                                }
                            } else {
                                // First run: accept the initialized size as the
                                // baseline, but do not persist this startup event.
                                ev_window_size_tracking_ready.set(true);
                            }
                            return EventResult::Propagate;
                        }
                        // Record the last user-adjusted windowed size while the
                        // resize event still carries authoritative native
                        // geometry. Persisting only during CloseRequested can
                        // observe an installer/minimize transition instead
                        // (#278). Keep writes in memory here; save_layout flushes
                        // the config on exit.
                        if ev_window_size_tracking_ready.get() && !maxed && !minimized {
                            let scale = win.window().scale_factor().max(0.01);
                            let width = size.width as f32 / scale;
                            let height = size.height as f32 / scale;
                            if width > 200.0 && height > 200.0 {
                                ev_store.borrow_mut().set_window_size(width, height);
                                tracing::debug!(
                                    "[WINDOW_SIZE] recorded user size {:.0}x{:.0}",
                                    width,
                                    height
                                );
                            }
                        }
                    }
                }
                WEvent::CloseRequested => {
                    // Confirm before closing if there are open session tabs (#88),
                    // so a stray double-click on the title-bar icon / X / Alt+F4
                    // doesn't silently drop live sessions. Installer/Restart
                    // Manager may send repeated requests, so never intercept
                    // again after the user has confirmed shutdown (#267).
                    if should_block_close(
                        ev_exit_confirmed.get(),
                        !close_handles.borrow().is_empty(),
                    ) {
                        if let Some(win) = weak.upgrade() {
                            win.set_confirm_close_open(true);
                        }
                        return EventResult::PreventDefault;
                    }
                    ev_exit_confirmed.set(true);
                    // No sessions → the window is about to close; persist layout.
                    if let Some(win) = weak.upgrade() {
                        save_layout(&win, &ev_store);
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }
    // Confirm-close dialog "Close" → actually quit the event loop (#88).
    {
        let weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        let sys_weak = sys_win.as_weak();
        let cc_store = store.clone();
        let close_handles = handles.clone();
        let close_sftp_handles = sftp_handles.clone();
        let close_exit_confirmed = exit_confirmed.clone();
        window.on_confirm_close_yes(move || {
            // Guard against a double click and against another close request
            // arriving from Windows Installer while shutdown is in progress.
            if close_exit_confirmed.replace(true) {
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.set_confirm_close_open(false);
                save_layout(&w, &cc_store);
                let _ = w.hide();
            }
            if let Some(w) = proc_weak.upgrade() {
                let _ = w.hide();
            }
            if let Some(w) = sys_weak.upgrade() {
                let _ = w.hide();
            }
            // Ask every worker to stop before the runtime/event loop is torn
            // down. Clearing the maps also makes any repeated close request see
            // no live sessions and pass through immediately.
            {
                let mut sessions = close_handles.borrow_mut();
                for handle in sessions.values() {
                    handle.close();
                }
                sessions.clear();
            }
            if let Ok(mut sftp) = close_sftp_handles.lock() {
                for handle in sftp.values() {
                    handle.close();
                }
                sftp.clear();
            }
            let _ = slint::quit_event_loop();
        });
    }

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
        let wc_store = store.clone();
        let wc_exit_confirmed = exit_confirmed.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if !should_block_close(
                    wc_exit_confirmed.get(),
                    !close_handles.borrow().is_empty(),
                ) {
                    wc_exit_confirmed.set(true);
                    save_layout(&w, &wc_store);
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
                schedule_slint_pointer_ungrab(weak.clone());
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
                schedule_slint_pointer_ungrab(weak.clone());
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





/// Per-session scrollback cap (recycled on clear / tab close).
pub(crate) const MAX_HISTORY: usize = 100_000;


#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn windows_process_key_ctrl_release_keeps_physical_side() {
        use i_slint_backend_winit::winit::event::ElementState;
        use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

        let process = Key::Named(NamedKey::Process);
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::ControlLeft),
            ),
            Some(CtrlKeySide::Left)
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::ControlRight),
            ),
            Some(CtrlKeySide::Right)
        );
    }

    #[test]
    fn windows_process_key_recovery_ignores_other_key_events() {
        use i_slint_backend_winit::winit::event::ElementState;
        use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

        let process = Key::Named(NamedKey::Process);
        let left_ctrl = PhysicalKey::Code(KeyCode::ControlLeft);
        assert_eq!(
            windows_process_ctrl_release(ElementState::Pressed, &process, &left_ctrl),
            None
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &Key::Named(NamedKey::Control),
                &left_ctrl,
            ),
            None
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::KeyC),
            ),
            None
        );
    }

    #[test]
    fn bare_alt_is_not_forwarded() {
        // Slint sends Alt-alone as key=0x12 with alt=true. It must produce no
        // bytes — otherwise it becomes ESC+0x12 and clears the input (issue #43).
        assert_eq!(
            key_to_pty_bytes("\u{0012}", false, true, false),
            Vec::<u8>::new()
        );
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
        // forwarded; the #274 fix filters only bare Ctrl/CtrlR markers.
        assert_eq!(key_to_pty_bytes("\u{0012}", true, false, false), vec![0x12]);
        // Ctrl+X as C0 0x18.
        assert_eq!(key_to_pty_bytes("\u{0018}", true, false, false), vec![0x18]);
    }

    #[test]
    fn debian_bare_ctrl_markers_do_not_reach_nano() {
        // Slint on Debian emits these before the actual Ctrl+letter event.
        assert!(should_drop_debian_bare_ctrl_marker("\u{0011}", true, true));
        assert!(should_drop_debian_bare_ctrl_marker("\u{0016}", true, true));
        // Other platforms retain their existing direct-C0 behaviour.
        assert!(!should_drop_debian_bare_ctrl_marker(
            "\u{0011}",
            true,
            false
        ));
        assert!(!should_drop_debian_bare_ctrl_marker("x", true, true));
        // The following Ctrl+X must still become CAN (0x18), which nano uses
        // for Exit.
        assert_eq!(key_to_pty_bytes("x", true, false, false), vec![0x18]);
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

    #[test]
    fn paste_uses_remote_bracketed_paste_mode() {
        assert_eq!(
            encode_pasted_text("first\r\n  second", true),
            b"\x1b[200~first\r\n  second\x1b[201~"
        );
        assert_eq!(
            encode_pasted_text("safe\x1b[201~\x03text", true),
            b"\x1b[200~safe[201~text\x1b[201~"
        );
        assert_eq!(
            encode_pasted_text("first\r\nsecond", false),
            b"first\rsecond"
        );
    }

    #[test]
    fn long_pastes_switch_to_large_review() {
        assert!(!paste_requires_large_review("short prompt\nsecond line"));
        assert!(!paste_requires_large_review(&"a".repeat(600)));
        assert!(paste_requires_large_review(&"a".repeat(601)));
        assert!(!paste_requires_large_review(&vec!["line"; 12].join("\r\n")));
        assert!(paste_requires_large_review(&vec!["line"; 13].join("\r\n")));
    }

    #[test]
    fn confirmed_exit_never_reopens_close_prompt() {
        assert!(should_block_close(false, true));
        assert!(!should_block_close(false, false));
        assert!(!should_block_close(true, true));
        assert!(!should_block_close(true, false));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::terminal::Line;

    fn sftp_entry(name: &str, is_dir: bool) -> SftpEntry {
        SftpEntry {
            name: name.into(),
            full_path: format!("/{name}").into(),
            is_dir,
            size: String::new().into(),
            size_bytes: 0.0,
            modified: String::new().into(),
            modified_ts: 0.0,
            mode: 0,
            selected: false,
        }
    }

    fn sftp_names(entries: &[SftpEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.to_string()).collect()
    }

    #[test]
    fn sftp_name_sort_uses_natural_numeric_order() {
        let mut entries = vec![
            sftp_entry("file100", false),
            sftp_entry("file10", false),
            sftp_entry("file2", false),
            sftp_entry("file11", false),
            sftp_entry("file1", false),
        ];
        sort_sftp_entries(&mut entries, "name", 1);
        assert_eq!(
            sftp_names(&entries),
            vec!["file1", "file2", "file10", "file11", "file100"]
        );

        sort_sftp_entries(&mut entries, "name", -1);
        assert_eq!(
            sftp_names(&entries),
            vec!["file100", "file11", "file10", "file2", "file1"]
        );
    }

    #[test]
    fn sftp_default_sort_keeps_dirs_first_with_natural_names() {
        let mut entries = vec![
            sftp_entry("file100", false),
            sftp_entry("dir10", true),
            sftp_entry("file11", false),
            sftp_entry("dir2", true),
        ];
        sort_sftp_entries(&mut entries, "", 0);
        assert_eq!(sftp_names(&entries), vec!["dir2", "dir10", "file11", "file100"]);
    }

    fn hist_line(s: &str) -> Line {
        (s.to_string(), Vec::new(), false)
    }

    fn wrapped_hist_line(s: &str) -> Line {
        (s.to_string(), Vec::new(), true)
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
            output_highlight: OutputHighlightPreset::Log,
            custom_highlight_rules: Vec::new(),
            sel_anchor: None,
            sel_focus: None,
            sel_ranges: Vec::new(),
            history: history.iter().map(|s| hist_line(s)).collect(),
            prev: Vec::new(),
            view_offset,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            raw: std::collections::VecDeque::new(),
        }
    }

    #[test]
    fn paste_tracks_remote_bracketed_paste_state() {
        let bufs = TermBuffers::default();
        let mut buffer = make_buf(2, 20, &[], &[], 0);
        buffer.parser.process(b"\x1b[?2004h");
        bufs.lock()
            .unwrap()
            .insert("tab".into(), Arc::new(Mutex::new(buffer)));

        assert!(terminal_uses_bracketed_paste(&bufs, "tab"));
        assert!(!terminal_uses_bracketed_paste(&bufs, "missing"));

        let buffer = term_buf(&bufs, "tab").unwrap();
        buffer.lock().unwrap().parser.process(b"\x1b[?2004l");
        assert!(!terminal_uses_bracketed_paste(&bufs, "tab"));
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
            let mut b = make_buf(
                5,
                20,
                &["HIST0", "HIST1", "HIST2"],
                &["LIVE0", "LIVE1"],
                off,
            );
            b.sel_anchor = Some((0, 0));
            b.sel_focus = Some((4, 19));
            b.extract_selection_text()
        };
        assert_eq!(sel(3), sel(0));
        assert_eq!(sel(3), "HIST0\nHIST1\nHIST2\nLIVE0\nLIVE1");
    }

    #[test]
    fn extract_joins_soft_wrapped_rows() {
        let mut buf = make_buf(5, 10, &[], &["x"], 0);
        buf.history = vec![
            wrapped_hist_line("0123456789"),
            wrapped_hist_line("abcdefghij"),
            hist_line("klmnop"),
            hist_line("next"),
        ];
        buf.sel_anchor = Some((0, 0));
        buf.sel_focus = Some((3, 9));
        assert_eq!(
            buf.extract_selection_text(),
            "0123456789abcdefghijklmnop\nnext"
        );
    }

    #[test]
    fn highlight_clipped_to_current_view() {
        // Scrolled to the top: a history selection is on-screen and highlighted.
        let mut top = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], 3);
        top.sel_anchor = Some((0, 2));
        top.sel_focus = Some((2, 4));
        let rects = top.selection_rects_visible(20);
        assert_eq!(
            rects.len(),
            3,
            "rows 0,1,2 (the 3 history lines) highlighted"
        );
        assert_eq!(rects[0].row, 0);
        assert_eq!(rects[2].row, 2);

        // At the live bottom the same history selection is scrolled off → none.
        let mut live = make_buf(5, 20, &["HIST0", "HIST1", "HIST2"], &["LIVE0", "LIVE1"], 0);
        live.sel_anchor = Some((0, 2));
        live.sel_focus = Some((2, 4));
        assert!(live.selection_rects_visible(20).is_empty());
    }

    #[test]
    fn extract_handles_wide_cjk_columns() {
        // Regression for #132: copying after CJK glyphs drifted right by the
        // number of wide chars before the selection (e.g. selecting "1pctl"
        // yielded "ctl…"). The history line lays out on the grid as:
        //   提(0-1) 示(2-3) :(4) space(5) 1(6) p(7) c(8) t(9) l(10)
        let mut buf = make_buf(5, 20, &["提示: 1pctl"], &["x"], 0);

        // The "1pctl" run sits at grid cols 6..=10.
        buf.sel_anchor = Some((0, 6));
        buf.sel_focus = Some((0, 10));
        assert_eq!(buf.extract_selection_text(), "1pctl");

        // Selecting from the second CJK glyph through the end.
        buf.sel_anchor = Some((0, 2));
        buf.sel_focus = Some((0, 10));
        assert_eq!(buf.extract_selection_text(), "示: 1pctl");

        // Anchoring on the *second* cell of a wide glyph still grabs the whole
        // glyph — you can't half-select a CJK char.
        buf.sel_anchor = Some((0, 3));
        buf.sel_focus = Some((0, 10));
        assert_eq!(buf.extract_selection_text(), "示: 1pctl");
    }

    #[test]
    fn find_matches_report_grid_columns_past_cjk() {
        // Highlight rects must sit at the GRID column, not the char index, so
        // they line up over the text after CJK glyphs (#132).
        let rows = vec!["提示: 1pctl".to_string()];
        let m = compute_find_matches(&rows, "1pctl");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].col, 6, "grid column 6, not char index 4");
        assert_eq!(m[0].len, 5);

        // A CJK query spans two grid cells per glyph.
        let m2 = compute_find_matches(&rows, "提示");
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].col, 0);
        assert_eq!(m2[0].len, 4, "two wide glyphs span four grid cells");
    }

    #[test]
    fn inverse_default_colours_paint_a_visible_background() {
        let (fg, bg) = vt_span_colors(
            vt100::Color::Default,
            vt100::Color::Default,
            false,
            true,
            true,
        );
        assert_eq!(fg.as_argb_encoded(), 0xff0e0f13);
        assert_eq!(bg.as_argb_encoded(), 0xffd4d4d4);

        let mut parser = vt100::Parser::new(3, 30, 0);
        parser.process(b"abc \x1b[7m20260705\x1b[27m end");
        let (_plain, runs, _wrapped) = build_row(parser.screen(), 0, 30);
        let hit = runs
            .iter()
            .find(|span| span.text.contains("20260705"))
            .expect("reverse-video search hit should be a separate span");
        assert!(hit.inverse);
        assert!(matches!(hit.fg, vt100::Color::Default));
        assert!(matches!(hit.bg, vt100::Color::Default));
    }
}

#[cfg(test)]
mod log_highlight_tests {
    use super::*;
    use crate::terminal::{CompiledOutputRule, HistSpan};

    fn plain_run(text: &str, col: i32) -> HistSpan {
        HistSpan {
            text: text.to_string(),
            fg: vt100::Color::Default,
            bg: vt100::Color::Default,
            bold: false,
            inverse: false,
            col,
            cells: text.chars().count() as i32,
        }
    }

    fn custom_rule(
        pattern: &str,
        regex: bool,
        case_sensitive: bool,
        whole_line: bool,
        color: &str,
    ) -> CompiledOutputRule {
        compile_output_rules(&[OutputHighlightRule {
            pattern: pattern.to_string(),
            regex,
            case_sensitive,
            whole_line,
            color: color.to_string(),
            enabled: true,
        }])
        .pop()
        .expect("test rule should compile")
    }

    #[test]
    fn highlights_uppercase_level_and_preserves_columns() {
        let runs = highlight_plain_output(
            vec![plain_run(
                "2026-07-14T10:20:30Z ERROR request failed",
                0,
            )],
            OutputHighlightPreset::Log,
            &[],
        );
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].text, "ERROR");
        assert_eq!(runs[1].col, 21);
        assert_eq!(runs[1].cells, 5);
        assert!(runs[1].bold);
        assert!(matches!(runs[1].fg, vt100::Color::Idx(9)));
        assert_eq!(runs[2].col, 26);
    }

    #[test]
    fn highlights_structured_lowercase_level_only() {
        let json = r#"{"level":"warn","message":"disk nearly full"}"#;
        let runs = highlight_plain_output(
            vec![plain_run(json, 4)],
            OutputHighlightPreset::Log,
            &[],
        );
        let level = runs
            .iter()
            .find(|run| run.text == "warn")
            .expect("structured level should be highlighted");
        assert!(matches!(level.fg, vt100::Color::Idx(11)));

        assert!(log_level_marker("an error occurred", 96).is_none());
        assert!(log_level_marker("ERROR_CODE=5", 96).is_none());
    }

    #[test]
    fn preserves_existing_ansi_styles() {
        let mut coloured = plain_run("ERROR", 0);
        coloured.fg = vt100::Color::Idx(2);
        let runs = highlight_plain_output(vec![coloured], OutputHighlightPreset::Log, &[]);
        assert_eq!(runs.len(), 1);
        assert!(matches!(runs[0].fg, vt100::Color::Idx(2)));
        assert!(!runs[0].bold);
    }

    #[test]
    fn alternate_screen_does_not_add_log_colours() {
        let mut parser = vt100::Parser::new(3, 30, 0);
        parser.process(b"\x1b[?1049hERROR");
        assert!(parser.screen().alternate_screen());
        let (_plain, runs, _wrapped) = build_row(parser.screen(), 0, 30);
        let level = runs
            .iter()
            .find(|run| run.text.contains("ERROR"))
            .expect("alternate-screen text should still render");
        assert!(matches!(level.fg, vt100::Color::Default));
        assert!(!level.bold);
    }

    #[test]
    fn off_preset_leaves_plain_levels_untouched() {
        let runs = highlight_plain_output(
            vec![plain_run("ERROR request failed", 0)],
            OutputHighlightPreset::Off,
            &[],
        );
        assert_eq!(runs.len(), 1);
        assert!(matches!(runs[0].fg, vt100::Color::Default));
        assert!(!runs[0].bold);
    }

    #[test]
    fn devops_preset_adds_deployment_and_structured_states() {
        let success = highlight_plain_output(
            vec![plain_run("deploy SUCCESS", 0)],
            OutputHighlightPreset::DevOps,
            &[],
        );
        let token = success
            .iter()
            .find(|run| run.text == "SUCCESS")
            .expect("DevOps success should be highlighted");
        assert!(matches!(token.fg, vt100::Color::Idx(10)));

        let json = highlight_plain_output(
            vec![plain_run(r#"{"status":"failed"}"#, 0)],
            OutputHighlightPreset::DevOps,
            &[],
        );
        let token = json
            .iter()
            .find(|run| run.text == "failed")
            .expect("structured DevOps state should be highlighted");
        assert!(matches!(token.fg, vt100::Color::Idx(9)));

        let conservative = highlight_plain_output(
            vec![plain_run("deploy SUCCESS", 0)],
            OutputHighlightPreset::Log,
            &[],
        );
        assert_eq!(conservative.len(), 1);
    }

    #[test]
    fn custom_literal_is_case_insensitive_and_overrides_builtin_colour() {
        let rule = custom_rule("error", false, false, false, "green");
        let runs = highlight_plain_output(
            vec![plain_run("ERROR then error", 0)],
            OutputHighlightPreset::Log,
            &[rule],
        );
        let hits: Vec<_> = runs
            .iter()
            .filter(|run| matches!(run.fg, vt100::Color::Idx(10)))
            .collect();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "ERROR");
        assert_eq!(hits[1].text, "error");
        assert!(!runs.iter().any(|run| matches!(run.fg, vt100::Color::Idx(9))));
    }

    #[test]
    fn custom_regex_can_highlight_whole_line_without_overwriting_ansi() {
        let rule = custom_rule(r"timeout|denied", true, false, true, "magenta");
        let mut ansi = plain_run(" ANSI", 18);
        ansi.fg = vt100::Color::Idx(2);
        let runs = highlight_plain_output(
            vec![plain_run("request timeout   ", 0), ansi],
            OutputHighlightPreset::Log,
            &[rule],
        );
        assert!(matches!(runs[0].fg, vt100::Color::Idx(13)));
        assert!(runs[0].bold);
        assert!(matches!(runs[1].fg, vt100::Color::Idx(2)));
    }

    #[test]
    fn custom_unicode_match_preserves_terminal_grid_columns() {
        let rule = custom_rule("错误", false, true, false, "red");
        let text = "前缀错误 done";
        let mut run = plain_run(text, 0);
        run.cells = text_cell_width(text);
        let runs = highlight_plain_output(
            vec![run],
            OutputHighlightPreset::Log,
            &[rule],
        );
        let hit = runs
            .iter()
            .find(|run| run.text == "错误")
            .expect("CJK keyword should be highlighted");
        assert_eq!(hit.col, 4);
        assert_eq!(hit.cells, 4);
    }

    #[test]
    fn invalid_regex_is_rejected_before_persistence() {
        assert!(validate_output_highlight_rule("([", true, false).is_err());
        assert!(validate_output_highlight_rule("literal", false, false).is_ok());
    }
}
