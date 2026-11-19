#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{io::Write as _, process::ExitCode};
use zevria_desktop::cli::{Cli, USAGE};

#[cfg(windows)]
mod windows_startup;

fn main() -> ExitCode {
    match parse_startup() {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(cli)) => match launch(cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                #[cfg(windows)]
                windows_startup::report_launch_error(&error);
                #[cfg(not(windows))]
                eprintln!("Error: {error:?}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            #[cfg(windows)]
            windows_startup::ensure_stderr();
            eprintln!("Error: {error:?}");
            ExitCode::FAILURE
        }
    }
}

// Noninteractive invocations must not initialize GPUI or read provider config.
fn parse_startup() -> anyhow::Result<Option<Cli>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--__print-env") {
        anyhow::ensure!(
            args.len() == 2,
            "private environment exporter expects one marker"
        );
        zevria_foundation::shell::print_environment(&args[1])?;
        return Ok(None);
    }
    if args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--help" || arg == "-h")
    {
        #[cfg(windows)]
        windows_startup::ensure_stdout();
        writeln!(std::io::stdout().lock(), "{USAGE}")?;
        return Ok(None);
    }
    Cli::parse(args).map(Some)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn launch(cli: Cli) -> anyhow::Result<()> {
    zevria_desktop::launch::run(cli)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launch(_: Cli) -> anyhow::Result<()> {
    anyhow::bail!(
        "Zevria Desktop supports macOS and x64 Windows; use the zevria CLI on this platform"
    )
}
