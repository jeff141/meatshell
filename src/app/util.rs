//! General utility helpers used by the app module.

use crate::config::{AuthMethod, Secret, Session};
use slint::SharedString;

pub(crate) fn tab_title_len(title: &str) -> i32 {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1usize } else { 2usize })
        .sum::<usize>()
        .min(i32::MAX as usize) as i32
}

pub(crate) fn should_block_close(exit_confirmed: bool, has_live_sessions: bool) -> bool {
    !exit_confirmed && has_live_sessions
}

/// Parse the batch-import textarea (#150). Each non-empty, non-`#` line is
/// `host|port|user|password|name`; trailing fields are optional (port → 22,
/// user → root, password → none, name → user@host). A leading header row such as
/// `host|port|username|password|name` is skipped. Dedup happens at the call site.
pub(crate) fn parse_batch_import(text: &str) -> Vec<Session> {
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

/// Any printable character could be a password character, so we never emit it.
/// Only C0/C1 control code points (Backspace, Esc, the IME-injected 0x10/0x15
/// markers, …) are revealed — those are exactly what the Shift/Backspace IME
/// diagnostics need and are never password material. Printable characters are
/// collapsed to a count, so the logs stay useful without exposing keystrokes.
pub(crate) fn redact_key(key: &str) -> String {
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
pub(crate) fn line_numbers_for(content: &str) -> String {
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
pub(crate) fn clipboard_set_text(text: String) {
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
pub(crate) fn resolve_ui_font_family() -> SharedString {
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

pub(crate) fn system_monospace_fonts() -> Vec<SharedString> {
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
    let mut out = vec![SharedString::from("Meatshell Mono")];
    out.extend(names.into_iter().map(SharedString::from));
    out
}

/// Parse a "vX.Y.Z" / "X.Y.Z" tag into a comparable tuple, or None if it isn't
/// a three-part numeric version. A pre-release suffix on the patch (e.g.
/// "3-rc1") is tolerated by taking its leading digits (#48).
pub(crate) fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
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

/// Split a stored proxy URL into `(type, host:port)` for the session dialog.
///
/// `""` → `("none", "")`. Recognises `socks5`/`socks5h`/`socks` and
/// `http`/`https` scheme prefixes. A value without a (recognised) scheme is
/// treated as SOCKS5, matching proxy.rs's parse default, so older configs that
/// stored a bare `host:port` keep working.
pub(crate) fn split_proxy(url: &str) -> (String, String) {
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

/// Normalise pasted text's line endings to a single CR (0x0d) — what a terminal
/// expects for Enter.
///
/// The clipboard may hold CRLF (Windows) or LF line breaks. Sending those to the
/// PTY verbatim makes the remote shell see *two* line breaks per line (CR then
/// LF), which prematurely ends a `\`-continued line: pasting
/// `sudo apt install \<newline>  docker-ce` would run `sudo apt install` with no
/// package and drop the rest. Collapsing every CRLF/LF to one CR fixes it.
pub(crate) fn normalize_pasted_newlines(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

/// Encode clipboard text according to the mode requested by the remote
/// application. Bracketed paste lets shells and editors distinguish pasted
/// text from typed keystrokes, preserving multi-line layout and indentation.
pub(crate) fn encode_pasted_text(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return normalize_pasted_newlines(text).into_bytes();
    }

    // A pasted ESC could forge the end marker; Ctrl+C also terminates bracketed
    // paste in some shells. Match established terminal-emulator behaviour by
    // filtering both before wrapping the payload.
    let filtered = text.replace(['\x1b', '\x03'], "");
    let mut bytes = Vec::with_capacity(filtered.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(filtered.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Return the parent directory of `path`.
/// "/a/b/c" → "/a/b", "/a" → "/", "/" → "/"
pub(crate) fn parent_path(path: &str) -> String {
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
