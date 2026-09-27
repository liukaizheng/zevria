//! File-based `tracing` setup.
//!
//! The app is a TUI, so log output must never reach stdout/stderr while the
//! terminal is in raw mode — everything goes to `~/.zevria/logs/zevria.log`
//! (or the directory configured under `[log]`) through a non-blocking writer.

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::config::LogConfig;

/// Install the global tracing subscriber writing to the log file.
///
/// The returned [`WorkerGuard`] flushes buffered log lines when dropped, so
/// `main` must keep it alive for the whole run (bind it to a named variable —
/// `let _ = …` would drop it immediately).
pub fn init(config: &LogConfig) -> anyhow::Result<WorkerGuard> {
    let directory = match &config.directory {
        Some(directory) => directory.clone(),
        None => crate::config::zevria_dir()?.join("logs"),
    };
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create the log directory at {}",
            directory.display()
        )
    })?;

    let appender = tracing_appender::rolling::never(&directory, "zevria.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    // RUST_LOG wins over the configured level when set.
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::try_new(&config.level)
            .with_context(|| format!("invalid log level {:?} in the config file", config.level))?,
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();

    // Log panics before ratatui's hook (installed later) restores the terminal
    // and the default hook prints; the panic reaches the log file even if the
    // terminal output is lost.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "zevria_core::logging", "panic: {info}");
        previous_hook(info);
    }));

    Ok(guard)
}
