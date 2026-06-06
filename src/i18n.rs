//! Tiny runtime internationalisation.
//!
//! Two cooperating mechanisms keep the whole UI translatable:
//!
//! * **Static `.slint` text** uses Slint's own `@tr("English")` plus bundled
//!   `.po` translations.  The source language (the msgids) is **English**; the
//!   Chinese strings live in `lang/zh/LC_MESSAGES/meatshell.po`.  Switching is
//!   done with `slint::select_bundled_translation` (`"zh"` → Chinese, `""`/`"en"`
//!   → the English source).
//!
//! * **Dynamic Rust text** (status lines, errors, transfer details that Rust
//!   builds with `format!`) can't use `@tr`, so it uses [`t`] which returns the
//!   Chinese or English variant based on the current language flag.
//!
//! [`set_language`] updates both at once so the two stay in sync.

use std::sync::atomic::{AtomicU8, Ordering};

const ZH: u8 = 0;
const EN: u8 = 1;

static LANG: AtomicU8 = AtomicU8::new(ZH);

/// Apply a language code (`"zh"` or `"en"`).  Updates the Rust-side flag and
/// Slint's bundled-translation selection.  Safe to call before the first
/// component exists for the flag; the Slint selection is a no-op error then and
/// should be re-applied once the window is created.
pub fn set_language(code: &str) {
    let en = code.eq_ignore_ascii_case("en");
    LANG.store(if en { EN } else { ZH }, Ordering::Relaxed);
    apply_to_slint();
}

/// Re-apply the current language to Slint's bundled translations.  Must run
/// after the first component is created (Slint requirement).  We bundle BOTH an
/// `en` (identity) and a `zh` translation and select explicitly, because the
/// empty/`"en"` shortcut selects bundle index 0 — which would be `zh` when only
/// the Chinese bundle exists.
pub fn apply_to_slint() {
    let lang = if is_en() { "en" } else { "zh" };
    let _ = slint::select_bundled_translation(lang);
}

/// Current language code, for persisting to config (`"zh"` / `"en"`).
pub fn current_code() -> &'static str {
    if is_en() {
        "en"
    } else {
        "zh"
    }
}

pub fn is_en() -> bool {
    LANG.load(Ordering::Relaxed) == EN
}

/// Pick the variant for the current language: `zh` is Chinese, `en` is English.
pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() {
        en
    } else {
        zh
    }
}
