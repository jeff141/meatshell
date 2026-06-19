// Entry point. Wires the Slint UI to the config store, system sampler and
// SSH session manager.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod errlog;
mod forward;
mod i18n;
mod known_hosts;
mod proxy;
mod serial;
mod sftp;
mod ssh;
mod ssh_config;
mod system;
mod telnet;
mod zmodem;

fn main() -> anyhow::Result<()> {
    // Install the `log` filter BEFORE tracing init: Slint's diagnostics (now
    // routed via the `log` feature) flow here, where we drop the harmless
    // ICU4X segmentation-model spam and forward everything else to stderr.
    init_log_filter();
    init_tracing();

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

/// Set up tracing: stderr (honours RUST_LOG, default info) **plus** a capped
/// `error.log` file at WARN and above so users can send diagnostics — e.g. a
/// bastion disconnect reason — without setting RUST_LOG (#86).
fn init_tracing() {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(env_filter);

    // One file, capped at 5 MiB, auto-overwriting when full.
    let file_layer = errlog::path()
        .and_then(|p| errlog::CappedFile::open(p, 5 * 1024 * 1024).ok())
        .map(|cf| {
            fmt::layer()
                .with_ansi(false)
                .with_writer(errlog::CappedWriter::new(cf))
                .with_filter(LevelFilter::WARN)
        });

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();
}

/// Install a `log` crate logger that suppresses the harmless ICU4X
/// line-break fallback warning which otherwise floods the console whenever the
/// terminal renders CJK text.
///
/// Slint 1.16 bundles ICU4X data that ships no `ja` segmentation dictionary
/// model, so `icu_segmenter` logs "No segmentation model for language: ja" per
/// shaped run and falls back to generic line-breaking rules — text still
/// renders correctly, it's just noisy (slint-ui/slint#11604). Other `log`
/// records (warn/error) are forwarded to stderr so real Slint/ICU4X issues stay
/// visible. Must be called before Slint starts emitting diagnostics.
fn init_log_filter() {
    use log::{Level, Log, Metadata, Record};

    struct FilteredLogger;

    impl Log for FilteredLogger {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= Level::Warn
        }

        fn log(&self, record: &Record) {
            // Drop the specific ICU4X segmentation-model fallback warning.
            let args = record.args();
            if args.to_string().contains("No segmentation model") {
                return;
            }
            eprintln!("{} {}: {}", record.level(), record.target(), args);
        }

        fn flush(&self) {}
    }

    static LOGGER: FilteredLogger = FilteredLogger;
    // Ignore the error if a logger was already installed elsewhere.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
}
