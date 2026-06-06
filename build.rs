fn main() {
    // Bundle the gettext `.po` translations under `lang/` so the UI's `@tr(...)`
    // strings can switch language at runtime via slint::select_bundled_translation.
    // Source language is Chinese (the msgids); `lang/<lc>/LC_MESSAGES/meatshell.po`
    // provides other locales.  No per-component context, so msgids are the raw
    // Chinese strings.
    println!("cargo:rerun-if-changed=lang");

    // Embed UI style.  Default is "native" which adapts to the host OS look.
    // Override at build time via MEATSHELL_STYLE env var (e.g. "fluent").
    // At runtime the SLINT_STYLE env var or the user's config file selects
    // which style to use; only the compiled-in style is available.
    let style = std::env::var("MEATSHELL_STYLE").unwrap_or_else(|_| "native".into());

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style(style.into())
            .with_bundled_translations("lang")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("Slint build failed");

    // Embed the application icon into the Windows executable so it shows up in
    // Explorer, the taskbar and shortcuts. No-op on non-Windows targets.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/meatshell.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/meatshell.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
