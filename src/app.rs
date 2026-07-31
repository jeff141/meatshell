//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// Keeps a single UI callback from spending hundreds of ms in vt100 ingest.
const OUTPUT_MERGE_BYTE_CAP: usize = 64 * 1024;

/// Output parsed between UI-flush checkpoints during sustained traffic.
const INGEST_FRAME_BUDGET: usize = 64 * 1024;

/// A busy or closing UI must never block a session pump indefinitely.
const UI_FLUSH_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// Do not deliberately pace a pump while a large unbounded-channel backlog is
/// already present. It catches up first, then paces the tail of the stream.
const PACED_LOCAL_BACKLOG_LIMIT: usize = 1024 * 1024;
const PACED_QUEUE_EVENT_LIMIT: usize = 256;

/// Max UI renders per second for a tab under sustained output (#209).
const RENDER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

fn term_buf(bufs: &TermBuffers, tab_id: &str) -> Option<TermBufferHandle> {
    bufs.lock().unwrap().get(tab_id).cloned()
}

fn with_term_buf<R>(
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

fn record_ingested_chunk(chunk_len: usize, ingested_since_checkpoint: &mut usize) -> bool {
    debug_assert!(*ingested_since_checkpoint < INGEST_FRAME_BUDGET);
    if chunk_len == 0 {
        return false;
    }

    let remaining = INGEST_FRAME_BUDGET - *ingested_since_checkpoint;
    if chunk_len < remaining {
        *ingested_since_checkpoint += chunk_len;
        false
    } else {
        *ingested_since_checkpoint = (chunk_len - remaining) % INGEST_FRAME_BUDGET;
        true
    }
}

fn event_requires_immediate_ui(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Connected
            | SessionEvent::Closed(_)
            | SessionEvent::HostKeyPrompt { .. }
            | SessionEvent::CredentialPrompt { .. }
            | SessionEvent::MfaPrompt { .. }
    )
}

#[cfg(test)]
mod ingest_frame_tests {
    use super::{event_requires_immediate_ui, record_ingested_chunk, INGEST_FRAME_BUDGET};
    use crate::ssh::SessionEvent;

    fn count_requests(chunk_lengths: &[usize]) -> (usize, usize) {
        let mut since_checkpoint = 0usize;
        let mut requests = 0usize;
        let mut dirty_since_request = false;
        for &chunk_len in chunk_lengths {
            dirty_since_request = true;
            if record_ingested_chunk(chunk_len, &mut since_checkpoint) {
                requests += 1;
                dirty_since_request = false;
            }
        }
        if dirty_since_request {
            requests += 1;
        }
        (requests, since_checkpoint)
    }

    #[test]
    fn exact_frame_budget_chunks_do_not_add_an_empty_tail_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET]);
        assert_eq!(requests, 2);
        assert_eq!(remainder, 0);
    }

    #[test]
    fn a_partial_tail_gets_one_final_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET, 1]);
        assert_eq!(requests, 3);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn checkpoint_budget_carries_across_input_events() {
        let mut since_checkpoint = 0usize;
        assert!(!record_ingested_chunk(
            INGEST_FRAME_BUDGET - 1,
            &mut since_checkpoint
        ));
        assert!(record_ingested_chunk(1, &mut since_checkpoint));
        assert_eq!(since_checkpoint, 0);
    }

    #[test]
    fn an_oversized_output_event_stays_one_atomic_checkpoint() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET * 2 + 1]);
        assert_eq!(requests, 1);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn routine_shell_metadata_does_not_disable_tail_pacing() {
        assert!(!event_requires_immediate_ui(&SessionEvent::CommandRan(
            "tail -n 1000000 app.log".into()
        )));
        assert!(!event_requires_immediate_ui(&SessionEvent::CwdChanged(
            "/var/log".into()
        )));
        assert!(event_requires_immediate_ui(&SessionEvent::Connected));
        assert!(event_requires_immediate_ui(&SessionEvent::Closed(
            "connection lost".into()
        )));
    }
}

use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;

use crate::config::{
    AuthMethod, ConfigStore, OutputHighlightRule, Secret, Session, SessionKind,
};
use crate::i18n::t;
use crate::layout::{LogicalRect, TerminalWheelHit};
use crate::resource::{
    LocalGpuInfo, LocalHardwareInfo, LocalSnap, NetHist, TabStatus, TabStatuses,
};
use crate::session::{ConnectCtx, PendingCred, PendingHostKey, PendingMfa};
use crate::sftp::{spawn_sftp, SftpHandles, SftpLastCwd};
use crate::ssh::{
    format_mtime, format_size, spawn_session, test_session_auth, ProcInfo, SessionCommand,
    SessionEvent, SessionHandle, SystemDetails,
};
use crate::terminal::{
    bare_ctrl_marker_workaround_enabled, cell_prefix, compile_output_rules,
    encode_command_bar_input, encode_pasted_text, key_to_pty_bytes, paste_requires_large_review,
    should_drop_bare_ctrl_marker, terminal_uses_bracketed_paste, CsiState,
    OutputHighlightPreset, RenderGates, TabRenderGate, TermBuffer, TermBufferHandle, TermBuffers,
};
#[cfg(test)]
use crate::terminal::{
    build_row, highlight_plain_output, log_level_marker, normalize_pasted_newlines,
    text_cell_width, vt_span_colors, CompiledOutputRule, HistSpan, Line,
};
#[cfg(windows)]
use crate::terminal::c0_letter_key_down;
#[cfg(any(target_os = "windows", test))]
use crate::terminal::{windows_process_ctrl_release, CtrlKeySide};
use crate::resource::system::{format_bytes_per_sec, format_mem, SystemSampler, SystemSnapshot};
use crate::ui::*;
use crate::webdav::WebDavAcceptAnyCertVerifier;

fn tab_title_len(title: &str) -> i32 {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1usize } else { 2usize })
        .sum::<usize>()
        .min(i32::MAX as usize) as i32
}


fn should_block_close(exit_confirmed: bool, has_live_sessions: bool) -> bool {
    !exit_confirmed && has_live_sessions
}

/// Tab ids currently shown in a pane (`term.id == pane.active-id` in Slint).
fn visible_tab_ids(win: &AppWindow) -> HashSet<String> {
    use slint::Model as _;
    let mut out = HashSet::new();
    let panes = win.get_panes();
    if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
        for i in 0..pm.row_count() {
            if let Some(pane) = pm.row_data(i) {
                out.insert(pane.active_id.to_string());
            }
        }
    }
    out
}

struct TabRenderTicket {
    gate: Arc<TabRenderGate>,
    generation: u64,
}

fn register_tab_render_request(
    tab_id: &str,
    gates: &RenderGates,
) -> Option<(Arc<TabRenderGate>, TabRenderTicket, bool)> {
    let gate = {
        let map = gates.lock().unwrap();
        map.get(tab_id).cloned()
    }?;
    let (generation, should_schedule) = gate.request()?;
    let ticket = TabRenderTicket {
        gate: gate.clone(),
        generation,
    };
    Some((gate, ticket, should_schedule))
}

fn request_tab_render(
    weak: slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) -> Option<TabRenderTicket> {
    let (gate, ticket, should_schedule) = register_tab_render_request(tab_id, gates)?;
    if !should_schedule {
        return Some(ticket);
    }

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();
    let gate2 = gate.clone();
    // Always bounce through the event loop from pump / worker threads.
    // Never call invoke_from_event_loop from inside a UI callback — that
    // deadlocks Slint (opening a second tab then froze the whole app).
    if slint::invoke_from_event_loop(move || {
        run_coalesced_tab_render(&weak2, &tid, &bufs2, gate2);
    })
    .is_err()
    {
        // The event loop is gone. Wake any pump waiting on this ticket and
        // reject future requests instead of leaving the gate scheduled forever.
        gate.close();
    }
    Some(ticket)
}

/// UI-thread variant for synthetic Output events. It shares the same gate but
/// enters the throttle directly because invoking Slint from its own callback
/// can deadlock.
fn request_tab_render_from_ui(
    weak: slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) {
    let Some((gate, _, should_schedule)) = register_tab_render_request(tab_id, gates) else {
        return;
    };
    if should_schedule {
        run_coalesced_tab_render(&weak, tab_id, bufs, gate);
    }
}

fn wait_for_ui_flush(ticket: Option<TabRenderTicket>) {
    if let Some(ticket) = ticket {
        let _ = ticket
            .gate
            .wait_for(ticket.generation, UI_FLUSH_ACK_TIMEOUT);
    }
}

/// UI-thread entry: honour the throttle, then render. Timer must be created
/// here — not on pump threads (#209).
fn run_coalesced_tab_render(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gate: Arc<TabRenderGate>,
) {
    let delay = gate.flush_delay(RENDER_MIN_INTERVAL);

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();

    if delay.is_zero() {
        do_tab_render_flush(&weak2, &tid, &bufs2, gate);
    } else {
        slint::Timer::single_shot(delay, move || {
            do_tab_render_flush(&weak2, &tid, &bufs2, gate);
        });
    }
}

/// UI-thread only: commit the vt100 snapshot to Slint's model, then reschedule
/// if output arrived after this snapshot began. `request_redraw` is asynchronous,
/// so completion acknowledges a model flush rather than GPU presentation.
fn do_tab_render_flush(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gate: Arc<TabRenderGate>,
) {
    let Some(through) = gate.begin_flush() else {
        return;
    };

    let visible = if let Some(win) = weak.upgrade() {
        if visible_tab_ids(&win).contains(tab_id) {
            rebuild_tab_display(&win, bufs, tab_id);
            true
        } else {
            false
        }
    } else {
        false
    };

    if gate.finish_flush(through, visible) {
        let weak2 = weak.clone();
        let tid = tab_id.to_string();
        let bufs2 = bufs.clone();
        // Defer the continuation to avoid recursive flushes for hidden tabs,
        // whose last-visible timestamp intentionally does not throttle them.
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            run_coalesced_tab_render(&weak2, &tid, &bufs2, gate);
        });
    }
}

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
    let Ok(img) = image::load_from_memory(ICON_PNG) else {
        return;
    };
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let Ok(icon) = Icon::from_rgba(rgba.into_raw(), w, h) else {
        return;
    };
    window
        .window()
        .with_winit_window(|ww| ww.set_window_icon(Some(icon)));
}

/// On Windows, keep the frameless Slint surface and the native hit-test surface
/// aligned. Some Win10 systems expose winit's undecorated-shadow compatibility
/// frame as a real non-client strip, which shifts hit testing (#193).
#[cfg(windows)]
fn apply_window_chrome(window: &slint::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window.with_winit_window(|ww| {
        let Ok(handle) = ww.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        let hwnd = h.hwnd.get();

        #[link(name = "dwmapi")]
        extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *const core::ffi::c_void,
                cb: u32,
            ) -> i32;
        }
        // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2 (Windows 11+).
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        unsafe {
            let pref: u32 = DWMWCP_ROUND;
            let corner_hr = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&pref as *const u32).cast(),
                4,
            );
            tracing::debug!(
                "window chrome applied: hwnd={hwnd:#x} corner_hr={corner_hr:#x}"
            );
        }
    });
}

#[cfg(not(windows))]
fn apply_window_chrome(_window: &slint::Window) {}

#[cfg(windows)]
fn setup_windows_platform(renderer_mode: &str) {
    use i_slint_backend_winit::winit::platform::windows::WindowAttributesExtWindows;

    let mut builder = i_slint_backend_winit::Backend::builder();
    let configured_renderer = match renderer_mode {
        "gpu" => Some("femtovg".to_owned()),
        "software" => Some("software".to_owned()),
        _ => None,
    };
    // Any explicit environment value wins, including plain "winit" (automatic
    // renderer selection). This keeps the existing diagnostic escape hatch.
    let env_backend = std::env::var("SLINT_BACKEND").ok();
    let renderer = match env_backend.as_deref() {
        Some(backend) => backend
            .strip_prefix("winit-")
            .filter(|renderer| !renderer.is_empty())
            .map(str::to_owned),
        None => configured_renderer,
    };
    if let Some(renderer) = renderer.as_ref() {
        builder = builder.with_renderer_name(renderer.clone());
    }
    tracing::info!(
        renderer_mode,
        renderer = renderer.as_deref().unwrap_or("auto"),
        source = if env_backend.is_some() {
            "SLINT_BACKEND"
        } else {
            "settings"
        },
        "initializing Windows renderer"
    );
    let backend = builder
        .with_window_attributes_hook(|attrs| {
            attrs
                .with_transparent(false)
                .with_undecorated_shadow(false)
        })
        .build();

    match backend {
        Ok(backend) => {
            if slint::platform::set_platform(Box::new(backend)).is_err() {
                tracing::warn!("Windows winit backend was already initialized");
            }
        }
        Err(err) => tracing::warn!("failed to initialize Windows winit backend: {err}"),
    }
}

fn clamp_window_size_to_monitor(
    window: &slint::Window,
    preferred: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    use i_slint_backend_winit::winit::dpi::{LogicalPosition, LogicalSize};

    window.with_winit_window(|ww| {
        #[cfg(target_os = "linux")]
        {
            use i_slint_backend_winit::winit::platform::wayland::WindowExtWayland;

            // Wayland compositors own the final surface size. A
            // request_inner_size call is only advisory and KWin may configure a
            // different size, leaving Slint's rendered and input geometries out
            // of sync (#286). Let the compositor choose the startup size.
            if ww.xdg_toplevel().is_some() {
                return None;
            }
        }

        let scale = ww.scale_factor().max(0.01);
        // Before `Window::run()` makes the native window visible, winit often
        // has no current monitor yet. Falling back to the primary monitor lets
        // the persisted size actually apply during startup (#278).
        let monitor = ww.current_monitor().or_else(|| ww.primary_monitor())?;
        let monitor_size = monitor.size();
        let monitor_pos = monitor.position();
        let max_w = (monitor_size.width as f64 / scale - 16.0).max(1.0) as f32;
        let max_h = (monitor_size.height as f64 / scale - 16.0).max(1.0) as f32;
        let min_w = 960.0_f32.min(max_w);
        let min_h = 600.0_f32.min(max_h);
        let current = ww.inner_size();
        let current_w = (current.width as f64 / scale) as f32;
        let current_h = (current.height as f64 / scale) as f32;
        let (want_w, want_h) = preferred.unwrap_or((current_w, current_h));
        let target_w = want_w.clamp(min_w, max_w);
        let target_h = want_h.clamp(min_h, max_h);

        if (target_w - current_w).abs() > 0.5
            || (target_h - current_h).abs() > 0.5
            || preferred.is_some()
        {
            let _ = ww.request_inner_size(LogicalSize::new(target_w as f64, target_h as f64));
        }

        if (target_w - want_w).abs() > 0.5 || (target_h - want_h).abs() > 0.5 {
            let mon_w = monitor_size.width as f64 / scale;
            let mon_h = monitor_size.height as f64 / scale;
            let mon_x = monitor_pos.x as f64 / scale;
            let mon_y = monitor_pos.y as f64 / scale;
            ww.set_outer_position(LogicalPosition::new(
                mon_x + (mon_w - target_w as f64).max(0.0) / 2.0,
                mon_y + (mon_h - target_h as f64).max(0.0) / 2.0,
            ));
        }

        Some((target_w, target_h))
    })?
}

#[cfg(target_os = "linux")]
fn is_wayland_window(window: &slint::Window) -> bool {
    use i_slint_backend_winit::winit::platform::wayland::WindowExtWayland;

    window
        .with_winit_window(|ww| ww.xdg_toplevel().is_some())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn is_wayland_window(_window: &slint::Window) -> bool {
    false
}

/// Detect the Windows mixed-DPI failure where the native maximized flag stays
/// set but the HWND keeps a much smaller geometry from the previous monitor.
/// Normal maximized work areas may be a little smaller because of the taskbar;
/// only a large mismatch is considered stale.
fn maximized_geometry_needs_repair(
    window_width: u32,
    window_height: u32,
    monitor_width: u32,
    monitor_height: u32,
) -> bool {
    window_width.saturating_mul(4) < monitor_width.saturating_mul(3)
        || window_height.saturating_mul(4) < monitor_height.saturating_mul(3)
}

/// Ask the renderer to repaint after the window becomes visible again and, on
/// Windows, repair a stale maximized rectangle caused by crossing monitors with
/// different DPI scales (#272). The second redraw runs after the window manager
/// has applied the restore/maximize transition.
fn refresh_revealed_main_window(weak: slint::Weak<AppWindow>) {
    let Some(win) = weak.upgrade() else { return };
    let repair = win
        .window()
        .with_winit_window(|ww| {
            ww.request_redraw();
            if !cfg!(windows) || !ww.is_maximized() {
                return false;
            }
            let Some(monitor) = ww.current_monitor() else {
                return false;
            };
            let outer = ww.outer_size();
            let screen = monitor.size();
            let stale = maximized_geometry_needs_repair(
                outer.width,
                outer.height,
                screen.width,
                screen.height,
            );
            if stale {
                tracing::warn!(
                    "repairing stale maximized geometry: window={}x{} monitor={}x{} scale={}",
                    outer.width,
                    outer.height,
                    screen.width,
                    screen.height,
                    ww.scale_factor(),
                );
                ww.set_maximized(false);
            }
            stale
        })
        .unwrap_or(false);

    let weak2 = weak.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
        if let Some(win) = weak2.upgrade() {
            win.window().with_winit_window(|ww| {
                if repair {
                    ww.set_maximized(true);
                }
                ww.request_redraw();
            });
        }
    });
}

#[cfg(test)]
mod mixed_dpi_window_tests {
    use super::maximized_geometry_needs_repair;

    #[test]
    fn repairs_large_maximized_geometry_mismatch() {
        assert!(maximized_geometry_needs_repair(604, 1384, 1080, 1501));
        assert!(maximized_geometry_needs_repair(1920, 1000, 3840, 2160));
    }

    #[test]
    fn accepts_taskbar_sized_maximized_work_area() {
        assert!(!maximized_geometry_needs_repair(1920, 1040, 1920, 1080));
        assert!(!maximized_geometry_needs_repair(2560, 1400, 2560, 1440));
    }
}

#[cfg(target_os = "linux")]
fn schedule_slint_pointer_ungrab<T>(weak: slint::Weak<T>)
where
    T: slint::ComponentHandle + 'static,
{
    // Linux window managers/compositors may consume the release event after a
    // system move/resize starts. If Slint keeps its press grab, the whole app
    // can remain stuck in move/resize cursor mode. A few deferred synthetic
    // releases cover Cinnamon/Mutter/KWin timing differences.
    for delay_ms in [0_u64, 16, 80, 200] {
        let weak2 = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay_ms), move || {
            if let Some(w) = weak2.upgrade() {
                let win = w.window();
                win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(-1.0, -1.0),
                    button: slint::platform::PointerEventButton::Left,
                });
                win.dispatch_event(slint::platform::WindowEvent::PointerExited);
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn schedule_slint_pointer_ungrab<T>(_weak: slint::Weak<T>)
where
    T: slint::ComponentHandle + 'static,
{
}

/// macOS-only: install a custom winit backend that makes the native title bar
/// transparent and lets the window content render *under* it (fullSizeContentView).
/// The title bar then picks up the app's dark theme / wallpaper (`Theme.window-base`)
/// instead of showing a bright native bar in dark mode (#162 follow-up — immersive
/// title bar). The traffic-light buttons are left in place; the UI insets its top by
/// `titlebar-inset` so tabs don't hide behind them.
///
/// Must run before any window is created. We build the backend explicitly, which
/// would otherwise bypass the `SLINT_BACKEND` renderer override that exists as the
/// macOS femtovg/Skia escape hatch (#108/#129) — so we re-honour it by hand.
#[cfg(target_os = "macos")]
fn setup_macos_platform(renderer_mode: &str) {
    use i_slint_backend_winit::winit::platform::macos::WindowAttributesExtMacOS;

    let mut builder = i_slint_backend_winit::Backend::builder();
    // An explicit environment value wins, including plain "winit" (Slint's
    // automatic choice). Otherwise use the renderer selected in Settings.
    let env_backend = std::env::var("SLINT_BACKEND").ok();
    let renderer = match env_backend.as_deref() {
        Some(backend) => backend
            .strip_prefix("winit-")
            .filter(|renderer| !renderer.is_empty())
            .map(str::to_owned),
        None => Some(renderer_mode.to_owned()),
    };
    if let Some(renderer) = renderer.as_ref() {
        builder = builder.with_renderer_name(renderer.clone());
    }
    tracing::info!(
        renderer_mode,
        renderer = renderer.as_deref().unwrap_or("auto"),
        source = if env_backend.is_some() {
            "SLINT_BACKEND"
        } else {
            "settings"
        },
        "initializing macOS renderer"
    );
    builder = builder.with_window_attributes_hook(|attrs| {
        attrs
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
    });
    match builder.build() {
        Ok(backend) => {
            if slint::platform::set_platform(Box::new(backend)).is_err() {
                tracing::warn!("winit backend already set; immersive macOS titlebar disabled");
            }
        }
        Err(e) => {
            tracing::warn!("winit backend build failed ({e}); immersive macOS titlebar disabled")
        }
    }
}

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
    setup_macos_platform(config.renderer_mode());

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
        // Restoring a saved wallpaper must not override the user's persisted
        // light/dark preference. Built-in wallpapers only suggest their paired
        // theme when the user actively selects them (#theme-persistence).
        apply_wallpaper(&window, &store.borrow(), &bufs, &id, false);
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
            let mut selected_builtin_theme = None;
            if let Some(w) = weak.upgrade() {
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, true);
                if crate::wallpaper::is_builtin(&id) {
                    selected_builtin_theme = Some(w.get_dark_mode());
                }
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            }
            let mut s = store.borrow_mut();
            s.set_wallpaper(id);
            // Choosing a built-in wallpaper applies its recommended palette once;
            // persist that result so it too survives the next launch. A later
            // manual theme toggle will overwrite this preference as expected.
            if let Some(dark) = selected_builtin_theme {
                s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
            }
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
                    apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, false);
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

    // --- Wire callbacks --------------------------------------------------
    wire_session_callbacks(
        &window,
        store.clone(),
        sessions_model.clone(),
        tabs_model.clone(),
        terminals_model.clone(),
        layout.clone(),
        content_size.clone(),
        panes_model.clone(),
        splitters_model.clone(),
        handles.clone(),
        bufs.clone(),
        render_gates.clone(),
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

    wire_tab_callbacks(
        &window,
        tabs_model.clone(),
        terminals_model.clone(),
        layout.clone(),
        content_size.clone(),
        panes_model.clone(),
        splitters_model.clone(),
        handles.clone(),
        bufs.clone(),
        render_gates.clone(),
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
            render_gates: render_gates.clone(),
            tab_statuses: tab_statuses.clone(),
            local_snap: local_snap.clone(),
            local_net_hist: local_net_hist.clone(),
            last_term_size: last_term_size.clone(),
            sftp_follow_cd: sftp_follow_cd.clone(),
            store: store.clone(),
        },
    );

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
        fn SystemParametersInfoW(action: u32, uiparam: u32, pvparam: *mut Rect, winini: u32)
            -> i32;
    }
    const SPI_GETWORKAREA: u32 = 0x0030;

    let size = win.window().size(); // physical pixels
    let mut wa = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
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

fn handle_macos_terminal_wheel(
    win: &AppWindow,
    bufs: &TermBuffers,
    x: f32,
    y: f32,
    lines: i32,
) -> bool {
    let Some(hit) = terminal_wheel_hit(win, bufs, x, y) else {
        return false;
    };
    if hit.is_alt {
        win.invoke_terminal_wheel(hit.tab_id.into(), lines.signum(), hit.col, hit.row);
    } else {
        win.invoke_terminal_scroll(hit.tab_id.into(), lines);
    }
    true
}

fn terminal_wheel_hit(
    win: &AppWindow,
    bufs: &TermBuffers,
    x: f32,
    y: f32,
) -> Option<TerminalWheelHit> {
    let (active, term, term_state) = active_terminal_panel_rects(win)?;
    let mut term_x = term.x;
    let mut term_y = term.y;
    let mut term_w = term.w;
    let mut term_h = term.h;

    // TerminalView starts with a 24px status line, then the SFTP dock-region.
    term_y += 24.0;
    term_h = (term_h - 24.0).max(0.0);

    let sftp_dock = win.get_sftp_dock().to_string();
    let sftp_take = if term_state.sftp_collapsed {
        36.0
    } else if sftp_dock == "left" || sftp_dock == "right" {
        term_state.sftp_panel_width + 4.0
    } else {
        term_state.sftp_panel_height + 4.0
    };
    shrink_edge(&mut term_x, &mut term_y, &mut term_w, &mut term_h, &sftp_dock, sftp_take);

    // Leave the command bar to TextInput/history handling; wheel fallback is for
    // terminal output only.
    term_h = (term_h - 34.0).max(0.0);
    if !contains_logical(
        LogicalRect {
            x: term_x,
            y: term_y,
            w: term_w,
            h: term_h,
        },
        x,
        y,
    ) {
        return None;
    }

    let h = term_buf(bufs, &active)?;
    let guard = h.lock().ok()?;
    let screen = guard.parser.screen();
    let (rows, cols) = screen.size();
    let cell_w = (term_w / cols.max(1) as f32).max(1.0);
    let cell_h = (term_h / rows.max(1) as f32).max(1.0);
    Some(TerminalWheelHit {
        tab_id: active,
        is_alt: screen.alternate_screen(),
        col: ((x - term_x) / cell_w).floor() as i32,
        row: ((y - term_y) / cell_h).floor() as i32,
    })
}

fn shrink_edge(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, dock: &str, amount: f32) {
    let amount = amount.max(0.0);
    match dock {
        "left" => {
            *x += amount;
            *w = (*w - amount).max(0.0);
        }
        "right" => *w = (*w - amount).max(0.0),
        "top" => {
            *y += amount;
            *h = (*h - amount).max(0.0);
        }
        "bottom" => *h = (*h - amount).max(0.0),
        _ => {}
    }
}

fn contains_logical(rect: LogicalRect, x: f32, y: f32) -> bool {
    x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h
}

fn app_content_area(win: &AppWindow) -> LogicalRect {
    let size = win.window().size();
    let scale = win.window().scale_factor().max(0.01) as f32;
    let mut area = LogicalRect {
        x: 0.0,
        y: if win.get_custom_titlebar() {
            38.0
        } else if win.get_is_mac() {
            28.0
        } else {
            0.0
        },
        w: size.width as f32 / scale,
        h: 0.0,
    };
    area.h = size.height as f32 / scale - area.y;

    if win.get_welcome_as_sidebar() {
        let dock = win.get_welcome_sidebar_dock().to_string();
        let sidebar_strip_outside = !win.get_welcome_collapsed()
            && win.get_sidebar_collapsed()
            && win.get_sidebar_dock().as_str() == dock.as_str();
        let welcome_taken = (if win.get_welcome_collapsed() {
            36.0
        } else {
            win.get_welcome_sidebar_width()
        }) + if sidebar_strip_outside { 36.0 } else { 0.0 };
        shrink_edge(
            &mut area.x,
            &mut area.y,
            &mut area.w,
            &mut area.h,
            &dock,
            welcome_taken,
        );
    }

    let side_dock = win.get_sidebar_dock().to_string();
    let side_take = if win.get_sidebar_collapsed() {
        36.0
    } else if side_dock == "left" || side_dock == "right" {
        win.get_sidebar_width() + 4.0
    } else {
        win.get_sidebar_height() + 4.0
    };
    shrink_edge(
        &mut area.x,
        &mut area.y,
        &mut area.w,
        &mut area.h,
        &side_dock,
        side_take,
    );
    if win.get_quick_panel_open() {
        let quick_dock = win.get_quick_panel_dock().to_string();
        let quick_merged = win.get_quick_panel_collapsed()
            && ((win.get_welcome_as_sidebar()
                && win.get_welcome_collapsed()
                && win.get_welcome_sidebar_dock().as_str() == quick_dock.as_str())
                || (win.get_sidebar_collapsed() && side_dock.as_str() == quick_dock.as_str()));
        if quick_merged {
            return area;
        }
        let quick_take = if win.get_quick_panel_collapsed() {
            36.0
        } else if quick_dock == "left" || quick_dock == "right" {
            win.get_quick_panel_width() + 4.0
        } else {
            win.get_quick_panel_height() + 4.0
        };
        shrink_edge(
            &mut area.x,
            &mut area.y,
            &mut area.w,
            &mut area.h,
            &quick_dock,
            quick_take,
        );
    }
    area
}

fn active_terminal_panel_rects(win: &AppWindow) -> Option<(String, LogicalRect, TerminalState)> {
    let active = win.get_active_tab_id().to_string();
    if active.is_empty() || active == "welcome" {
        return None;
    }

    let area = app_content_area(win);
    let panes = win.get_panes();
    let pane = (0..panes.row_count())
        .filter_map(|i| panes.row_data(i))
        .find(|p| p.active_id.as_str() == active.as_str())?;

    let terms = win.get_terminals();
    let term_state = (0..terms.row_count())
        .filter_map(|i| terms.row_data(i))
        .find(|t| t.id.as_str() == active.as_str())?;

    Some((
        active,
        LogicalRect {
            x: area.x + pane.x,
            y: area.y + pane.y + 40.0,
            w: pane.w,
            h: (pane.h - 40.0).max(0.0),
        },
        term_state,
    ))
}

fn active_sftp_file_list_rect(win: &AppWindow) -> Option<LogicalRect> {
    let (_active, term, term_state) = active_terminal_panel_rects(win)?;
    if term_state.sftp_collapsed {
        return None;
    }

    // TerminalView starts with a 24px connection-status line; SFTP docks inside
    // the remaining dock-region. This mirrors ui/terminal_view.slint.
    let dock_region = LogicalRect {
        x: term.x,
        y: term.y + 24.0,
        w: term.w,
        h: (term.h - 24.0).max(0.0),
    };
    let dock = win.get_sftp_dock().to_string();
    let mut panel = LogicalRect {
        x: dock_region.x,
        y: dock_region.y,
        w: if dock == "left" || dock == "right" {
            term_state.sftp_panel_width
        } else {
            dock_region.w
        },
        h: if dock == "left" || dock == "right" {
            dock_region.h
        } else {
            term_state.sftp_panel_height
        },
    };
    if dock == "right" {
        panel.x = dock_region.x + (dock_region.w - panel.w).max(0.0);
    } else if dock == "bottom" {
        panel.y = dock_region.y + (dock_region.h - panel.h).max(0.0);
    }

    // SftpPanel layout: toolbar 34, then file headers 20 + separator 1; when the
    // tree is shown (top/bottom docks), the file list starts after tree 160 + sep.
    let show_tree = dock != "left" && dock != "right";
    panel.y += 34.0 + 20.0 + 1.0;
    panel.h = (panel.h - 34.0 - 20.0 - 1.0).max(0.0);
    if show_tree {
        panel.x += 160.0 + 1.0;
        panel.w = (panel.w - 160.0 - 1.0).max(0.0);
    }
    Some(panel)
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
fn handle_file_drop(win: &AppWindow, sftp_handles: &SftpHandles, path: std::path::PathBuf) {
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let w = win.window();
    let scale = w.scale_factor().max(0.01);
    let Some(inner) = w.with_winit_window(|ww| ww.inner_position().ok()).flatten() else {
        return;
    };
    let Some((cx, cy)) = cursor_pos() else {
        return;
    };
    // Drop point in logical client coordinates.
    let client_x = (cx - inner.x) as f32 / scale;
    let client_y = (cy - inner.y) as f32 / scale;
    let Some(file_list) = active_sftp_file_list_rect(win) else {
        return;
    };
    if !contains_logical(file_list, client_x, client_y) {
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
    let other_dirs = if sync {
        terminal_sftp_paths(win)
    } else {
        HashMap::new()
    };
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(&active) {
            win.set_download_open(true);
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
}

#[cfg(not(windows))]
fn handle_file_drop(_win: &AppWindow, _sftp_handles: &SftpHandles, _path: std::path::PathBuf) {}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

/// Parse the batch-import textarea (#150). Each non-empty, non-`#` line is
/// `host|port|user|password|name`; trailing fields are optional (port → 22,
/// user → root, password → none, name → user@host). A leading header row such as
/// `host|port|username|password|name` is skipped. Dedup happens at the call site.
fn parse_batch_import(text: &str) -> Vec<Session> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // splitn(5) so the last field (name) may itself contain '|'.
        let parts: Vec<&str> = line.splitn(5, '|').map(str::trim).collect();
        let host = parts.first().copied().unwrap_or("");
        // Skip blank hosts and a header row like "host|port|username|...".
        if host.is_empty() || host.eq_ignore_ascii_case("host") {
            continue;
        }
        let port = parts
            .get(1)
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|&p| p > 0)
            .unwrap_or(22);
        let user = parts
            .get(2)
            .copied()
            .filter(|s| !s.is_empty())
            .unwrap_or("root");
        let password = parts.get(3).copied().unwrap_or("");
        let name = parts
            .get(4)
            .copied()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{user}@{host}"));
        let mut sess = Session {
            name,
            host: host.to_string(),
            port,
            user: user.to_string(),
            auth: AuthMethod::Password,
            ..Session::new_empty()
        };
        if !password.is_empty() {
            sess.password = Secret::new(password.to_string());
        }
        out.push(sess);
    }
    out
}

/// Distinct named groups (explicit folders ∪ the groups sessions are filed under),
/// de-duplicated and sorted alphabetically — feeds the new/edit dialog's group
/// dropdown (#179). Ungrouped ("") is excluded; the dialog leaves the field blank
/// for that case.
fn session_groups_model(store: &ConfigStore) -> ModelRc<SharedString> {
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
fn jump_candidates(
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

fn sync_sessions_to_model(store: &ConfigStore, model: &VecModel<SessionInfo>) {
    // Group sessions by their `group` (named groups alphabetically, ungrouped
    // last), then by name within each group, and tag the first row of every
    // group with a header so the welcome list can render a folder heading (#41).
    let sessions = store.sessions();
    let collapsed_groups = store.collapsed_session_groups();
    let group_is_collapsed = |group: &str| {
        collapsed_groups
            .map(|groups| groups.iter().any(|collapsed| collapsed == group))
            .unwrap_or(true)
    };

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
        collapsed: group_is_collapsed(group),
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
            collapsed: group_is_collapsed("system"),
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
                    collapsed: group_is_collapsed(group),
                });
            }
        }
    }
    model.set_vec(rows);
}

fn builtin_local_sessions() -> Vec<Session> {
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

fn builtin_local_session(id: &str, name: impl Into<String>, host: &str) -> Session {
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
fn wsl_available() -> bool {
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

// ---------------------------------------------------------------------------
// Session callbacks (welcome page + dialog)
// ---------------------------------------------------------------------------

/// Build the effective session represented by the dialog. When editing, blank
/// secret fields retain their saved values because real passwords and pasted
/// private keys are deliberately never echoed back into the UI (#10, #276).
fn session_from_draft(
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

fn wire_session_callbacks(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    sessions_model: Rc<VecModel<SessionInfo>>,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    render_gates: RenderGates,
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
        let store = store.clone();
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
            {
                let mut store = store.borrow_mut();
                store.set_session_group_collapsed(&target, new_state);
                if let Err(err) = store.save() {
                    tracing::warn!("failed to save Quick Connect folder state: {err:#}");
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
                    .set_title(t("选择私钥文件", "Choose private key file"));
            // macOS uses UTIs, not extensions — a filter would hide
            // extensionless keys like id_ed25519. Only filter on Windows/Linux.
            #[cfg(not(target_os = "macos"))]
            {
                dialog = dialog.add_filter(
                    t("SSH 私钥", "SSH private keys"),
                    &["ppk", "pem", "key"],
                );
            }
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
            let ctx = ConnectCtx {
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
            start_session_in_tab(&tab_id, session, &ctx);
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

/// Resolve a session's configured SSH jump host to the saved session it points
/// at, ignoring a missing / dangling / self reference (#211).
fn resolve_jump(store: &Rc<RefCell<ConfigStore>>, session: &Session) -> Option<Session> {
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
fn start_session_in_tab(tab_id: &str, session: Session, ctx: &ConnectCtx) {
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
            // This survives drain batches, so a stream of small events cannot
            // evade the frame checkpoint merely because of thread timing.
            let mut ingested_since_checkpoint = 0usize;
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
                                if prev.len() + chunk.len() <= OUTPUT_MERGE_BYTE_CAP {
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

                // Ingest terminal output on this pump thread (not the UI thread).
                // Keep each Output event atomic: TermBuffer detects full-screen
                // redraw sequences within one ingest call, so artificial byte
                // splits could corrupt scrollback when they bisect such a refresh.
                let mut remaining_output_bytes: usize = ui_batch
                    .iter()
                    .map(|event| match event {
                        SessionEvent::Output(chunk) => chunk.len(),
                        _ => 0,
                    })
                    .sum();
                let has_immediate_ui_events =
                    ui_batch.iter().any(event_requires_immediate_ui);
                let mut dirty_since_request = false;
                let mut ui_only: Vec<SessionEvent> = Vec::with_capacity(ui_batch.len());
                for evt in ui_batch {
                    match evt {
                        SessionEvent::Output(chunk) => {
                            let chunk_len = chunk.len();
                            ingest_terminal_output(&bufs_thread, &tab_id_pump, chunk.as_bytes());
                            remaining_output_bytes =
                                remaining_output_bytes.saturating_sub(chunk_len);
                            dirty_since_request = true;

                            if record_ingested_chunk(chunk_len, &mut ingested_since_checkpoint) {
                                let ticket = request_tab_render(
                                    weak_inner.clone(),
                                    &tab_id_pump,
                                    &bufs_thread,
                                    &render_gates_pump,
                                );
                                dirty_since_request = false;

                                // The event channel is intentionally unbounded
                                // today. Waiting while a large backlog exists would
                                // only move bytes from the terminal buffer into that
                                // channel and inflate memory, so catch up first and
                                // pace once the stream's tail is within reach.
                                if !has_immediate_ui_events
                                    && remaining_output_bytes <= PACED_LOCAL_BACKLOG_LIMIT
                                    && shell_rx.len() <= PACED_QUEUE_EVENT_LIMIT
                                {
                                    wait_for_ui_flush(ticket);
                                }
                            }
                        }
                        other => ui_only.push(other),
                    }
                }

                if dirty_since_request {
                    let _ = request_tab_render(
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

fn sorted_sftp_entries_from_model(
    model: &ModelRc<SftpEntry>,
    key: &str,
    dir: i32,
) -> ModelRc<SftpEntry> {
    let Some(vec_model) = model.as_any().downcast_ref::<VecModel<SftpEntry>>() else {
        return model.clone();
    };
    let mut entries = Vec::with_capacity(vec_model.row_count());
    for i in 0..vec_model.row_count() {
        if let Some(entry) = vec_model.row_data(i) {
            entries.push(entry);
        }
    }
    sort_sftp_entries(&mut entries, key, dir);
    ModelRc::from(std::rc::Rc::new(VecModel::from(entries)))
}

fn sort_sftp_entries(entries: &mut [SftpEntry], key: &str, dir: i32) {
    use std::cmp::Ordering;

    let name_cmp = |a: &SftpEntry, b: &SftpEntry| natural_name_cmp(&a.name, &b.name);
    let default_cmp = |a: &SftpEntry, b: &SftpEntry| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => name_cmp(a, b),
    };

    if dir == 0 || key.is_empty() {
        entries.sort_by(default_cmp);
        return;
    }

    entries.sort_by(|a, b| {
        let group = match (a.is_dir, b.is_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        };
        if group != Ordering::Equal {
            return group;
        }
        let ord = match key {
            "size" => a
                .size_bytes
                .partial_cmp(&b.size_bytes)
                .unwrap_or(Ordering::Equal)
                .then_with(|| default_cmp(a, b)),
            "modified" => a
                .modified_ts
                .partial_cmp(&b.modified_ts)
                .unwrap_or(Ordering::Equal)
                .then_with(|| default_cmp(a, b)),
            _ => name_cmp(a, b).then_with(|| default_cmp(a, b)),
        };
        if dir < 0 {
            ord.reverse()
        } else {
            ord
        }
    });
}

fn natural_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_ascii_cmp(&a.to_lowercase(), &b.to_lowercase()).then_with(|| a.cmp(b))
}

fn natural_ascii_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut ai = 0;
    let mut bi = 0;
    while ai < ab.len() && bi < bb.len() {
        let ad = ab[ai].is_ascii_digit();
        let bd = bb[bi].is_ascii_digit();
        if ad && bd {
            let a_start = ai;
            let b_start = bi;
            while ai < ab.len() && ab[ai].is_ascii_digit() {
                ai += 1;
            }
            while bi < bb.len() && bb[bi].is_ascii_digit() {
                bi += 1;
            }

            let mut a_sig = a_start;
            let mut b_sig = b_start;
            while a_sig < ai && ab[a_sig] == b'0' {
                a_sig += 1;
            }
            while b_sig < bi && bb[b_sig] == b'0' {
                b_sig += 1;
            }

            let a_len = ai - a_sig;
            let b_len = bi - b_sig;
            let ord = a_len
                .cmp(&b_len)
                .then_with(|| ab[a_sig..ai].cmp(&bb[b_sig..bi]))
                .then_with(|| (ai - a_start).cmp(&(bi - b_start)));
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }

        let ord = ab[ai].cmp(&bb[bi]);
        if ord != Ordering::Equal {
            return ord;
        }
        ai += 1;
        bi += 1;
    }
    ab.len().cmp(&bb.len())
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
fn disk_rows(disks: &[(String, u64, u64)]) -> Vec<DiskInfo> {
    disks
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
        .collect()
}

fn disk_model(disks: &[(String, u64, u64)]) -> ModelRc<DiskInfo> {
    ModelRc::from(Rc::new(VecModel::from(disk_rows(disks))))
}

/// Build the process-monitor model for the popup (#23). `cpu`/`mem` are
/// pre-formatted to one decimal; `cpu_frac` (0..1) drives the row's load bar.
fn set_process_action_error(weak: &slint::Weak<ProcWindow>, message: &str) {
    if let Some(window) = weak.upgrade() {
        window.set_action_busy(false);
        window.set_action_error(true);
        window.set_action_status(message.into());
    }
}

/// A root login can signal any process directly. Non-root logins may signal
/// only their own processes; root and other users' processes require `su`.
fn process_needs_root(current_user: &str, process_user: &str) -> bool {
    current_user != "root" && process_user != current_user
}

fn proc_rows(procs: &[ProcInfo], current_user: &str, tab_id: &str) -> Vec<ProcRow> {
    procs
        .iter()
        .map(|p| ProcRow {
            tab_id: tab_id.into(),
            pid: p.pid.to_string().into(),
            user: p.user.clone().into(),
            cpu: format!("{:.1}", p.cpu).into(),
            mem: format!("{:.1}", p.mem).into(),
            command: p.command.clone().into(),
            cpu_frac: (p.cpu / 100.0).clamp(0.0, 1.0),
            own_process: !process_needs_root(current_user, &p.user),
        })
        .collect()
}

#[cfg(test)]
mod process_row_tests {
    use super::*;

    #[test]
    fn marks_owner_and_preserves_source_tab() {
        let input = vec![
            ProcInfo { pid: 10, user: "alice".into(), cpu: 1.0, mem: 2.0, command: "own".into() },
            ProcInfo { pid: 11, user: "root".into(), cpu: 3.0, mem: 4.0, command: "other".into() },
        ];
        let rows = proc_rows(&input, "alice", "term-a");
        assert!(rows[0].own_process);
        assert!(!rows[1].own_process);
        assert!(rows.iter().all(|row| row.tab_id.as_str() == "term-a"));
    }

    #[test]
    fn privilege_rules_match_effective_login_user() {
        assert!(!process_needs_root("alice", "alice"));
        assert!(process_needs_root("alice", "root"));
        assert!(process_needs_root("alice", "bob"));
        assert!(!process_needs_root("root", "root"));
        assert!(!process_needs_root("root", "alice"));
    }
}

fn metric_rows(
    cpu: f32,
    mem: f32,
    swap: f32,
    mem_detail: impl Into<SharedString>,
    swap_detail: impl Into<SharedString>,
) -> Vec<SysMetricRow> {
    vec![
        SysMetricRow {
            label: "CPU".into(),
            percent: cpu,
            detail: "".into(),
            kind: 0,
        },
        SysMetricRow {
            label: t("内存", "Memory").into(),
            percent: mem,
            detail: mem_detail.into(),
            kind: 1,
        },
        SysMetricRow {
            label: t("交换", "Swap").into(),
            percent: swap,
            detail: swap_detail.into(),
            kind: 2,
        },
    ]
}

fn net_rows(net: &[(String, u64, u64)]) -> Vec<SysNetRow> {
    net.iter()
        .map(|(name, rx, tx)| SysNetRow {
            name: name.clone().into(),
            up: format_bytes_per_sec(*tx).into(),
            down: format_bytes_per_sec(*rx).into(),
        })
        .collect()
}

fn pairs_to_overview_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    pairs
        .chunks(2)
        .map(|chunk| {
            let first = &chunk[0];
            let second = chunk.get(1);
            SysInfoRow {
                c1: first.0.clone().into(),
                c2: first.1.clone().into(),
                c3: second.map(|p| p.0.clone()).unwrap_or_default().into(),
                c4: second.map(|p| p.1.clone()).unwrap_or_default().into(),
                c5: "".into(),
            }
        })
        .collect()
}

fn pairs_to_one_row(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(1).into(),
        c3: value(2).into(),
        c4: value(3).into(),
        c5: value(4).into(),
    }]
}

fn pairs_to_rows(pairs: &[(String, String)], width: usize) -> Vec<SysInfoRow> {
    pairs
        .chunks(width)
        .filter(|chunk| chunk.iter().any(|(_, v)| !v.trim().is_empty() && v.trim() != "-"))
        .map(|chunk| {
            let value = |idx: usize| {
                chunk
                    .get(idx)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| "-".to_string())
            };
            SysInfoRow {
                c1: value(0).into(),
                c2: value(1).into(),
                c3: value(2).into(),
                c4: value(3).into(),
                c5: value(4).into(),
            }
        })
        .collect()
}

fn cpu_usage_detail_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "0.0%".to_string())
    };
    let extra = pairs
        .iter()
        .skip(4)
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(" / ");
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(2).into(),
        c3: value(1).into(),
        c4: value(3).into(),
        c5: extra.into(),
    }]
}

fn tuple5_rows(rows: &[(String, String, String, String, String)]) -> Vec<SysInfoRow> {
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

fn nonempty_or_dash(value: impl Into<String>) -> String {
    let value = value.into();
    if value.trim().is_empty() {
        "-".to_string()
    } else {
        value
    }
}

fn local_hardware_info() -> &'static LocalHardwareInfo {
    static INFO: OnceLock<LocalHardwareInfo> = OnceLock::new();
    INFO.get_or_init(|| {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all();
        let first_cpu = sys.cpus().first();
        let mut info = LocalHardwareInfo {
            os: sysinfo::System::long_os_version()
                .or_else(sysinfo::System::name)
                .unwrap_or_else(|| std::env::consts::OS.to_string()),
            kernel: sysinfo::System::name().unwrap_or_else(|| std::env::consts::FAMILY.to_string()),
            kernel_version: sysinfo::System::kernel_version().unwrap_or_default(),
            arch: std::env::consts::ARCH.to_string(),
            hostname: sysinfo::System::host_name().unwrap_or_default(),
            cpu_name: first_cpu
                .map(|cpu| cpu.brand().to_string())
                .unwrap_or_default(),
            cpu_vendor: first_cpu
                .map(|cpu| cpu.vendor_id().to_string())
                .unwrap_or_default(),
            cpu_cores: sys.cpus().len().to_string(),
            cpu_frequency: first_cpu
                .map(|cpu| {
                    let mhz = cpu.frequency();
                    if mhz == 0 {
                        String::new()
                    } else if mhz >= 1000 {
                        format!("{:.2} GHz", mhz as f64 / 1000.0)
                    } else {
                        format!("{mhz} MHz")
                    }
                })
                .unwrap_or_default(),
            ..Default::default()
        };
        fill_local_gpu_info(&mut info);
        info
    })
}

#[cfg(target_os = "windows")]
fn fill_local_gpu_info(info: &mut LocalHardwareInfo) {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$controllers = @(Get-CimInstance Win32_VideoController | Select-Object Name,AdapterCompatibility,DriverVersion,AdapterRAM); $regs = @(Get-ChildItem 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}' -ErrorAction SilentlyContinue | ForEach-Object { $p = Get-ItemProperty $_.PsPath -ErrorAction SilentlyContinue; if ($p.DriverDesc) { [pscustomobject]@{ Name=$p.DriverDesc; Vendor=$p.ProviderName; Driver=$p.DriverVersion; Memory=$p.'HardwareInformation.qwMemorySize' } } }); [pscustomobject]@{ Controllers=$controllers; Registry=$regs } | ConvertTo-Json -Compress -Depth 4",
        ])
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return;
    };
    let registry_values = value
        .get("Registry")
        .map(json_values)
        .unwrap_or_default();
    let controller_values = value
        .get("Controllers")
        .map(json_values)
        .unwrap_or_else(|| json_values(&value));
    let registry_gpus: Vec<LocalGpuInfo> = registry_values
        .iter()
        .filter_map(gpu_from_registry_json)
        .collect();
    info.gpus = controller_values
        .iter()
        .filter_map(|gpu| {
            let get_str = |key: &str| {
                gpu.get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let name = get_str("Name");
            if name.is_empty() {
                return None;
            }
            let matched = registry_gpus
                .iter()
                .find(|item| item.name.eq_ignore_ascii_case(&name))
                .or_else(|| {
                    registry_gpus
                        .iter()
                        .find(|item| !item.name.is_empty() && name.contains(&item.name))
                });
            Some(LocalGpuInfo {
                name,
                vendor: nonempty_prefer(
                    matched.map(|item| item.vendor.as_str()).unwrap_or_default(),
                    &get_str("AdapterCompatibility"),
                ),
                driver: nonempty_prefer(
                    matched.map(|item| item.driver.as_str()).unwrap_or_default(),
                    &get_str("DriverVersion"),
                ),
                memory: nonempty_prefer(
                    matched.map(|item| item.memory.as_str()).unwrap_or_default(),
                    &gpu
                        .get("AdapterRAM")
                        .and_then(|v| v.as_u64())
                        .filter(|bytes| *bytes > 0)
                        .map(format_size)
                        .unwrap_or_default(),
                ),
            })
        })
        .collect();
}

#[cfg(target_os = "windows")]
fn json_values(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(items) = value.as_array() {
        items.clone()
    } else if value.is_null() {
        Vec::new()
    } else {
        vec![value.clone()]
    }
}

#[cfg(target_os = "windows")]
fn nonempty_prefer(primary: &str, fallback: &str) -> String {
    if primary.trim().is_empty() {
        fallback.trim().to_string()
    } else {
        primary.trim().to_string()
    }
}

#[cfg(target_os = "windows")]
fn gpu_from_registry_json(gpu: &serde_json::Value) -> Option<LocalGpuInfo> {
    let get_str = |key: &str| {
        gpu.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let name = get_str("Name");
    if name.is_empty() {
        return None;
    }
    Some(LocalGpuInfo {
        name,
        vendor: get_str("Vendor"),
        driver: get_str("Driver"),
        memory: gpu
            .get("Memory")
            .and_then(|v| {
                v.as_u64().or_else(|| {
                    v.as_array().and_then(|bytes| {
                        let mut raw = [0u8; 8];
                        let mut any = false;
                        for (idx, b) in bytes.iter().take(8).enumerate() {
                            if let Some(n) = b.as_u64() {
                                raw[idx] = n as u8;
                                any = true;
                            }
                        }
                        any.then(|| u64::from_le_bytes(raw))
                    })
                })
            })
            .filter(|bytes| *bytes > 0)
            .map(format_size)
            .unwrap_or_default(),
    })
}

#[cfg(not(target_os = "windows"))]
fn fill_local_gpu_info(_info: &mut LocalHardwareInfo) {}

fn local_system_details(snap: &SystemSnapshot) -> SystemDetails {
    let mem_used = snap.mem_used_mib.saturating_mul(1024 * 1024);
    let mem_total = snap.mem_total_mib.saturating_mul(1024 * 1024);
    let swap_used = snap.swap_used_mib.saturating_mul(1024 * 1024);
    let swap_total = snap.swap_total_mib.saturating_mul(1024 * 1024);
    let info = local_hardware_info();
    SystemDetails {
        overview: vec![
            (t("操作系统", "Operating system").to_string(), nonempty_or_dash(&info.os)),
            (
                t("内核版本", "Kernel version").to_string(),
                nonempty_or_dash(&info.kernel_version),
            ),
            (t("主机名称", "Hostname").to_string(), nonempty_or_dash(&info.hostname)),
            (t("内核", "Kernel").to_string(), nonempty_or_dash(&info.kernel)),
            (t("硬件架构", "Architecture").to_string(), nonempty_or_dash(&info.arch)),
            (t("连接", "Connection").to_string(), t("本机", "Local").to_string()),
        ],
        cpu_info: vec![
            (t("名称", "Name").to_string(), nonempty_or_dash(&info.cpu_name)),
            (t("核心数", "Cores").to_string(), nonempty_or_dash(&info.cpu_cores)),
            (t("频率", "Frequency").to_string(), nonempty_or_dash(&info.cpu_frequency)),
            (t("缓存", "Cache").to_string(), "-".to_string()),
            ("BogoMips".to_string(), nonempty_or_dash(&info.cpu_vendor)),
        ],
        gpu_info: info
            .gpus
            .iter()
            .flat_map(|gpu| {
                [
                    (t("名称", "Name").to_string(), nonempty_or_dash(&gpu.name)),
                    (t("厂商", "Vendor").to_string(), nonempty_or_dash(&gpu.vendor)),
                    (t("驱动", "Driver").to_string(), nonempty_or_dash(&gpu.driver)),
                    (t("内存", "Memory").to_string(), nonempty_or_dash(&gpu.memory)),
                ]
            })
            .collect(),
        cpu_usage: vec![
            (t("用户", "User").to_string(), format!("{:.1}%", snap.cpu_percent * 100.0)),
            ("Nice".to_string(), "-".to_string()),
            (t("系统", "System").to_string(), "-".to_string()),
            (t("空闲", "Idle").to_string(), "-".to_string()),
        ],
        memory: vec![
            (t("总计", "Total").to_string(), format_size(mem_total)),
            (t("已使用", "Used").to_string(), format_size(mem_used)),
            (
                t("剩余", "Free").to_string(),
                format_size(mem_total.saturating_sub(mem_used)),
            ),
            (t("已用", "Usage").to_string(), format!("{:.1}%", snap.mem_percent * 100.0)),
            (t("缓存", "Cached").to_string(), "-".to_string()),
        ],
        swap: vec![
            (t("总计", "Total").to_string(), format_size(swap_total)),
            (t("已使用", "Used").to_string(), format_size(swap_used)),
            (
                t("剩余", "Free").to_string(),
                format_size(swap_total.saturating_sub(swap_used)),
            ),
            (t("已用", "Usage").to_string(), format!("{:.1}%", snap.swap_percent * 100.0)),
        ],
        networks: vec![(
            t("本机", "Local").to_string(),
            "-".to_string(),
            "-".to_string(),
            format_bytes_per_sec(snap.net_tx_per_sec),
            format_bytes_per_sec(snap.net_rx_per_sec),
        )],
        filesystems: snap
            .disks
            .iter()
            .map(|(mount, avail, total)| {
                let used = total.saturating_sub(*avail);
                let pct = if *total == 0 {
                    "-".to_string()
                } else {
                    format!("{:.1}%", used as f64 * 100.0 / *total as f64)
                };
                (
                    mount.clone(),
                    format_size(*total),
                    pct,
                    format_size(*avail),
                    mount.clone(),
                )
            })
            .collect(),
    }
}

/// Mirror the main window's theme/scale/UI-font onto the detached process
/// window. Theme is a per-window Slint global, so a detached window keeps its
/// compile-time (dark) defaults until we copy these across (#23).
fn sync_proc_theme(main: &AppWindow, proc: &ProcWindow) {
    proc.set_dark_mode(main.get_dark_mode());
    proc.set_ui_scale(main.get_ui_scale());
    proc.set_ui_font_family(main.get_ui_font_family());
    // Mirror the immersive wallpaper so the detached window shares the frosted
    // backdrop instead of a flat panel.
    proc.set_wallpaper_img(main.get_wallpaper_img());
    proc.set_wallpaper_active(main.get_wallpaper_active());
    proc.set_wp_accent(main.get_wp_accent());
    proc.set_wp_tint(main.get_wp_tint());
}

fn sync_system_info_theme(main: &AppWindow, sys: &SystemInfoWindow) {
    sys.set_dark_mode(main.get_dark_mode());
    sys.set_ui_scale(main.get_ui_scale());
    sys.set_ui_font_family(main.get_ui_font_family());
    sys.set_wallpaper_img(main.get_wallpaper_img());
    sys.set_wallpaper_active(main.get_wallpaper_active());
    sys.set_wp_accent(main.get_wp_accent());
    sys.set_wp_tint(main.get_wp_tint());
}

fn place_system_info_window(main: &AppWindow, sys: &SystemInfoWindow) {
    use i_slint_backend_winit::winit::dpi::{LogicalPosition, LogicalSize};

    let Some((mon_x, mon_y, mon_w, mon_h, scale)) = main
        .window()
        .with_winit_window(|ww| {
            let scale = ww.scale_factor().max(0.01);
            let monitor = ww.current_monitor().or_else(|| ww.primary_monitor())?;
            let pos = monitor.position();
            let size = monitor.size();
            Some((
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
                scale,
            ))
        })
        .flatten()
    else {
        return;
    };

    let target_w = (mon_w * 0.5).clamp(760.0, (mon_w - 24.0).max(760.0));
    let target_h = (mon_h * 0.5).clamp(520.0, (mon_h - 24.0).max(520.0));
    let x = mon_x + (mon_w - target_w).max(0.0) / 2.0;
    let y = mon_y + (mon_h - target_h).max(0.0) / 2.0;

    sys.window().with_winit_window(|ww| {
        let _ = ww.request_inner_size(LogicalSize::new(target_w, target_h));
        ww.set_outer_position(LogicalPosition::new(x, y));
        let _ = scale; // documents that all values above are already logical.
    });
}

/// Center the process monitor on the same physical monitor as the main window.
/// Physical coordinates avoid logical/physical rounding errors when the two
/// displays use different DPI scale factors. Keep the user's current process
/// window size; opening it should reposition, not reset a manual resize.
fn place_process_window(main: &AppWindow, process: &ProcWindow) {
    use i_slint_backend_winit::winit::dpi::PhysicalPosition;

    let monitor = main
        .window()
        .with_winit_window(|ww| ww.current_monitor().or_else(|| ww.primary_monitor()))
        .flatten();
    let Some(monitor) = monitor else { return };
    let origin = monitor.position();
    let monitor_size = monitor.size();

    process.window().with_winit_window(|ww| {
        let window_size = ww.outer_size();
        let x = origin.x + monitor_size.width.saturating_sub(window_size.width) as i32 / 2;
        let y = origin.y + monitor_size.height.saturating_sub(window_size.height) as i32 / 2;
        ww.set_outer_position(PhysicalPosition::new(x, y));
    });
}

/// Persist the current panel docking layout (both panels' edge + size) and the
/// window size, so the next launch restores the user's arrangement. Called on
/// every exit path (#dock).
fn save_layout(win: &AppWindow, store: &Rc<RefCell<ConfigStore>>) {
    let scale = win.window().scale_factor().max(0.01);
    let size = win.window().size();
    let w = size.width as f32 / scale;
    let h = size.height as f32 / scale;
    let mut s = store.borrow_mut();
    s.set_sidebar_width(win.get_sidebar_width());
    s.set_sidebar_height(win.get_sidebar_height());
    s.set_sidebar_dock(win.get_sidebar_dock().to_string());
    s.set_sidebar_collapsed(win.get_sidebar_collapsed());
    s.set_sftp_panel_width(win.get_sftp_panel_width());
    s.set_sftp_panel_height(win.get_sftp_panel_height());
    s.set_sftp_dock(win.get_sftp_dock().to_string());
    s.set_quick_panel_open(win.get_quick_panel_open());
    s.set_quick_panel_collapsed(win.get_quick_panel_collapsed());
    s.set_quick_panel_width(win.get_quick_panel_width());
    s.set_quick_panel_height(win.get_quick_panel_height());
    s.set_quick_panel_dock(win.get_quick_panel_dock().to_string());
    s.set_welcome_sidebar_width(win.get_welcome_sidebar_width());
    s.set_welcome_sidebar_dock(win.get_welcome_sidebar_dock().to_string());
    s.set_welcome_collapsed(win.get_welcome_collapsed());
    // A maximized size isn't a useful "preferred" size to restore to, so only
    // remember the windowed size. Ask the native window too, because the Slint
    // property can lag during startup/shutdown on frameless Windows (#234).
    let native_maximized = win
        .window()
        .with_winit_window(|ww| ww.is_maximized())
        .unwrap_or_else(|| win.get_window_maximized());
    let (saved_w, saved_h) = s.window_size();
    if !native_maximized
        && (saved_w <= 0.0 || saved_h <= 0.0)
        && w > 200.0
        && h > 200.0
    {
        // Normal resize events keep this cache current. Only fall back to the
        // close-time geometry for a first run where no valid resize was seen;
        // do not issue a new native resize while the window is shutting down.
        s.set_window_size(w, h);
    }
    let _ = s.save();
}

/// Every quick-command group name (used to start with all groups collapsed, #55):
/// "default" when any ungrouped command exists, plus explicit quick-groups and any
/// group referenced by a command.
fn all_quick_group_names(store: &ConfigStore) -> std::collections::HashSet<String> {
    let cmds = store.quick_commands();
    let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if cmds.iter().any(|c| c.group.trim().is_empty()) {
        set.insert("default".to_string());
    }
    for g in store.quick_groups() {
        set.insert(g.clone());
    }
    for c in cmds {
        let g = c.group.trim();
        if !g.is_empty() {
            set.insert(g.to_string());
        }
    }
    set
}

/// Build the quick-command model for the command bar + manage dialog (#55).
///
/// Grouped like the welcome session list: the implicit "default" group (entries
/// with an empty group) comes first, then named groups alphabetically. Within a
/// group, entries keep their saved order. `group_header` is set on the first row
/// of each group; `collapsed` reflects `collapsed_groups` (runtime-only state);
/// `orig_index` points back into the stored vec so deletes target the right entry
/// even though the display order differs.
fn quick_cmd_model(
    store: &ConfigStore,
    collapsed_groups: &std::collections::HashSet<String>,
) -> ModelRc<QuickCmd> {
    let cmds = store.quick_commands();

    let has_default = cmds.iter().any(|c| c.group.trim().is_empty());
    // Named groups = explicit quick-groups ∪ groups referenced by commands.
    let mut named: Vec<String> = store
        .quick_groups()
        .iter()
        .cloned()
        .chain(
            cmds.iter()
                .map(|c| c.group.trim().to_string())
                .filter(|g| !g.is_empty()),
        )
        .collect();
    named.sort_by_key(|g| g.to_lowercase());
    named.dedup();

    let mut groups: Vec<String> = Vec::new();
    if has_default {
        groups.push("default".to_string());
    }
    groups.extend(named);

    let mut rows: Vec<QuickCmd> = Vec::new();
    for group in &groups {
        let is_collapsed = collapsed_groups.contains(group);
        let members: Vec<(usize, &crate::config::QuickCommand)> = cmds
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let g = c.group.trim();
                if group == "default" {
                    g.is_empty()
                } else {
                    g == group
                }
            })
            .collect();
        if members.is_empty() {
            // Header-only placeholder for an empty group (orig_index -1) so it can
            // still be renamed / deleted, matching empty session folders.
            rows.push(QuickCmd {
                name: "".into(),
                command: "".into(),
                group: group.clone().into(),
                group_header: group.clone().into(),
                collapsed: is_collapsed,
                orig_index: -1,
                send_enter: true,
            });
        } else {
            for (i, (orig_idx, c)) in members.iter().enumerate() {
                rows.push(QuickCmd {
                    name: c.name.clone().into(),
                    command: c.command.clone().into(),
                    group: group.clone().into(),
                    group_header: if i == 0 {
                        group.clone().into()
                    } else {
                        "".into()
                    },
                    collapsed: is_collapsed,
                    orig_index: *orig_idx as i32,
                    send_enter: c.send_enter,
                });
            }
        }
    }
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn blank_forward_draft() -> PortFwd {
    PortFwd {
        kind: "local".into(),
        name: "".into(),
        bind_addr: "127.0.0.1".into(),
        bind_port: "".into(),
        host: "".into(),
        host_port: "".into(),
    }
}

fn forward_drafts(forwards: &[crate::config::PortForward]) -> Vec<PortFwd> {
    forwards
        .iter()
        .map(|forward| PortFwd {
            kind: forward.kind.clone().into(),
            name: forward.name.clone().into(),
            bind_addr: if forward.bind_addr.trim().is_empty() {
                "127.0.0.1".into()
            } else {
                forward.bind_addr.trim().into()
            },
            bind_port: forward.bind_port.to_string().into(),
            host: forward.host.clone().into(),
            host_port: if forward.kind == "dynamic" {
                "".into()
            } else {
                forward.host_port.to_string().into()
            },
        })
        .collect()
}

fn forward_model(forwards: &[PortFwd]) -> ModelRc<PortFwd> {
    ModelRc::from(Rc::new(VecModel::from(forwards.to_vec())))
}

fn validated_port_forwards(
    drafts: &[PortFwd],
) -> std::result::Result<Vec<crate::config::PortForward>, String> {
    let mut forwards = Vec::new();
    for draft in drafts {
        let is_blank = draft.name.trim().is_empty()
            && draft.bind_port.trim().is_empty()
            && draft.host.trim().is_empty()
            && draft.host_port.trim().is_empty();
        if is_blank {
            continue;
        }

        let bind_port = draft
            .bind_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                t(
                    "请输入有效的监听端口（1-65535）",
                    "Enter a valid listen port (1-65535).",
                )
                .to_string()
            })?;
        let kind = draft.kind.as_str();
        let (host, host_port) = if kind == "dynamic" {
            (String::new(), 0)
        } else {
            let host = draft.host.trim();
            let host_port = draft
                .host_port
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0);
            if host.is_empty() || host_port.is_none() {
                return Err(t(
                    "请输入目标主机和有效的目标端口（1-65535）",
                    "Enter a target host and a valid target port (1-65535).",
                )
                .to_string());
            }
            (host.to_string(), host_port.unwrap())
        };

        forwards.push(crate::config::PortForward {
            kind: kind.to_string(),
            name: draft.name.trim().to_string(),
            bind_addr: if draft.bind_addr.trim().is_empty() {
                "127.0.0.1".to_string()
            } else {
                draft.bind_addr.trim().to_string()
            },
            bind_port,
            host,
            host_port,
        });
    }
    Ok(forwards)
}

#[cfg(test)]
mod port_forward_draft_tests {
    use super::{blank_forward_draft, validated_port_forwards};

    #[test]
    fn blank_rows_are_ignored_when_saving() {
        assert!(validated_port_forwards(&[blank_forward_draft()])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn filled_rows_are_saved_without_an_add_step() {
        let mut local = blank_forward_draft();
        local.bind_port = "8080".into();
        local.host = "service.internal".into();
        local.host_port = "80".into();

        let mut dynamic = blank_forward_draft();
        dynamic.kind = "dynamic".into();
        dynamic.bind_port = "1080".into();

        let forwards = validated_port_forwards(&[local, dynamic]).unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(forwards[0].bind_port, 8080);
        assert_eq!(forwards[0].host, "service.internal");
        assert_eq!(forwards[1].kind, "dynamic");
        assert_eq!(forwards[1].host_port, 0);
    }

    #[test]
    fn partially_filled_rows_block_saving() {
        let mut draft = blank_forward_draft();
        draft.bind_port = "8080".into();
        assert!(validated_port_forwards(&[draft]).is_err());
    }
}

/// Collect the full paths of the checked SFTP entries for a tab (#100).
fn collect_sftp_selected(terminals: &VecModel<TerminalState>, tab_id: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for ti in 0..terminals.row_count() {
        let Some(row) = terminals.row_data(ti) else {
            continue;
        };
        if row.id.as_str() != tab_id {
            continue;
        }
        if let Some(em) = row
            .sftp_entries
            .as_any()
            .downcast_ref::<VecModel<SftpEntry>>()
        {
            for ei in 0..em.row_count() {
                if let Some(e) = em.row_data(ei) {
                    if e.selected {
                        paths.push(e.full_path.to_string());
                    }
                }
            }
        }
        break;
    }
    paths
}

/// Uncheck every SFTP entry for a tab and reset its selected-count (#100).
fn clear_sftp_selection(terminals: &VecModel<TerminalState>, tab_id: &str) {
    for ti in 0..terminals.row_count() {
        let Some(row) = terminals.row_data(ti) else {
            continue;
        };
        if row.id.as_str() != tab_id {
            continue;
        }
        if let Some(em) = row
            .sftp_entries
            .as_any()
            .downcast_ref::<VecModel<SftpEntry>>()
        {
            for ei in 0..em.row_count() {
                if let Some(mut e) = em.row_data(ei) {
                    if e.selected {
                        e.selected = false;
                        em.set_row_data(ei, e);
                    }
                }
            }
        }
        let mut r = row.clone();
        r.sftp_selected_count = 0;
        terminals.set_row_data(ti, r);
        break;
    }
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

fn output_highlight_rule_model(store: &ConfigStore) -> ModelRc<OutputRuleItem> {
    let rows: Vec<OutputRuleItem> = store
        .output_highlight_rules()
        .iter()
        .map(|rule| OutputRuleItem {
            pattern: rule.pattern.clone().into(),
            regex: rule.regex,
            case_sensitive: rule.case_sensitive,
            whole_line: rule.whole_line,
            color: match rule.color.as_str() {
                "yellow" | "green" | "cyan" | "magenta" | "gray" => rule.color.clone(),
                _ => "red".to_string(),
            }
            .into(),
            enabled: rule.enabled,
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn parse_hex_color(value: &str) -> Option<slint::Color> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(slint::Color::from_rgb_u8(red, green, blue))
}

fn validate_output_highlight_rule(
    pattern: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> std::result::Result<(), String> {
    if pattern.is_empty() {
        return Err(t("请输入关键词或正则表达式", "Enter a keyword or regular expression").into());
    }
    if pattern.chars().count() > 512 {
        return Err(t("规则不能超过 512 个字符", "Rules cannot exceed 512 characters").into());
    }
    if is_regex {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| format!("{}: {error}", t("无效的正则表达式", "Invalid regular expression")))?;
    }
    Ok(())
}

/// Build the filtered history-view model for the dropdown: case-insensitive
/// substring matches of `query`, in the same order as the full history (#101).
fn history_view_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let q = query.trim().to_lowercase();
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .filter(|c| q.is_empty() || c.to_lowercase().contains(&q))
        .map(|s| s.clone().into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Find every (case-insensitive) occurrence of `query` across the currently
/// displayed rows and return highlight rectangles in GRID-COLUMN space (wide
/// CJK glyphs count as two columns, so highlights line up over the text #132).
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

/// Apply a settled terminal size to the PTY + vt100 grid. Factored out of the
/// resize callback so that callback can debounce — a layout reflow can briefly
/// report a near-zero width, collapsing term-cols to its 10-col floor; applying
/// that to the remote PTY reflows vt100 and garbles running output like a
/// `git clone` progress meter (#163). Debouncing means only the settled size
/// ever reaches the server.
fn apply_terminal_resize(
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: &TermBuffers,
    last_term_size: &Arc<Mutex<(u32, u32)>>,
    tab_id: &str,
    cols: u32,
    rows: u32,
) {
    *last_term_size.lock().unwrap() = (cols, rows);
    if let Some(handle) = handles.borrow().get(tab_id) {
        handle.resize(cols, rows);
    }
    if let Some(h) = term_buf(bufs, tab_id) {
        let mut buf = h.lock().unwrap();
        let (old_rows, old_cols) = buf.parser.screen().size();
        let (new_rows, new_cols) = (rows as u16, cols as u16);
        if (new_rows, new_cols) != (old_rows, old_cols) {
            if buf.parser.screen().alternate_screen() {
                // Alt-screen (tmux/vim/btop): the remote redraws the whole screen
                // on SIGWINCH, so just resize the grid and let that redraw fill it.
                buf.parser.set_size(new_rows, new_cols);
            } else {
                // Reflow already-printed output to the new width by replaying the
                // byte stream — vt100's set_size only truncates/pads (#169).
                buf.reflow(new_rows, new_cols);
            }
            // The pre/post-resize screens differ; drop the scroll-detection
            // snapshot so the next output isn't mis-read as a scroll.
            buf.prev.clear();
        }
    }
}

/// Recompute spans + cursor + find/selection highlights for one tab from its
/// current vt100 screen (respecting scrollback) and push them to the model.
/// Used by scroll + selection callbacks (Output has its own equivalent inline).
fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let data = with_term_buf(bufs, tab_id, |buf| {
        let cols = buf.parser.screen().size().1;
        let b = buf.render(); // also refreshes buf.displayed_text
        let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
        let sel = buf.selection_rects_visible(cols);
        (b, matches, sel)
    });
    let Some((b, matches, sel)) = data else {
        return;
    };
    let spans = ModelRc::from(Rc::new(VecModel::from(b.spans)));
    let fm = ModelRc::from(Rc::new(VecModel::from(matches)));
    let sm = ModelRc::from(Rc::new(VecModel::from(sel)));
    let (cr, cc, ru, alt) = (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
    let (smax, soff) = (b.scroll_max, b.scroll_offset);
    set_terminal_row(win, tab_id, move |row| {
        row.spans = spans.clone();
        row.cursor_row = cr;
        row.cursor_col = cc;
        row.rows_used = ru;
        row.is_alt_screen = alt;
        row.find_matches = fm.clone();
        row.selection = sm.clone();
        row.scroll_max = smax;
        row.scroll_offset = soff;
    });
    win.window().request_redraw();
}

/// Refresh only the lightweight selection overlay. Dragging used to call
/// `rebuild_tab_display` for every mouse-move event, reparsing and rebuilding
/// all terminal spans even though the underlying screen had not changed.
fn refresh_terminal_selection(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let selection = with_term_buf(bufs, tab_id, |buf| {
        let cols = buf.parser.screen().size().1;
        buf.selection_rects_visible(cols)
    });
    let Some(selection) = selection else {
        return;
    };
    let model = ModelRc::from(Rc::new(VecModel::from(selection)));
    set_terminal_row(win, tab_id, move |row| {
        row.selection = model.clone();
    });
    win.window().request_redraw();
}

/// Resolve the user's saved theme preference to a dark/light bool (mirrors the
/// startup logic): "light"/"dark" win; otherwise ask the OS, defaulting to dark.
fn theme_pref_is_dark(store: &ConfigStore) -> bool {
    match store.theme_pref() {
        "light" => false,
        "dark" => true,
        _ => match dark_light::detect() {
            dark_light::Mode::Light => false,
            dark_light::Mode::Dark => true,
            dark_light::Mode::Default => true, // undetectable → dark
        },
    }
}

/// Flip the whole app between light and dark. Setting `Theme.dark` alone only
/// recolours the Slint chrome — each terminal bakes its ANSI/default colours
/// from a per-buffer `is_dark` flag at render time, so we must also update every
/// buffer and re-render it. Both the theme toggle and wallpaper switching route
/// through here (the proc-window mirror stays with the toggle).
fn apply_dark_mode(window: &AppWindow, bufs: &TermBuffers, dark: bool) {
    window.set_dark_mode(dark);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for h in handles {
            h.lock().unwrap().is_dark = dark;
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tid in tab_ids {
        rebuild_tab_display(window, bufs, &tid);
    }
}

fn apply_output_highlight(
    window: &AppWindow,
    bufs: &TermBuffers,
    enabled: bool,
    preset: &str,
) {
    let mode = OutputHighlightPreset::from_settings(enabled, preset);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for handle in handles {
            handle.lock().unwrap().output_highlight = mode;
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

fn apply_custom_output_rules(
    window: &AppWindow,
    bufs: &TermBuffers,
    rules: &[OutputHighlightRule],
) {
    let compiled = compile_output_rules(rules);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for handle in handles {
            handle.lock().unwrap().custom_highlight_rules = compiled.clone();
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

/// Apply a wallpaper id to the window: load the image + derived palette, push the
/// immersive Theme overrides (accent / tint / image) and set `dark` from the
/// image luminance. An empty or undecodable id turns immersive mode off and
/// restores the user's saved light/dark theme.
fn apply_wallpaper(
    window: &AppWindow,
    store: &ConfigStore,
    bufs: &TermBuffers,
    id: &str,
    apply_builtin_theme: bool,
) {
    match crate::wallpaper::load(id) {
        Some(wp) => {
            let (ar, ag, ab) = wp.palette.accent;
            let (tr, tg, tb) = wp.palette.tint;
            window.set_wallpaper_img(wp.image);
            window.set_wp_accent(slint::Color::from_rgb_u8(ar, ag, ab));
            window.set_wp_tint(slint::Color::from_rgb_u8(tr, tg, tb));
            // Only the built-ins (designed as a light/dark pair) auto-set the
            // theme. A custom photo keeps the user's light/dark choice so the
            // theme toggle still governs text contrast — a light/white wallpaper
            // reads best in light mode (crisp dark text) rather than being forced
            // dark and greying the text out (#wallpaper).
            if apply_builtin_theme && crate::wallpaper::is_builtin(id) {
                apply_dark_mode(window, bufs, wp.palette.is_dark);
            }
            window.set_wallpaper_active(true);
            window.set_current_wallpaper(id.into());
            let name = if crate::wallpaper::is_builtin(id) {
                String::new()
            } else {
                std::path::Path::new(id)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            window.set_custom_wallpaper_name(name.into());
        }
        None => {
            window.set_wallpaper_active(false);
            window.set_current_wallpaper("".into());
            window.set_custom_wallpaper_name("".into());
            apply_dark_mode(window, bufs, theme_pref_is_dark(store));
        }
    }
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
/// The copyable IP/host from a `user@host` connection label (#192): the part
/// after the last `@`, trimmed. Falls back to the whole string when there's no
/// `@` (already a bare host/IP).
fn conn_ip(host: &str) -> String {
    host.rsplit('@').next().unwrap_or(host).trim().to_string()
}

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

    // Process monitor (#23) lives in a shared model (the AppWindow and the
    // detachable ProcWindow point at the same VecModel), so mutate it in place
    // instead of replacing it — replacing would break the sharing. Only a live
    // remote session has process data; default to empty and let the connected
    // branch below fill it in.
    let set_procs = |win: &AppWindow, procs: &[ProcInfo], current_user: &str, tab_id: &str| {
        if let Some(vm) = win
            .get_proc_list()
            .as_any()
            .downcast_ref::<VecModel<ProcRow>>()
        {
            vm.set_vec(proc_rows(procs, current_user, tab_id));
        }
    };
    let set_system_models =
        |win: &AppWindow,
         cpu: f32,
         mem: f32,
         swap: f32,
         mem_detail: SharedString,
         swap_detail: SharedString,
         nets: Vec<SysNetRow>,
         disks: Vec<DiskInfo>,
         sys: SystemDetails| {
            if let Some(vm) = win
                .get_sys_metrics()
                .as_any()
                .downcast_ref::<VecModel<SysMetricRow>>()
            {
                vm.set_vec(metric_rows(cpu, mem, swap, mem_detail, swap_detail));
            }
            if let Some(vm) = win
                .get_sys_net_rows()
                .as_any()
                .downcast_ref::<VecModel<SysNetRow>>()
            {
                vm.set_vec(nets);
            }
            if let Some(vm) = win
                .get_sys_disks()
                .as_any()
                .downcast_ref::<VecModel<DiskInfo>>()
            {
                vm.set_vec(disks);
            }
            if let Some(vm) = win
                .get_sys_overview_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_overview_rows(&sys.overview));
            }
            if let Some(vm) = win
                .get_sys_cpu_info_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.cpu_info));
            }
            if let Some(vm) = win
                .get_sys_gpu_info_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_rows(&sys.gpu_info, 4));
            }
            if let Some(vm) = win
                .get_sys_cpu_usage_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(cpu_usage_detail_rows(&sys.cpu_usage));
            }
            if let Some(vm) = win
                .get_sys_memory_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.memory));
            }
            if let Some(vm) = win
                .get_sys_swap_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.swap));
            }
            if let Some(vm) = win
                .get_sys_network_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(tuple5_rows(&sys.networks));
            }
            if let Some(vm) = win
                .get_sys_filesystem_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(tuple5_rows(&sys.filesystems));
            }
        };
    win.set_proc_available(false);
    win.set_system_info_available(false);
    set_procs(win, &[], "", "");

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
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            win.set_cpu_percent(st.cpu);
            win.set_mem_percent(pct(st.mem_used_kib, st.mem_total_kib));
            win.set_swap_percent(pct(st.swap_used_kib, st.swap_total_kib));
            win.set_mem_detail(format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into());
            win.set_swap_detail(
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            win.set_net_top_history(normalized_model(&st.net_hist));
            win.set_net_show_selector(!st.net.is_empty());
            win.set_net_selected(name.into());
            let ifaces: Vec<SharedString> = st.net.iter().map(|e| e.0.clone().into()).collect();
            win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::from(ifaces))));
            win.set_disks(disk_model(&st.disks));
            win.set_proc_available(true);
            win.set_system_info_available(true);
            set_procs(win, &st.procs, &st.user, &active);
            set_system_models(
                win,
                st.cpu,
                pct(st.mem_used_kib, st.mem_total_kib),
                pct(st.swap_used_kib, st.swap_total_kib),
                format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
                net_rows(&st.net),
                disk_rows(&st.disks),
                st.sys.clone(),
            );
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            win.set_conn_state(2);
            win.set_connection_state(format!("{} {}", st.host, t("已断开", "disconnected")).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Welcome tab (or unknown) → local machine top + bottom.
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            win.set_conn_host("".into());
            show_local_res(win);
            set_top_local(win);
            set_system_models(
                win,
                snap.cpu_percent,
                snap.mem_percent,
                snap.swap_percent,
                format_mem(snap.mem_used_mib, snap.mem_total_mib).into(),
                format_mem(snap.swap_used_mib, snap.swap_total_mib).into(),
                vec![SysNetRow {
                    name: t("本机", "Local").into(),
                    up: format_bytes_per_sec(snap.net_tx_per_sec).into(),
                    down: format_bytes_per_sec(snap.net_rx_per_sec).into(),
                }],
                Vec::new(),
                SystemDetails::default(),
            );
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
            request_tab_render_from_ui(win.as_weak(), tab_id, bufs, gates);
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
    static HISTORY_STORE: RefCell<Option<Rc<RefCell<ConfigStore>>>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Host-key confirmation (#109-5)
// ---------------------------------------------------------------------------

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
            // Only remember an *accept* for this run (so a slightly-later SFTP
            // prompt for the same host is answered without a second dialog). We
            // must NOT cache a reject: a single dismissal — e.g. an accidental
            // backdrop click instead of "Trust & connect" — used to poison the
            // host for the whole session, auto-rejecting every later connect with
            // "Unknown server key" until the app was restarted (#152). A reject now
            // only fails the current attempt; the next connect prompts again.
            if accept {
                HOSTKEY_DECIDED.with(|d| {
                    d.borrow_mut()
                        .insert(format!("{}:{}", p.host, p.port), true);
                });
            }
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
// MFA / keyboard-interactive prompt (#86-MFA)
// ---------------------------------------------------------------------------

thread_local! {
    static MFA_QUEUE: RefCell<VecDeque<PendingMfa>> = RefCell::new(VecDeque::new());
}

/// Queue an MFA prompt: a concurrent connection for the same session (the shell
/// and its SFTP channel both hitting the prompt at once) merges into the open
/// dialog so the code is only typed once; otherwise enqueue (and show it now if
/// nothing else is up). We deliberately do NOT cache answers across attempts —
/// a wrong code must re-prompt on reconnect, not be silently replayed.
fn enqueue_mfa_prompt(
    win: &AppWindow,
    session_id: String,
    host: String,
    prompt: String,
    echo: bool,
    responder: crate::ssh::MfaResponder,
) {
    let show_now = MFA_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.session_id == session_id) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        q.push_back(PendingMfa {
            session_id,
            host,
            prompt,
            echo,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_mfa(win);
    }
}

/// Populate the MFA dialog from the front prompt and open it.
fn show_front_mfa(win: &AppWindow) {
    MFA_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_mfa_host(p.host.clone().into());
            win.set_mfa_prompt(p.prompt.clone().into());
            win.set_mfa_echo(p.echo);
            win.set_mfa_answer("".into());
            win.set_mfa_prompt_open(true);
        }
    });
}

/// Apply the user's answer to the front MFA prompt (or cancel), then show the
/// next prompt or close.
fn resolve_front_mfa(win: &AppWindow, accept: bool) {
    let answer: Option<String> = if accept {
        Some(win.get_mfa_answer().to_string())
    } else {
        None
    };
    let has_next = MFA_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            for r in &p.responders {
                r.respond(answer.clone());
            }
        }
        !q.is_empty()
    });
    // Don't leave the typed code lingering in the UI property.
    win.set_mfa_answer("".into());
    if has_next {
        show_front_mfa(win);
    } else {
        win.set_mfa_prompt_open(false);
    }
}

// ---------------------------------------------------------------------------
// Split panes (v0.5)
// ---------------------------------------------------------------------------

/// Re-flatten the split-tree `layout` for the current content-area size and push
/// the result into the AppWindow's `panes` / `splitters` models. Also keeps the
/// single global `active-tab-id` pointing at the focused pane's active tab — the
/// sidebar and key routing still read that one id.
/// True when two tab sub-models hold the same ids in the same order.
fn tabs_eq(a: &ModelRc<TabInfo>, b: &ModelRc<TabInfo>) -> bool {
    if a.row_count() != b.row_count() {
        return false;
    }
    (0..a.row_count()).all(|i| match (a.row_data(i), b.row_data(i)) {
        (Some(x), Some(y)) => x.id == y.id,
        _ => false,
    })
}

/// Find the terminal row with `tab_id`, apply `mutator`, and write it back.
fn update_terminal_row(
    model: &VecModel<TerminalState>,
    tab_id: &str,
    mutator: impl FnOnce(&mut TerminalState),
) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                return;
            }
        }
    }
}

fn refresh_panes(
    window: &AppWindow,
    layout: &crate::layout::Layout,
    content: (f32, f32),
    tabs_model: &VecModel<TabInfo>,
    panes_model: &VecModel<PaneInfo>,
    splitters_model: &VecModel<SplitterInfo>,
) {
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, splits) = layout.flatten(0.0, 0.0, cw, ch);

    let pane_infos: Vec<PaneInfo> = panes
        .iter()
        .map(|p| {
            // Map this pane's tab ids to their TabInfo rows (skipping any not yet
            // in the model).
            let tabs: Vec<TabInfo> = p
                .tabs
                .iter()
                .filter_map(|tid| {
                    (0..tabs_model.row_count()).find_map(|i| {
                        let row = tabs_model.row_data(i)?;
                        (row.id.as_str() == tid.as_str()).then_some(row)
                    })
                })
                .collect();
            // Only the pane touching the top-right corner keeps room for the
            // floating toolbar icons (#122).
            let top_right = p.x + p.w >= cw - 0.5 && p.y <= 0.5;
            PaneInfo {
                id: p.id as i32,
                x: p.x,
                y: p.y,
                w: p.w,
                h: p.h,
                active_id: p.active.clone().into(),
                focused: p.focused,
                reserve_right: if top_right { 140.0 } else { 0.0 },
                tabs: ModelRc::from(Rc::new(VecModel::from(tabs))),
            }
        })
        .collect();

    // Update the models IN PLACE rather than replacing them, so the `for pane` /
    // `for sp` elements are reused: this keeps terminals from being recreated on
    // every refresh AND preserves the splitter's pointer-grab during a drag (a
    // fresh model would destroy the element mid-drag and drop the grab). When the
    // structure changes (split/close → different row count) a full rebuild is fine
    // since no drag is in flight.
    if panes_model.row_count() == pane_infos.len() {
        for (i, mut r) in pane_infos.into_iter().enumerate() {
            if let Some(old) = panes_model.row_data(i) {
                // Reuse the existing tab sub-model when the tabs are unchanged so a
                // geometry-only refresh doesn't churn the tab strips.
                if old.id == r.id && tabs_eq(&old.tabs, &r.tabs) {
                    r.tabs = old.tabs;
                }
            }
            panes_model.set_row_data(i, r);
        }
    } else {
        panes_model.set_vec(pane_infos);
    }

    let split_infos: Vec<SplitterInfo> = splits
        .iter()
        .map(|s| SplitterInfo {
            split_id: s.split_id as i32,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
            vertical: s.vertical,
        })
        .collect();
    if splitters_model.row_count() == split_infos.len() {
        for (i, r) in split_infos.into_iter().enumerate() {
            splitters_model.set_row_data(i, r);
        }
    } else {
        splitters_model.set_vec(split_infos);
    }

    if let Some(fp) = panes.iter().find(|p| p.focused) {
        if window.get_active_tab_id().as_str() != fp.active.as_str() {
            window.set_active_tab_id(fp.active.clone().into());
        }
    }
}

/// Hit-test a drag point (pane-area coords) to a target pane + drop zone, plus
/// the highlight rect the dropped tab would affect. Zone is one of
/// "tabstrip"/"left"/"right"/"up"/"down"/"center"; `None` when the point is
/// outside every pane. The 30% edge bands trigger a split; the tab strip and
/// middle drop into the pane's tab group.
fn drag_target(
    layout: &crate::layout::Layout,
    content: (f32, f32),
    x: f32,
    y: f32,
) -> Option<(u64, &'static str, (f32, f32, f32, f32))> {
    const STRIP: f32 = 36.0;
    const EDGE: f32 = 0.30;
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, _) = layout.flatten(0.0, 0.0, cw, ch);
    let p = panes
        .iter()
        .find(|p| x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h)?;
    let body_top = p.y + STRIP;
    if y < body_top {
        let ix = x.clamp(p.x + 3.0, p.x + p.w - 3.0) - 3.0;
        return Some((p.id, "tabstrip", (ix, p.y + 4.0, 6.0, STRIP - 8.0)));
    }
    let bw = p.w.max(1.0);
    let bh = (p.h - STRIP).max(1.0);
    let rx = (x - p.x) / bw;
    let ry = (y - body_top) / bh;
    let (dl, dr, dt, db) = (rx, 1.0 - rx, ry, 1.0 - ry);
    let m = dl.min(dr).min(dt).min(db);
    let (zone, rect) = if m > EDGE {
        ("center", (p.x, p.y, p.w, p.h))
    } else if m == dl {
        ("left", (p.x, p.y, p.w * 0.5, p.h))
    } else if m == dr {
        ("right", (p.x + p.w * 0.5, p.y, p.w * 0.5, p.h))
    } else if m == dt {
        ("up", (p.x, p.y, p.w, p.h * 0.5))
    } else {
        ("down", (p.x, p.y + p.h * 0.5, p.w, p.h * 0.5))
    };
    Some((p.id, zone, rect))
}

// ---------------------------------------------------------------------------
// Tab callbacks
// ---------------------------------------------------------------------------

fn wire_tab_callbacks(
    window: &AppWindow,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    render_gates: RenderGates,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
) {
    // Ctrl+Tab / Ctrl+Shift+Tab cycle within the currently focused pane (#294).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs_cycle = bufs.clone();
        window.on_cycle_tab(move |reverse: bool| {
            let next = layout.borrow_mut().cycle_focused_tab(reverse);
            let Some(id) = next else {
                return;
            };
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
                rebuild_tab_display(&w, &bufs_cycle, &id);
            }
        });
    }

    // Select a tab inside a pane: make it that pane's active tab and focus the
    // pane. refresh_panes propagates active-tab-id (→ sidebar refresh).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs_tab_sel = bufs.clone();
        window.on_pane_tab_selected(move |pane_id: i32, id: SharedString| {
            let id = id.to_string();
            {
                let mut lay = layout.borrow_mut();
                lay.focused = pane_id as u64;
                if let Some(l) = lay.leaf_mut(pane_id as u64) {
                    if l.tabs.iter().any(|t| t == &id) {
                        l.active = id.clone();
                    }
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
                // Tab just became visible — render any output ingested while it
                // was in the background (e.g. another session was unzipping).
                rebuild_tab_display(&w, &bufs_tab_sel, &id);
            }
        });
    }

    // Drag-to-reorder within a pane's strip: move the tab at `from` one slot in
    // `dir`. Only the pane's own tab order changes; content shows by active id.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_tab_reorder(move |pane_id: i32, from: i32, dir: i32| {
            {
                let mut lay = layout.borrow_mut();
                if let Some(l) = lay.leaf_mut(pane_id as u64) {
                    let n = l.tabs.len() as i32;
                    if n <= 1 {
                        return;
                    }
                    let from = from.clamp(0, n - 1);
                    let to = (from + dir).clamp(0, n - 1);
                    if from == to {
                        return;
                    }
                    let item = l.tabs.remove(from as usize);
                    l.tabs.insert(to as usize, item);
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

    // Close a tab: tear down its session / buffers, drop it from the models, then
    // remove it from the split tree (which re-homes the pane's active tab and
    // collapses the pane if it becomes empty).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_tab_closed(move |_pane_id: i32, id: SharedString| {
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
            if let Some(gate) = render_gates.lock().unwrap().remove(&id) {
                gate.close();
            }
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

            layout.borrow_mut().remove_tab(&id);
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

    // "+" in a pane's strip: focus the welcome page (there is a single welcome
    // tab; move focus to whichever pane owns it and make it active).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_new_tab(move |pane_id: i32| {
            // In welcome-as-sidebar mode there is no welcome tab — the session list
            // lives in the left panel, so "+" has nothing to open.
            if weak
                .upgrade()
                .map(|w| w.get_welcome_as_sidebar())
                .unwrap_or(false)
            {
                return;
            }
            {
                let mut lay = layout.borrow_mut();
                if let Some(owner) = lay.leaf_of_tab("welcome") {
                    lay.focused = owner;
                    if let Some(l) = lay.leaf_mut(owner) {
                        l.active = "welcome".into();
                    }
                } else {
                    lay.focused = pane_id as u64;
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

    // Click anywhere in a pane → focus it (drives which terminal the sidebar and
    // key routing follow). A single pane is always focused, so this is a no-op
    // until splits exist.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_focus(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                if lay.leaf(pane_id as u64).is_some() {
                    lay.focused = pane_id as u64;
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

    // Drag a splitter to re-balance the two panes it divides. `pos` is the new
    // boundary position in content coordinates along the split's axis; we look
    // the split's axis window up from a fresh flatten and convert it to a ratio.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_splitter_drag(move |split_id: i32, pos: f32, _vertical: bool| {
            {
                let mut lay = layout.borrow_mut();
                let (cw, ch) = content_size.get();
                let extent = {
                    let (_, splits) = lay.flatten(0.0, 0.0, cw.max(1.0), ch.max(1.0));
                    splits
                        .iter()
                        .find(|s| s.split_id == split_id as u64)
                        .map(|s| (s.axis_start, s.axis_len))
                };
                if let Some((start, len)) = extent {
                    lay.set_ratio(split_id as u64, start, len, pos);
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

    // Split a pane: peel `tab-id` out of pane `pane-id` into a new pane on the
    // given side ("left"/"right"/"up"/"down"). Needs >1 tab so the source pane
    // doesn't empty and immediately collapse back.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_split(
            move |pane_id: i32, tab_id: SharedString, dir: SharedString| {
                let tab_id = tab_id.to_string();
                {
                    let mut lay = layout.borrow_mut();
                    let can = lay
                        .leaf(pane_id as u64)
                        .map(|l| l.tabs.len() > 1 && l.tabs.iter().any(|t| t == &tab_id))
                        .unwrap_or(false);
                    if !can {
                        return;
                    }
                    let (d, before) = match dir.as_str() {
                        "left" => (crate::layout::Dir::Horizontal, true),
                        "right" => (crate::layout::Dir::Horizontal, false),
                        "up" => (crate::layout::Dir::Vertical, true),
                        _ => (crate::layout::Dir::Vertical, false), // "down"
                    };
                    lay.split(pane_id as u64, d, &tab_id, before);
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
            },
        );
    }

    // Merge a split pane back into another pane. The source pane's tabs are
    // appended to the first remaining pane, then the emptied source collapses.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_merge(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                lay.merge_leaf_into_other(pane_id as u64);
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

    // Drag-to-split: while a tab is dragged over the pane area, highlight the
    // drop zone the cursor is in (an edge band → split, the middle → move).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        window.on_tab_drag_move(move |_tab_id: SharedString, x: f32, y: f32| {
            if let Some(w) = weak.upgrade() {
                match drag_target(&layout.borrow(), content_size.get(), x, y) {
                    Some((_, _, (hx, hy, hw, hh))) => {
                        w.set_drag_active(true);
                        w.set_drag_hl_x(hx);
                        w.set_drag_hl_y(hy);
                        w.set_drag_hl_w(hw);
                        w.set_drag_hl_h(hh);
                    }
                    None => w.set_drag_active(false),
                }
            }
        });
    }

    // Drop: split the target pane toward the dropped-on edge (peeling the tab
    // into the new pane), or drop into another pane's tab group from the middle
    // / tab strip (IDEA-style merge by dragging onto the tab row).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_tab_drag_drop(move |tab_id: SharedString, x: f32, y: f32| {
            let tab_id = tab_id.to_string();
            let target = drag_target(&layout.borrow(), content_size.get(), x, y);
            if let Some((pane, zone, _)) = target {
                let mut lay = layout.borrow_mut();
                let src = lay.leaf_of_tab(&tab_id);
                match zone {
                    "left" => {
                        lay.split(pane, crate::layout::Dir::Horizontal, &tab_id, true);
                    }
                    "right" => {
                        lay.split(pane, crate::layout::Dir::Horizontal, &tab_id, false);
                    }
                    "up" => {
                        lay.split(pane, crate::layout::Dir::Vertical, &tab_id, true);
                    }
                    "down" => {
                        lay.split(pane, crate::layout::Dir::Vertical, &tab_id, false);
                    }
                    "tabstrip" => {
                        if src != Some(pane) {
                            lay.move_tab(&tab_id, pane);
                        }
                    }
                    _ => {
                        if src != Some(pane) {
                            lay.move_tab(&tab_id, pane);
                        }
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_drag_active(false);
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
}

// ---------------------------------------------------------------------------
// SFTP callbacks
// ---------------------------------------------------------------------------

fn wire_sftp_callbacks(window: &AppWindow, sftp_handles: SftpHandles, sftp_last_cwd: SftpLastCwd) {
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
            // If the user has checked 2+ entries, ANY download (right-click,
            // row button or the toolbar) packs the whole checked set into one
            // archive (#100) — this matches "download these together". A single
            // checked item (or none) downloads the clicked file as-is.
            let (arc_dir, arc_names) = weak
                .upgrade()
                .and_then(|w| {
                    let terminals = w.get_terminals();
                    let tm = terminals
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    let paths = collect_sftp_selected(tm, &tab_id);
                    if paths.len() >= 2 {
                        let dir = active_sftp_path(&w, &tab_id);
                        let names: Vec<String> = paths
                            .iter()
                            .map(|p| {
                                p.trim_end_matches('/')
                                    .rsplit(['/', '\\'])
                                    .next()
                                    .unwrap_or(p)
                                    .to_string()
                            })
                            .collect();
                        clear_sftp_selection(tm, &tab_id);
                        Some((dir, names))
                    } else {
                        None
                    }
                })
                .map(|(d, n)| (Some(d), n))
                .unwrap_or((None, Vec::new()));
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
                        if let Some(ref dir) = arc_dir {
                            h.download_archive(dir.clone(), arc_names.clone(), preset);
                        } else {
                            h.download(remote_path, preset);
                        }
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
                            if let Some(ref rdir) = arc_dir {
                                h.download_archive(rdir.clone(), arc_names.clone(), local_dir);
                            } else {
                                h.download(remote_path, local_dir);
                            }
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
                std::thread::spawn(move || {
                    // The remote SFTP upload handles a file or a whole directory;
                    // only the local picker differs (#85). Folder uploads one dir;
                    // file mode allows selecting several at once.
                    let locals: Vec<std::path::PathBuf> = if folder {
                        rfd::FileDialog::new()
                            .pick_folder()
                            .map(|p| vec![p])
                            .unwrap_or_default()
                    } else {
                        rfd::FileDialog::new()
                            .pick_files()
                            .map(|v| v.into_iter().collect())
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
                    // Refresh re-syncs the left tree too, not just the file list (#189).
                    h.refresh_dir(path);
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

    // SFTP file-list sorting (#248): click a header to cycle asc -> desc -> default.
    {
        let weak = window.as_weak();
        window.on_sftp_sort_request(move |tab_id: SharedString, key: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                let key = key.to_string();
                let next_dir = if row.sftp_sort_key.as_str() != key || row.sftp_sort_dir == 0 {
                    1
                } else if row.sftp_sort_dir > 0 {
                    -1
                } else {
                    0
                };
                let next_key = if next_dir == 0 { String::new() } else { key };
                row.sftp_entries =
                    sorted_sftp_entries_from_model(&row.sftp_entries, &next_key, next_dir);
                row.sftp_sort_key = next_key.into();
                row.sftp_sort_dir = next_dir;
            });
        });
    }
    {
        let weak = window.as_weak();
        window.on_sftp_clear_sort(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                row.sftp_entries = sorted_sftp_entries_from_model(&row.sftp_entries, "", 0);
                row.sftp_sort_key = "".into();
                row.sftp_sort_dir = 0;
            });
        });
    }

    // SFTP multi-select: toggle a row's checkbox + recount (#100).
    {
        let weak = window.as_weak();
        window.on_sftp_toggle_select(move |tab_id: SharedString, idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            for ti in 0..tm.row_count() {
                let Some(row) = tm.row_data(ti) else { continue };
                if row.id.as_str() != tab_id.as_str() {
                    continue;
                }
                if let Some(em) = row
                    .sftp_entries
                    .as_any()
                    .downcast_ref::<VecModel<SftpEntry>>()
                {
                    let i = idx as usize;
                    if let Some(mut e) = em.row_data(i) {
                        e.selected = !e.selected;
                        em.set_row_data(i, e);
                    }
                    let mut n = 0;
                    for ei in 0..em.row_count() {
                        if em.row_data(ei).map(|x| x.selected).unwrap_or(false) {
                            n += 1;
                        }
                    }
                    let mut r = row.clone();
                    r.sftp_selected_count = n;
                    tm.set_row_data(ti, r);
                }
                break;
            }
        });
    }
    // SFTP multi-select: download all checked entries into one folder (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            // Single selection downloads as a plain file (no compression, #100.3);
            // multiple selections are tar-packed into one archive on the remote
            // (#100.2) — this also avoids the concurrent-transfer races (#100.1).
            let single = paths.len() == 1;
            let remote_dir = active_sftp_path(&w, tab_id.as_str());
            let names: Vec<String> = paths
                .iter()
                .map(|p| {
                    p.trim_end_matches('/')
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(p)
                        .to_string()
                })
                .collect();
            let preset = w.get_download_dir().to_string();
            let always_ask = w.get_download_always_ask();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(tab_id.as_str()) {
                        if single {
                            h.download(paths[0].clone(), preset.clone());
                        } else {
                            h.download_archive(remote_dir.clone(), names.clone(), preset.clone());
                        }
                    }
                }
                w.set_download_open(true);
            } else {
                let sftp_handles = sftp_handles.clone();
                let weak2 = weak.clone();
                let tab = tab_id.to_string();
                std::thread::spawn(move || {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let dir = dir.to_string_lossy().to_string();
                        if let Ok(handles) = sftp_handles.lock() {
                            if let Some(h) = handles.get(&tab) {
                                if single {
                                    h.download(paths[0].clone(), dir.clone());
                                } else {
                                    h.download_archive(
                                        remote_dir.clone(),
                                        names.clone(),
                                        dir.clone(),
                                    );
                                }
                            }
                        }
                        let _ = weak2.upgrade_in_event_loop(|w| w.set_download_open(true));
                    }
                });
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }
    // SFTP multi-select: delete all checked entries (confirmed in the UI) (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_delete_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    for p in &paths {
                        h.delete(p.clone());
                    }
                }
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }

    // Context menu → 查看 (read-only) / 编辑 (editable). Both load the file's
    // text into the built-in editor instead of an external app (#70).
    // SFTP remote-to-remote copy (#203): stage through a local temp directory,
    // then upload into the target session's current SFTP directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_to_target(
            move |tab_id: SharedString, remote_path: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let paths = vec![remote_path.to_string()];
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_selected_to_target(
            move |tab_id: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let terminals = w.get_terminals();
                let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                    return;
                };
                let paths = collect_sftp_selected(tm, tab_id.as_str());
                if paths.is_empty() {
                    return;
                }
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
                clear_sftp_selection(tm, tab_id.as_str());
            },
        );
    }
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
                        let to =
                            format!("{}/{}", parent_path(&target).trim_end_matches('/'), value);
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
    // Runtime SSH tunnel panel (#206). These tunnels live only for the active
    // connection; saved session configuration remains unchanged.
    {
        let handles_rc = handles.clone();
        window.on_tunnel_add(
            move |tab_id: SharedString,
                  name: SharedString,
                  kind: SharedString,
                  bind: SharedString,
                  bind_port: SharedString,
                  host: SharedString,
                  host_port: SharedString| {
                let kind = kind.to_string();
                if kind != "local" && kind != "dynamic" {
                    return;
                }
                let Ok(bind_port) = bind_port.trim().parse::<u16>() else {
                    return;
                };
                let host_port = if kind == "dynamic" {
                    0
                } else {
                    match host_port.trim().parse::<u16>() {
                        Ok(p) => p,
                        Err(_) => return,
                    }
                };
                let forward = crate::config::PortForward {
                    kind,
                    name: name.trim().to_string(),
                    bind_addr: bind.trim().to_string(),
                    bind_port,
                    host: host.trim().to_string(),
                    host_port,
                };
                if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                    handle.add_tunnel(format!("runtime-{}", uuid::Uuid::new_v4()), forward);
                }
            },
        );
    }
    {
        let handles_rc = handles.clone();
        window.on_tunnel_stop(move |tab_id: SharedString, tunnel_id: SharedString| {
            if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                handle.stop_tunnel(tunnel_id.to_string());
            }
        });
    }

    // --- Command bar (#55): run command + quick-command management ---------
    {
        let handles_rc = handles.clone();
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_run_command(
            move |tab_id: SharedString, cmd: SharedString, to_all: bool| {
                let Some((line, bytes)) = encode_command_bar_input(&cmd) else {
                    return;
                };
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
            },
        );
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
    // History search (#101): filter the dropdown by a case-insensitive substring.
    // The current query is shared so a delete from a filtered view re-filters.
    let hist_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_search_history(move |query: SharedString| {
            *hist_query.borrow_mut() = query.to_string();
            if let Some(w) = weak.upgrade() {
                w.set_history_view(history_view_model(&store_rc.borrow(), &query));
            }
        });
    }
    // Delete a history entry by its command text (#101) — index-free so it works
    // from the filtered dropdown view.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_delete_history_cmd(move |cmd: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                if let Some(idx) = s.command_history().iter().position(|c| c == cmd.as_str()) {
                    s.remove_command_history(idx);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                let s = store_rc.borrow();
                w.set_command_history(history_model(&s));
                w.set_history_view(history_view_model(&s, &hist_query.borrow()));
            }
        });
    }
    // Runtime-only collapse state for quick-command groups (#55) — like the
    // welcome session groups, this is not persisted across restarts. Starts with
    // every group collapsed (default-collapsed view).
    let collapsed_quick_groups: Rc<RefCell<std::collections::HashSet<String>>> =
        Rc::new(RefCell::new(all_quick_group_names(&store.borrow())));
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_add_quick_command(
            move |name: SharedString,
                  command: SharedString,
                  group: SharedString,
                  send_enter: bool| {
                let name = name.trim().to_string();
                let command = command.to_string();
                let group = group.trim().to_string();
                if name.is_empty() || command.trim().is_empty() {
                    return;
                }
                {
                    let mut s = store_rc.borrow_mut();
                    let mut v = s.quick_commands().to_vec();
                    v.push(crate::config::QuickCommand {
                        name,
                        command,
                        group,
                        send_enter,
                    });
                    s.set_quick_commands(v);
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
                }
            },
        );
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
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
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_toggle_quick_group(move |group: SharedString| {
            let g = group.to_string();
            {
                let mut set = collapsed.borrow_mut();
                if !set.remove(&g) {
                    set.insert(g);
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Edit (#55): load the entry into the manage form in edit mode.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_edit_quick_command(move |index: i32| {
            let i = index as usize;
            let cmd = store_rc.borrow().quick_commands().get(i).cloned();
            if let (Some(c), Some(w)) = (cmd, weak.upgrade()) {
                w.set_qcm_name(c.name.into());
                w.set_qcm_command(c.command.into());
                w.set_qcm_group(c.group.into());
                w.set_qcm_send_enter(c.send_enter);
                w.set_qcm_edit_index(index);
                w.set_quick_cmd_manage_open(true);
            }
        });
    }
    // Save an edited entry (#55).
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_save_quick_command(
            move |index: i32,
                  name: SharedString,
                  command: SharedString,
                  group: SharedString,
                  send_enter: bool| {
                let name = name.trim().to_string();
                let command = command.to_string();
                let group = group.trim().to_string();
                if name.is_empty() || command.trim().is_empty() {
                    return;
                }
                {
                    let mut s = store_rc.borrow_mut();
                    s.update_quick_command(
                        index as usize,
                        crate::config::QuickCommand {
                            name,
                            command,
                            group,
                            send_enter,
                        },
                    );
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
                }
            },
        );
    }
    // Duplicate (#55): clone the entry as a starting point.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_duplicate_quick_command(move |index: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                if let Some(c) = v.get(index as usize).cloned() {
                    let dup = crate::config::QuickCommand {
                        name: format!("{} (copy)", c.name),
                        command: c.command,
                        group: c.group,
                        send_enter: c.send_enter,
                    };
                    v.insert(index as usize + 1, dup);
                    s.set_quick_commands(v);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Move to a group (#55): "default" maps to the empty (ungrouped) group.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_move_quick_command(move |index: i32, group: SharedString| {
            let target = group.to_string();
            let target = if target == "default" {
                String::new()
            } else {
                target
            };
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                if let Some(c) = v.get_mut(index as usize) {
                    c.group = target;
                }
                s.set_quick_commands(v);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Quick-group create / rename (#55).
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_submit_quick_group(move |orig: SharedString, name: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                if orig.is_empty() {
                    s.add_quick_group(name.to_string());
                } else {
                    s.rename_quick_group(&orig.to_string(), name.to_string());
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Quick-group delete (#55) — UI only offers this on empty groups.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_delete_quick_group(move |name: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                s.remove_quick_group(&name.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
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
        let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
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
                        if let Some(h) = term_buf(&ctx.bufs, tab_id.as_str()) {
                            let mut b = h.lock().unwrap();
                            let (rows, cols) = b.parser.screen().size();
                            b.parser = vt100::Parser::new(rows, cols, 5000);
                            b.history.clear();
                            b.prev.clear();
                            b.displayed_text.clear();
                            b.view_offset = 0;
                            b.sel_anchor = None;
                            b.sel_focus = None;
                            b.sel_ranges.clear();
                            b.raw.clear();
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
            let app_cursor = if let Some(h) = term_buf(&bufs, tab_id.as_str()) {
                let mut b = h.lock().unwrap();
                // Typing snaps the view back to the live bottom so the
                // user always sees what they're entering.
                b.view_offset = 0;
                b.parser.screen().application_cursor()
            } else {
                false
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
            // Two-layer defence:
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
            //     was pressed (key="", shift=true) a few lines above. Drop a
            //     Backspace arriving within the guarded interval unless a real
            //     intervening key has already cleared the marker.
            // Any real intervening key proves a previous Shift/IME marker is no
            // longer paired with this Backspace. Without clearing it, the broad
            // safety window drops legitimate Vim insert-mode Backspace (#319).
            if key.as_str() != "\u{0008}" && !key.as_str().is_empty() {
                *last_shift_time.lock().unwrap() = None;
            }

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
                // Do not consult the live VK_BACK state here. Under UI/SSH
                // backlog the key-up can be processed before this callback, so
                // that test drops a genuine queued Backspace (#319).
                tracing::info!("[KEY_DIAG] Backspace PASSED all filters → sent to PTY");
            }

            if should_drop_bare_ctrl_marker(
                key.as_str(),
                ctrl,
                bare_ctrl_marker_workaround_enabled(),
            ) {
                tracing::debug!(
                    "send_key: dropped Slint bare Ctrl modifier marker {}",
                    redact_key(key.as_str())
                );
                return;
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
        let weak_resize = window.as_weak();
        // The Slint side now measures the real Consolas cell size (via a hidden
        // probe Text) and passes whole column/row counts directly, so there is
        // no pixel→cell guesswork here.  This keeps full-screen programs like
        // nano from over-counting rows and clipping their bottom shortcut bar.
        // Debounce PTY resizes (#163): a layout reflow (a tab becoming visible,
        // the SFTP panel docking, a window drag) can momentarily report a
        // near-zero width, which collapses term-cols to its 10-col floor.
        // Applying that to the remote PTY immediately resizes the server to 10
        // columns and reflows vt100 — garbling running output (e.g. a `git clone`
        // progress meter wraps at 10 chars). Coalesce rapid changes and apply
        // only the size that's still set after a short quiet period, so a
        // transient bad value never reaches the server.
        let pending_size: Rc<RefCell<HashMap<String, (u32, u32)>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let resize_debounce = Rc::new(slint::Timer::default());
        window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
            // A hidden terminal (inactive tab, or a split sibling not currently
            // shown) reports 0 width/height. Ignore those: flooring 0 to the 10-col
            // minimum and applying it would shrink that tab's PTY *and* poison
            // `last_term_size`, so the next connection (e.g. "Duplicate connection")
            // would start at 10 cols and wrap its first output to ~10 chars (#v0.5).
            // Only genuine, visible sizes drive a resize.
            if cols_f < 1.0 || rows_f < 1.0 {
                return;
            }
            let cols = (cols_f as u32).max(10);
            let rows = (rows_f as u32).max(5);
            pending_size
                .borrow_mut()
                .insert(tab_id.to_string(), (cols, rows));
            let pending = pending_size.clone();
            let handles = handles.clone();
            let bufs = bufs_resize.clone();
            let last = last_term_size.clone();
            let weak = weak_resize.clone();
            // (Re)arm the single-shot timer; rapid changes keep resetting it so
            // only the final, settled size is applied.
            resize_debounce.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(150),
                move || {
                    let settled: Vec<(String, (u32, u32))> = pending.borrow_mut().drain().collect();
                    for (tab, (cols, rows)) in settled {
                        tracing::debug!("terminal_resize tab={} cols={} rows={}", tab, cols, rows);
                        apply_terminal_resize(&handles, &bufs, &last, &tab, cols, rows);
                        // Re-render so the reflowed (or resized) grid shows at once
                        // instead of waiting for the next remote output (#169).
                        if let Some(win) = weak.upgrade() {
                            rebuild_tab_display(&win, &bufs, &tab);
                        }
                    }
                },
            );
        });
    }

    // Ctrl+Shift+C: copy current terminal screen to clipboard.
    {
        let bufs = bufs.clone();
        window.on_copy_terminal_text(move |tab_id: SharedString| {
            let text = term_buf(&bufs, tab_id.as_str())
                .map(|h| {
                    let buf = h.lock().unwrap();
                    // Copy the drag-selection when there is one, else the
                    // whole displayed screen.
                    let sel = buf.extract_selection_text();
                    if sel.is_empty() {
                        buf.displayed_text.join("\n")
                    } else {
                        sel
                    }
                })
                .unwrap_or_default();
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
        let bufs = bufs.clone();
        let weak = window.as_weak();
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
            let bracketed = terminal_uses_bracketed_paste(&bufs, tab_id.as_str());
            let weak = weak.clone();
            let tab_id = tab_id.to_string();
            std::thread::spawn(move || {
                match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    Ok(text) => {
                        if text.contains(['\r', '\n']) {
                            let large = paste_requires_large_review(&text);
                            let preview = text.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_paste_confirm_tab(tab_id.into());
                                    w.set_paste_confirm_text(text.into());
                                    w.set_paste_confirm_preview(preview.into());
                                    w.set_paste_confirm_large(large);
                                    w.set_paste_confirm_open(true);
                                }
                            });
                        } else {
                            let bytes = encode_pasted_text(&text, bracketed);
                            let _ = sender.send(SessionCommand::RawInput(bytes));
                        }
                    }
                    Err(e) => tracing::warn!("paste_from_clipboard: clipboard error: {}", e),
                }
            });
        });
    }

    // Accept a previously reviewed multi-line paste (#262).
    {
        let handles_paste = handles.clone();
        let bufs_paste = bufs.clone();
        let weak = window.as_weak();
        window.on_paste_confirmed(move |tab_id: SharedString| {
            let Some(sender) = handles_paste
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone())
            else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            let text = w.get_paste_confirm_text().to_string();
            let bracketed = terminal_uses_bracketed_paste(&bufs_paste, tab_id.as_str());
            let _ = sender.send(SessionCommand::RawInput(encode_pasted_text(
                &text, bracketed,
            )));
            w.set_paste_confirm_open(false);
        });
    }

    window.on_paste_confirm_cancelled(|| {});

    // Context menu → 清空缓存: reset the local vt100 buffer (drops scrollback),
    // wipe the displayed screen, then nudge the remote to redraw a fresh prompt.
    {
        let bufs_clear = bufs.clone();
        let handles_clear = handles.clone();
        let weak = window.as_weak();
        window.on_clear_terminal(move |tab_id: SharedString| {
            let tid = tab_id.to_string();
            if let Some(h) = term_buf(&bufs_clear, &tid) {
                let mut buf = h.lock().unwrap();
                let (rows, cols) = buf.parser.screen().size();
                buf.parser = vt100::Parser::new(rows, cols, 5000);
                buf.find_query.clear();
                buf.history = VecDeque::new(); // recycle the session scrollback
                buf.prev = Vec::new();
                buf.view_offset = 0;
                buf.sel_anchor = None;
                buf.sel_focus = None;
                buf.sel_ranges.clear();
                buf.displayed_text = Vec::new();
                buf.raw.clear();
            }
            if let Some(win) = weak.upgrade() {
                set_terminal_row(&win, &tid, |row| {
                    row.spans = ModelRc::from(Rc::new(VecModel::<TermSpan>::default()));
                    row.find_matches = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.selection = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.cursor_row = 0;
                    row.cursor_col = 0;
                    row.rows_used = 0;
                    row.scroll_max = 0;
                    row.scroll_offset = 0;
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
            let (matches, jumped) = with_term_buf(&bufs_find, &tid, |buf| {
                buf.find_query = q.clone();
                let mut matches = compute_find_matches(&buf.displayed_text, &q);
                let jumped = matches.is_empty() && buf.scroll_to_first_find_match(&q);
                if jumped {
                    buf.render();
                    matches = compute_find_matches(&buf.displayed_text, &q);
                }
                (matches, jumped)
            })
            .unwrap_or_default();
            if let Some(win) = weak.upgrade() {
                if jumped {
                    rebuild_tab_display(&win, &bufs_find, &tid);
                    return;
                }
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
            with_term_buf(&bufs_scroll, &tid, |buf| {
                // Scroll within our own session scrollback (history lines above
                // the live screen).  Offset 0 = live bottom.
                let max_off = buf.history.len() as i64;
                let cur = buf.view_offset as i64;
                buf.view_offset = (cur + delta as i64).clamp(0, max_off) as usize;
            });
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_scroll, &tid);
            }
        });
    }

    // Wheel inside an alt-screen program (tmux / less / vim): forward it to the PTY
    // so the program scrolls, instead of doing nothing (#170 — FinalShell /
    // MobaXterm behave this way). If the app is tracking the mouse (e.g. tmux with
    // `mouse on`), send a real wheel mouse-event in the encoding it asked for;
    // otherwise fall back to arrow keys (xterm "alternate scroll"), which scrolls
    // less / man / vim.
    {
        let bufs_wheel = bufs.clone();
        let handles_wheel = handles.clone();
        window.on_terminal_wheel(move |tab_id: SharedString, dir: i32, col: i32, row: i32| {
            let tid = tab_id.to_string();
            let bytes = term_buf(&bufs_wheel, &tid).map(|h| {
                let buf = h.lock().unwrap();
                let screen = buf.parser.screen();
                if screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
                    // 1-based cell under the cursor, clamped to the screen.
                    let (rows, cols) = screen.size();
                    let c = (col.clamp(0, cols.saturating_sub(1) as i32) as u16) + 1;
                    let r = (row.clamp(0, rows.saturating_sub(1) as i32) as u16) + 1;
                    let btn: u16 = if dir > 0 { 64 } else { 65 }; // wheel up / down
                    if screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr {
                        format!("\x1b[<{btn};{c};{r}M").into_bytes()
                    } else {
                        // Legacy X10 encoding: ESC [ M  Cb Cx Cy  (each value + 32).
                        let cb = (btn + 32) as u8;
                        let cx = (c.min(223) + 32) as u8;
                        let cy = (r.min(223) + 32) as u8;
                        vec![0x1b, b'[', b'M', cb, cx, cy]
                    }
                } else {
                    // alternate-scroll: 3 arrow presses per notch, app-cursor aware.
                    let one: &[u8] = if dir > 0 {
                        if screen.application_cursor() {
                            b"\x1bOA"
                        } else {
                            b"\x1b[A"
                        }
                    } else if screen.application_cursor() {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    };
                    one.repeat(3)
                }
            });
            if let (Some(bytes), Some(h)) = (bytes, handles_wheel.borrow().get(&tid)) {
                h.send_raw(bytes);
            }
        });
    }

    // Scrollbar drag → jump to an absolute scrollback offset (#103).
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll_to(move |tab_id: SharedString, offset: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_scroll, &tid, |buf| {
                let max_off = buf.history.len() as i64;
                buf.view_offset = (offset as i64).clamp(0, max_off) as usize;
            });
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_scroll, &tid);
            }
        });
    }

    // Drag-selection lifecycle.
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_start(move |tab_id: SharedString, row: i32, col: i32, ctrl: bool, shift: bool| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_sel, &tid, |buf| {
                let (rows, cols) = buf.parser.screen().size();
                let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
                let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
                // Anchor + focus in absolute scrollback coordinates.
                let abs = buf.vis_to_abs(r);
                let point = (abs, c);
                if ctrl && !shift {
                    buf.sel_ranges.push((point, point));
                } else if shift && !buf.sel_ranges.is_empty() {
                    let anchor = buf.sel_ranges.last().map(|range| range.0).unwrap_or(point);
                    if let Some(range) = buf.sel_ranges.last_mut() {
                        *range = (anchor, point);
                    }
                } else {
                    buf.sel_ranges.clear();
                    buf.sel_ranges.push((point, point));
                }
                let (anchor, focus) = buf.sel_ranges.last().copied().unwrap_or((point, point));
                buf.sel_anchor = Some(anchor);
                buf.sel_focus = Some(focus);
            });
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_update(move |tab_id: SharedString, row: i32, col: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_sel, &tid, |buf| {
                let (rows, cols) = buf.parser.screen().size();
                let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
                let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
                if buf.sel_anchor.is_some() {
                    let abs = buf.vis_to_abs(r);
                    buf.sel_focus = Some((abs, c));
                    if let Some(range) = buf.sel_ranges.last_mut() {
                        range.1 = (abs, c);
                    }
                }
            });
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
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
            let text = with_term_buf(&bufs_sel, &tid, |buf| {
                // Selection endpoints are inclusive, so extracting an
                // anchor-only range returns the character under a plain click.
                // Compare coordinates instead of using extracted text as the
                // click-vs-drag signal (#319).
                if !buf.selection_has_extent() {
                    buf.sel_anchor = None;
                    buf.sel_focus = None;
                    buf.sel_ranges.clear();
                    return None;
                }
                let extracted = buf.extract_selection_text();
                if extracted.is_empty() {
                    // Zero-area selection (a plain click) → clear it.
                    buf.sel_anchor = None;
                    buf.sel_focus = None;
                    buf.sel_ranges.clear();
                    None
                } else {
                    Some(extracted)
                }
            })
            .flatten();
            match text {
                Some(t) if !t.is_empty() => {
                    // Auto-copy on release (select-to-copy, PuTTY style).
                    std::thread::spawn(move || clipboard_set_text(t));
                }
                _ => {}
            }
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
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
            let Some(h) = term_buf(&bufs_sel, &tid) else {
                return;
            };
            {
                let mut buf = h.lock().unwrap();
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
                if let Some(range) = buf.sel_ranges.last_mut() {
                    range.1 = (abs, focus_col);
                }
            }
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_sel, &tid);
            }
        });
    }
}

fn webdav_url(base: &str, remote_path: &str) -> Result<String> {
    let base = base.trim().trim_end_matches('/');
    if !base.starts_with("http://") && !base.starts_with("https://") {
        anyhow::bail!(
            "{}",
            t(
                "WebDAV 地址必须以 http:// 或 https:// 开头",
                "WebDAV URL must start with http:// or https://"
            )
        );
    }
    if base.starts_with("http://") && webdav_url_uses_port(base, 5006) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5006 通常是 HTTPS 端口，请改用 https://...:5006；如果要用 HTTP，请改用 5005 端口",
                "FnOS WebDAV port 5006 is usually HTTPS; use https://...:5006, or use port 5005 for HTTP"
            )
        );
    }
    if base.starts_with("https://") && webdav_url_uses_port(base, 5005) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5005 通常是 HTTP 端口，请改用 http://...:5005；如果要用 HTTPS，请改用 5006 端口",
                "FnOS WebDAV port 5005 is usually HTTP; use http://...:5005, or use port 5006 for HTTPS"
            )
        );
    }
    if base.ends_with(".json") {
        return Ok(base.to_string());
    }
    let remote = remote_path.trim().trim_start_matches('/');
    if (webdav_url_uses_port(base, 5005) || webdav_url_uses_port(base, 5006))
        && !webdav_url_has_path(base)
        && !remote.contains('/')
    {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 需要写入某个共享目录，不能直接写到根路径；请把 WebDAV 地址改成 https://IP:5006/all/，或把远端文件改成 all/meatshell-connections.json",
                "FnOS WebDAV needs a writable shared folder, not the server root; use https://IP:5006/all/ or set the remote file to all/meatshell-connections.json"
            )
        );
    }
    if remote.is_empty() {
        anyhow::bail!("{}", t("远端文件不能为空", "remote file cannot be empty"));
    }
    Ok(format!("{base}/{remote}"))
}

fn webdav_url_uses_port(base: &str, port: u16) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    let host_port = authority.split('/').next().unwrap_or(authority);
    host_port
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        == Some(port)
}

fn webdav_url_has_path(base: &str) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    authority
        .split_once('/')
        .is_some_and(|(_, path)| !path.is_empty())
}

fn webdav_auth_header(username: &str, password: &str) -> Option<String> {
    if username.is_empty() && password.is_empty() {
        return None;
    }
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    ))
}

fn webdav_auth_req(mut req: ureq::Request, auth: Option<&str>) -> ureq::Request {
    if let Some(auth) = auth {
        req = req.set("Authorization", auth);
    }
    req
}

fn webdav_agent(accept_invalid_certs: bool) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20));
    if accept_invalid_certs {
        let tls_config = ureq::rustls::ClientConfig::builder_with_provider(
            ureq::rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[&ureq::rustls::version::TLS12, &ureq::rustls::version::TLS13])
        .expect("rustls ring provider supports TLS 1.2 and TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(WebDavAcceptAnyCertVerifier))
        .with_no_client_auth();
        builder = builder.tls_config(Arc::new(tls_config));
    }
    builder.build()
}

fn webdav_error(e: ureq::Error) -> anyhow::Error {
    if let ureq::Error::Status(status, response) = e {
        let url = response.get_url().to_string();
        let body = response.into_string().unwrap_or_default();
        let body = body.trim();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(": {}", body.chars().take(240).collect::<String>())
        };
        if status == 400 {
            return anyhow::anyhow!(
                "{}: {url}: status code 400{detail}",
                t(
                    "请求被 WebDAV 服务拒绝，请检查地址协议/端口是否匹配，以及远端文件所在目录是否已开启 WebDAV 协议访问",
                    "WebDAV rejected the request; check the URL scheme/port and whether the remote folder allows WebDAV access"
                )
            );
        }
        if status == 405 {
            return anyhow::anyhow!(
                "{}: {url}: status code 405{detail}",
                t(
                    "当前 WebDAV 路径不允许上传；飞牛请写入已开启协议访问的共享目录，例如 WebDAV 地址填 https://IP:5006/all/，或远端文件填 all/meatshell-connections.json",
                    "The current WebDAV path does not allow upload; for FnOS, write into a shared folder such as https://IP:5006/all/ or set remote file to all/meatshell-connections.json"
                )
            );
        }
        return anyhow::anyhow!("{url}: status code {status}{detail}");
    }
    let msg = e.to_string();
    if msg.contains("UnknownIssuer") || msg.contains("invalid peer certificate") {
        anyhow::anyhow!(
            "{} ({msg})",
            t(
                "HTTPS 证书不受信任；如果这是可信 NAS/局域网 WebDAV，请在设置里开启“信任自签名/内网证书”",
                "HTTPS certificate is not trusted; enable \"Trust self-signed / intranet certs\" for a trusted NAS/LAN WebDAV"
            )
        )
    } else {
        anyhow::anyhow!("{msg}")
    }
}

fn webdav_parent_dirs(url: &str) -> Vec<String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return Vec::new();
    };
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 1 {
        return Vec::new();
    }
    let mut dirs = Vec::with_capacity(parts.len() - 1);
    let mut current = format!("{scheme}://{authority}");
    for part in parts.iter().take(parts.len() - 1) {
        current.push('/');
        current.push_str(part);
        current.push('/');
        dirs.push(current.clone());
        current.pop();
    }
    dirs
}

fn webdav_dir_missing_or_no_create_error() -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        t(
            "文件夹不存在也无权限创建",
            "folder does not exist and cannot be created"
        )
    )
}

fn webdav_dir_exists(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<bool> {
    let req = webdav_auth_req(agent.request("PROPFIND", url).set("Depth", "0"), auth);
    match req.call() {
        Ok(_) => Ok(true),
        Err(ureq::Error::Status(status, _)) if status == 404 || status == 409 => Ok(false),
        Err(ureq::Error::Status(status, _)) if status == 401 || status == 403 || status == 405 => {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

fn webdav_create_dir(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<()> {
    let req = webdav_auth_req(agent.request("MKCOL", url), auth);
    match req.call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, _)) if status == 405 => Ok(()),
        Err(ureq::Error::Status(status, _))
            if status == 401 || status == 403 || status == 404 || status == 409 =>
        {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

fn webdav_ensure_parent_dirs(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<()> {
    for dir in webdav_parent_dirs(url) {
        if !webdav_dir_exists(agent, &dir, auth)? {
            webdav_create_dir(agent, &dir, auth)?;
        }
    }
    Ok(())
}

fn webdav_put_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
    json: String,
) -> Result<()> {
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    webdav_ensure_parent_dirs(&agent, &url, auth.as_deref())?;
    let req = webdav_auth_req(
        agent.put(&url).set("Content-Type", "application/json"),
        auth.as_deref(),
    );
    req.send_string(&json).map(|_| ()).map_err(webdav_error)
}

fn webdav_get_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
) -> Result<String> {
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    let req = webdav_auth_req(agent.get(&url), auth.as_deref());
    req.call()
        .map_err(webdav_error)?
        .into_string()
        .map_err(|e| anyhow::anyhow!("{e}"))
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
/// Choose a UI font family that fontdb can actually resolve, falling back to the
/// embedded "Meatshell Mono" when the system font database is empty/unreadable.
///
/// macOS 26 (Tahoe) shipped a system where fontdb couldn't register the named
/// CJK font ("PingFang SC"), so hard-coding that name made the whole UI render
/// blank (#129). This probes the loaded faces and picks the first CJK-capable
/// family that exists; if none do, it returns the embedded font so the window is
/// still visible (Latin text shows; CJK may tofu — far better than a blank UI).
///
/// Emits a one-line WARN summary (faces loaded + chosen font) so the choice lands
/// in `error.log` for diagnostics without needing RUST_LOG.
fn resolve_ui_font_family() -> slint::SharedString {
    use fontdb::{Database, Family, Query, Stretch, Style, Weight};

    // Diagnostic / escape hatch (#129): force a specific UI font without a rebuild.
    // e.g. MEATSHELL_UI_FONT="Meatshell Mono" to test whether the embedded font
    // renders when system fonts don't. Empty value is ignored.
    if let Some(f) = std::env::var_os("MEATSHELL_UI_FONT") {
        let f = f.to_string_lossy().into_owned();
        if !f.trim().is_empty() {
            tracing::debug!(font = %f, "ui-font: overridden via MEATSHELL_UI_FONT");
            return f.into();
        }
    }

    let mut db = Database::new();
    db.load_system_fonts();
    let face_count = db.faces().count();

    // CJK-capable system families, most-preferred first, per platform. The UI
    // default font must cover CJK because TextInput doesn't glyph-fallback (#54).
    //
    // macOS note (#129): the modern system CJK fonts (PingFang SC, Hiragino) fail
    // to rasterize under femtovg on some macOS 26 machines — fontdb finds them but
    // every glyph comes out blank. The older Heiti/Songti faces render fine and
    // ship on every macOS, so we prefer them and keep PingFang only as a late
    // fallback. (Verified on an M2/macOS 26: Heiti SC/STHeiti/Songti SC render,
    // PingFang/Hiragino don't.) Power users can still force one via
    // MEATSHELL_UI_FONT. Heiti SC is a clean sans-serif (better for UI than the
    // serif Songti), so it leads.
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[
        "Heiti SC",
        "STHeiti",
        "Songti SC",
        "PingFang SC",
        "Hiragino Sans GB",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &["Microsoft YaHei UI", "Microsoft YaHei", "SimHei", "SimSun"];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[&str] = &[
        "Noto Sans CJK SC",
        "Noto Sans CJK",
        "Source Han Sans SC",
        "WenQuanYi Micro Hei",
        "Droid Sans Fallback",
    ];

    for name in candidates {
        let q = Query {
            families: &[Family::Name(name)],
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: Style::Normal,
        };
        if db.query(&q).is_some() {
            tracing::debug!(
                faces = face_count,
                font = name,
                "ui-font: using system CJK font"
            );
            return (*name).into();
        }
    }

    // No preferred family resolved. List what *is* available (if anything) so the
    // log shows whether enumeration is empty or just missing our candidates (#129).
    if face_count > 0 {
        let mut fams: Vec<String> = db
            .faces()
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .collect();
        fams.sort();
        fams.dedup();
        let sample: Vec<String> = fams.into_iter().take(40).collect();
        tracing::warn!(faces = face_count, available = ?sample,
            "ui-font: no preferred CJK font resolved; listing available families");
    }
    tracing::warn!(
        faces = face_count,
        "ui-font: falling back to embedded 'Meatshell Mono' (system fonts unusable, #129)"
    );
    "Meatshell Mono".into()
}

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
            return (
                "http".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    for p in ["socks5h://", "socks5://", "socks://"] {
        if lower.starts_with(p) {
            return (
                "socks5".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    ("socks5".to_string(), s.trim_end_matches('/').to_string())
}

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
        // forwarded; the #274/#312 fix filters only bare Ctrl/CtrlR markers.
        assert_eq!(key_to_pty_bytes("\u{0012}", true, false, false), vec![0x12]);
        // Ctrl+X as C0 0x18.
        assert_eq!(key_to_pty_bytes("\u{0018}", true, false, false), vec![0x18]);
    }

    #[test]
    fn platform_bare_ctrl_markers_do_not_reach_nano() {
        // Slint on Debian and macOS emits these before the actual Ctrl+letter event.
        assert!(should_drop_bare_ctrl_marker("\u{0011}", true, true));
        assert!(should_drop_bare_ctrl_marker("\u{0016}", true, true));
        // Other platforms retain their existing direct-C0 behaviour.
        assert!(!should_drop_bare_ctrl_marker(
            "\u{0011}",
            true,
            false
        ));
        assert!(!should_drop_bare_ctrl_marker("x", true, true));
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
    fn command_bar_preserves_multiline_heredoc() {
        let command = "cat <<'EOF'\nHEREDOC-1\n中文-HEREDOC-2\nEOF\n";
        let (history, bytes) = encode_command_bar_input(command).unwrap();
        assert_eq!(history, command.trim_end());
        assert_eq!(bytes, command.as_bytes());
        assert!(!history.lines().any(|line| line.starts_with(' ')));
    }

    #[test]
    fn paste_uses_remote_bracketed_paste_mode() {
        assert_eq!(
            encode_pasted_text("first\r\n  second", true),
            b"\x1b[200~first\r  second\x1b[201~"
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
    fn bash_readline_history_repaints_the_current_line() {
        let mut buffer = make_buf(4, 40, &[], &[], 0);
        buffer.ingest(b"\x1b[?2004hP> echo second");
        // GNU readline replaces "second" with the shorter "first" using six
        // backspaces, DCH for the leftover cell, then the replacement suffix.
        buffer.ingest(b"\x08\x08\x08\x08\x08\x08\x1b[1Pfirst");
        buffer.render();

        assert_eq!(buffer.displayed_text[0], "P> echo first");
        assert_eq!(buffer.parser.screen().cursor_position(), (0, 13));
    }

    #[test]
    fn plain_click_has_no_selection_extent() {
        let mut buffer = make_buf(2, 20, &[], &["one"], 0);
        buffer.sel_anchor = Some((0, 1));
        buffer.sel_focus = Some((0, 1));
        buffer.sel_ranges.push(((0, 1), (0, 1)));
        assert!(!buffer.selection_has_extent());

        buffer.sel_focus = Some((0, 2));
        buffer.sel_ranges[0].1 = (0, 2);
        assert!(buffer.selection_has_extent());
    }

    #[test]
    fn csi_3j_clears_meatshell_scrollback_even_when_split() {
        let mut buffer = make_buf(3, 20, &["old one", "old two"], &["current"], 2);
        buffer.raw.extend(b"old one\nold two\n");
        buffer.prev.push(hist_line("old two"));
        buffer.sel_anchor = Some((0, 0));
        buffer.sel_focus = Some((1, 2));

        buffer.ingest(b"\x1b[3");
        assert_eq!(buffer.history.len(), 2);
        buffer.ingest(b"J");

        assert!(buffer.history.is_empty());
        assert_eq!(buffer.view_offset, 0);
        assert!(buffer.raw.is_empty());
        assert!(buffer.sel_anchor.is_none());
        assert!(buffer.sel_focus.is_none());
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
        buf.history = VecDeque::from([
            wrapped_hist_line("0123456789"),
            wrapped_hist_line("abcdefghij"),
            hist_line("klmnop"),
            hist_line("next"),
        ]);
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
