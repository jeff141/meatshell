// Entry point. Wires the Slint UI to the config store, system sampler and
// SSH session manager.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod i18n;
mod proxy;
mod serial;
mod sftp;
mod ssh;
mod ssh_config;
mod system;
mod telnet;

fn main() -> anyhow::Result<()> {
    // Initialise tracing — honour RUST_LOG but default to info.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Apply the UI style from config before creating any Slint windows.
    // Honours SLINT_STYLE if already set in the environment; otherwise reads
    // the user's saved preference (default: "fluent").
    if std::env::var("SLINT_STYLE").is_err() {
        if let Ok(store) = config::ConfigStore::load() {
            let style = store.style().to_string();
            if style != "fluent" {
                tracing::info!("applying UI style: {}", style);
            }
            std::env::set_var("SLINT_STYLE", &style);
        }
    }

    // ── IME policy ───────────────────────────────────────────────────────────
    // NOTE: We deliberately DO **NOT** call `ImmDisableIME` here.
    //
    // An earlier version disabled the IME for the whole Slint event-loop thread
    // to work around a vim `:q!` glitch (Chinese IMEs intercept letter keys and,
    // on a Shift press, discard the in-flight pinyin).  But disabling the IME
    // also makes 中文输入 completely impossible — there is no composition window
    // at all, which is exactly the "无法输入任何中文" bug.
    //
    // Chinese input now flows through the hidden `ime-input` TextInput in
    // terminal_view.slint: composition happens there, and committed text is
    // forwarded to the PTY via the `edited` callback.  The vim/Shift side-effects
    // are handled instead by the C0-marker + 3-layer Backspace filters in
    // `app::on_send_key`, so we no longer need (and must not use) ImmDisableIME.

    app::run()
}
