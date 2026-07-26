//! Keyboard / paste input translation helpers.

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use std::collections::VecDeque;
use crate::session::ConnectCtx;
use crate::ssh::SessionCommand;
use crate::terminal::TermBuffers;

use super::*;

pub(crate) fn terminal_uses_bracketed_paste(bufs: &TermBuffers, tab_id: &str) -> bool {
    let buffer = bufs
        .lock()
        .ok()
        .and_then(|buffers| buffers.get(tab_id).cloned());
    buffer
        .and_then(|buffer| {
            buffer
                .lock()
                .ok()
                .map(|buffer| buffer.parser.screen().bracketed_paste())
        })
        .unwrap_or(false)
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CtrlKeySide {
    Left,
    Right,
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_process_ctrl_release(
    state: i_slint_backend_winit::winit::event::ElementState,
    logical_key: &i_slint_backend_winit::winit::keyboard::Key,
    physical_key: &i_slint_backend_winit::winit::keyboard::PhysicalKey,
) -> Option<CtrlKeySide> {
    use i_slint_backend_winit::winit::event::ElementState;
    use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    if state != ElementState::Released || !matches!(logical_key, Key::Named(NamedKey::Process)) {
        return None;
    }

    match physical_key {
        PhysicalKey::Code(KeyCode::ControlLeft) => Some(CtrlKeySide::Left),
        PhysicalKey::Code(KeyCode::ControlRight) => Some(CtrlKeySide::Right),
        _ => None,
    }
}

pub(crate) fn should_drop_debian_bare_ctrl_marker(key: &str, ctrl: bool, workaround: bool) -> bool {
    workaround
        && ctrl
        && matches!(key.chars().collect::<Vec<_>>().as_slice(), ['\u{0011}'] | ['\u{0016}'])
}

#[cfg(target_os = "linux")]
pub(crate) fn debian_ctrl_marker_workaround_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let Ok(release) = std::fs::read_to_string("/etc/os-release") else {
            return false;
        };
        release.lines().any(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return false;
            };
            let value = value.trim_matches('"');
            key == "ID" && value.eq_ignore_ascii_case("debian")
                || key == "ID_LIKE"
                    && value
                        .split_ascii_whitespace()
                        .any(|item| item.eq_ignore_ascii_case("debian"))
        })
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn debian_ctrl_marker_workaround_enabled() -> bool {
    false
}

pub(crate) fn key_to_pty_bytes(key: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    // --- Special keys (Slint PUA code points) ------------------------------
    // Arrow keys: respect DECCKM application-cursor mode.
    let special: Option<&[u8]> = match key {
        "\u{F700}" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }), // Up
        "\u{F701}" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }), // Down
        "\u{F702}" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }), // Left
        "\u{F703}" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }), // Right
        "\u{F729}" => Some(b"\x1b[H"),                                      // Home
        "\u{F72B}" => Some(b"\x1b[F"),                                      // End
        "\u{F72C}" => Some(b"\x1b[5~"),                                     // PageUp
        "\u{F72D}" => Some(b"\x1b[6~"),                                     // PageDown
        // Forward-Delete. Slint's canonical key code for the Delete key is
        // U+007F (see i-slint-common key_codes: F728 is explicitly *not* used,
        // it collapses to the 0x7f control code). The old F728 mapping never
        // matched on any platform, so Delete fell through to the generic path
        // and behaved like backspace / garbled the char instead of sending the
        // VT "delete forward" sequence (B站 fan report).
        "\u{007F}" | "\u{F728}" => Some(b"\x1b[3~"), // Delete (forward)
        "\u{F704}" => Some(b"\x1bOP"),               // F1
        "\u{F705}" => Some(b"\x1bOQ"),               // F2
        "\u{F706}" => Some(b"\x1bOR"),               // F3
        "\u{F707}" => Some(b"\x1bOS"),               // F4
        "\u{F708}" => Some(b"\x1b[15~"),             // F5
        "\u{F709}" => Some(b"\x1b[17~"),             // F6
        "\u{F70A}" => Some(b"\x1b[18~"),             // F7
        "\u{F70B}" => Some(b"\x1b[19~"),             // F8
        "\u{F70C}" => Some(b"\x1b[20~"),             // F9
        "\u{F70D}" => Some(b"\x1b[21~"),             // F10
        "\u{F70E}" => Some(b"\x1b[23~"),             // F11
        "\u{F70F}" => Some(b"\x1b[24~"),             // F12
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
    // Keep ctrl=true C0 values here: some Linux/macOS builds encode real
    // Ctrl+P..Ctrl+X directly as 0x10..=0x18. Debian's bare Ctrl markers are
    // filtered at the event boundary, where the distro-specific workaround is
    // available (#274).
    if let Some(c) = key.chars().next() {
        let cp = c as u32;
        if key.chars().count() == 1 {
            if !ctrl && (0x10..=0x18).contains(&cp) {
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
                    b'A'..=b'Z' => Some(upper - b'A' + 1), // Ctrl+A=\x01 … Ctrl+Z=\x1A
                    b'[' => Some(0x1b),                    // Ctrl+[ = ESC
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
pub(crate) fn is_vk_back_down() -> bool {
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
pub(crate) fn c0_letter_key_down(cp: u32) -> bool {
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

// ---------------------------------------------------------------------------
// Raw keystroke forwarding and PTY resize
// ---------------------------------------------------------------------------

pub(crate) fn wire_key_input(window: &AppWindow, ctx: Rc<AppContext>) {
    let handles = ctx.handles.clone();
    let bufs = ctx.bufs.clone();
    let last_term_size = ctx.last_term_size.clone();
    let store = ctx.store.clone();
    let ctx = ConnectCtx {
        weak: ctx.main_window.clone(),
        runtime: ctx.runtime.clone(),
        handles: ctx.handles.clone(),
        sftp_handles: ctx.sftp_handles.clone(),
        sftp_last_cwd: ctx.sftp_last_cwd.clone(),
        bufs: ctx.bufs.clone(),
        render_gates: ctx.render_gates.clone(),
        tab_statuses: ctx.tab_statuses.clone(),
        local_snap: ctx.local_snap.clone(),
        local_net_hist: ctx.local_net_hist.clone(),
        last_term_size: ctx.last_term_size.clone(),
        sftp_follow_cd: ctx.sftp_follow_cd.clone(),
        store: ctx.store.clone(),
    };
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

            if should_drop_debian_bare_ctrl_marker(
                key.as_str(),
                ctrl,
                debian_ctrl_marker_workaround_enabled(),
            ) {
                tracing::debug!(
                    "send_key: dropped Debian/Slint bare Ctrl modifier marker {}",
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
                rebuild_tab_display(&win, &bufs_sel, &tid);
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
            let text = with_term_buf(&bufs_sel, &tid, |buf| {
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
