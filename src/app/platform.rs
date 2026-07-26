//! Platform-specific window and backend helpers.

use crate::ui::AppWindow;
use i_slint_backend_winit::WinitWindowAccessor;
use slint::ComponentHandle;

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
pub(crate) fn set_window_icon(window: &AppWindow) {
    use i_slint_backend_winit::winit::window::Icon;
    const ICON_PNG: &[u8] = include_bytes!("../../assets/icon@512.png");
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
pub(crate) fn apply_window_chrome(window: &slint::Window) {
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
pub(crate) fn apply_window_chrome(_window: &slint::Window) {}

#[cfg(windows)]
pub(crate) fn setup_windows_platform(renderer_mode: &str) {
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

pub(crate) fn clamp_window_size_to_monitor(
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
pub(crate) fn is_wayland_window(window: &slint::Window) -> bool {
    use i_slint_backend_winit::winit::platform::wayland::WindowExtWayland;

    window
        .with_winit_window(|ww| ww.xdg_toplevel().is_some())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn is_wayland_window(_window: &slint::Window) -> bool {
    false
}

/// Detect the Windows mixed-DPI failure where the native maximized flag stays
/// set but the HWND keeps a much smaller geometry from the previous monitor.
/// Normal maximized work areas may be a little smaller because of the taskbar;
/// only a large mismatch is considered stale.
pub(crate) fn maximized_geometry_needs_repair(
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
pub(crate) fn refresh_revealed_main_window(weak: slint::Weak<AppWindow>) {
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
pub(crate) fn schedule_slint_pointer_ungrab<T>(weak: slint::Weak<T>)
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
pub(crate) fn schedule_slint_pointer_ungrab<T>(_weak: slint::Weak<T>)
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
pub(crate) fn setup_macos_platform() {
    use i_slint_backend_winit::winit::platform::macos::WindowAttributesExtMacOS;

    let mut builder = i_slint_backend_winit::Backend::builder();
    // Preserve the SLINT_BACKEND escape hatch: e.g. "winit-skia" → renderer "skia".
    if let Ok(v) = std::env::var("SLINT_BACKEND") {
        if let Some(r) = v.strip_prefix("winit-").filter(|r| !r.is_empty()) {
            builder = builder.with_renderer_name(r.to_string());
        }
    }
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
