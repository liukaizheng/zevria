//! Local command-line startup, before initializing the application.

use std::path::Path;

use crate::{skills_cli::SkillsCli, theme_cli::ThemeCli};

const USAGE: &str = "usage: zevria [--continue | -c]
       zevria --acp [--ensemble-worker]
       zevria skills <command>
       zevria theme <command>
       zevria clean";

#[derive(Debug, Default, PartialEq)]
pub(super) struct CliArgs {
    /// Resume the workspace's most recent session instead of starting fresh.
    pub(super) continue_session: bool,
    /// Serve ACP V1 over newline-delimited stdio without initializing a terminal.
    pub(super) acp: bool,
    /// Isolated read-only ACP analysis worker, never an interactive root.
    pub(super) ensemble_worker: bool,
    /// Purge the workspace's selected storage directories offline, without a prompt.
    pub(super) clean: bool,
    pub(super) skills: Option<SkillsCli>,
    pub(super) theme: Option<ThemeCli>,
}

pub(super) enum Startup {
    Run(CliArgs),
    Exit(i32),
}

pub(super) async fn startup(args: Vec<String>) -> anyhow::Result<Startup> {
    match args.first().map(String::as_str) {
        Some("--__print-env") => {
            anyhow::ensure!(
                args.len() == 2,
                "private environment exporter expects one marker"
            );
            zevria_foundation::shell::print_environment(std::ffi::OsStr::new(&args[1]))?;
        }
        Some("--__acp-job") => return Ok(Startup::Exit(acp_job(&args[1..]).await?)),
        Some("--help" | "-h") if args.len() == 1 => {
            println!(
                "{USAGE}\n\nOn Windows, command tools use Git for Windows Bash.\n--help, --version do not load configuration."
            );
        }
        Some("--version" | "-V") if args.len() == 1 => {
            println!("zevria {}", env!("CARGO_PKG_VERSION"));
        }
        _ => return Ok(Startup::Run(parse_args(&args)?)),
    }
    Ok(Startup::Exit(0))
}

fn parse_args(args: &[String]) -> anyhow::Result<CliArgs> {
    match args.first().map(String::as_str) {
        Some("clean") => {
            anyhow::ensure!(
                args.len() == 1,
                "clean accepts no arguments and cannot be combined with other startup modes; {USAGE}"
            );
            return Ok(CliArgs {
                clean: true,
                ..CliArgs::default()
            });
        }
        Some("skills") => {
            return Ok(CliArgs {
                skills: Some(SkillsCli::parse(&args[1..])?),
                ..CliArgs::default()
            });
        }
        Some("theme") => {
            return Ok(CliArgs {
                theme: Some(ThemeCli::parse(&args[1..])?),
                ..CliArgs::default()
            });
        }
        _ => {}
    }
    let mut parsed = CliArgs::default();
    for arg in args {
        match arg.as_str() {
            "--continue" | "-c" => parsed.continue_session = true,
            "--acp" => parsed.acp = true,
            "--ensemble-worker" => parsed.ensemble_worker = true,
            _ => anyhow::bail!("unknown argument {arg:?}; {USAGE}"),
        }
    }
    if parsed.ensemble_worker && !parsed.acp {
        anyhow::bail!("--ensemble-worker requires --acp");
    }
    if parsed.acp && parsed.continue_session {
        anyhow::bail!("--acp cannot be combined with --continue or -c");
    }
    Ok(parsed)
}

pub(super) fn diagnostics(workspace: &Path) -> anyhow::Result<()> {
    let config = workspace.join(zevria_foundation::config::config_path()?);
    let state = zevria_foundation::runtime_paths::workspace_state_root(workspace);
    eprintln!(
        "zevria: runtime {}; configuration {}; state {}",
        std::env::consts::OS,
        config.display(),
        state.display()
    );
    Ok(())
}

async fn acp_job(args: &[String]) -> anyhow::Result<i32> {
    #[cfg(not(windows))]
    {
        let _ = args;
        anyhow::bail!("the ACP job helper is Windows-only");
    }
    #[cfg(windows)]
    {
        use std::process::Stdio;

        anyhow::ensure!(!args.is_empty(), "ACP job helper requires a command");
        let shell = zevria_foundation::shell::frozen()?;
        // Positional argv supports npm/npx shell scripts without string rewriting.
        let mut command = shell.command(r#"program=$1; shift; case "$program" in [A-Za-z]:*|\\\\*) program=$(cygpath -u -- "$program") || exit;; esac; "$program" "$@""#);
        command
            .arg("zevria-acp-job")
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let (mut child, job) = zevria_foundation::process::ProcessTree::spawn(&mut command)?;
        let status = child.wait().await?;
        job.kill();
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<CliArgs> {
        parse_args(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parse_args_accepts_the_continue_flags() {
        assert_eq!(parse(&[]).unwrap(), CliArgs::default());
        for flag in ["--continue", "-c"] {
            let args = parse(&[flag]).unwrap_or_else(|_| panic!("{flag} should parse"));
            assert!(args.continue_session);
            assert!(!args.acp);
        }
    }

    #[test]
    fn parse_args_accepts_only_bare_clean() {
        assert_eq!(
            parse(&["clean"]).unwrap(),
            CliArgs {
                clean: true,
                ..CliArgs::default()
            }
        );
        for other in [
            "extra",
            "clean",
            "skills",
            "--continue",
            "-c",
            "--acp",
            "--ensemble-worker",
        ] {
            for args in [["clean", other], [other, "clean"]] {
                let error = parse(&args).unwrap_err();
                assert!(error.to_string().contains("usage"), "{args:?}: {error}");
            }
        }
        for args in [
            ["clean", "skills", "list"],
            ["skills", "list", "clean"],
            ["clean", "--acp", "--ensemble-worker"],
            ["--acp", "clean", "--ensemble-worker"],
            ["--acp", "--ensemble-worker", "clean"],
        ] {
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn parse_args_accepts_acp_and_rejects_continue_combination() {
        let args = parse(&["--acp"]).expect("--acp should parse");
        assert!(args.acp);
        assert!(!args.continue_session);

        for continue_flag in ["--continue", "-c"] {
            let error = parse(&["--acp", continue_flag])
                .expect_err("ACP and continue must be mutually exclusive");
            assert!(error.to_string().contains("cannot be combined"));
        }
    }

    #[test]
    fn worker_flag_requires_acp_and_rejects_other_startup_modes() {
        for flags in [
            ["--acp", "--ensemble-worker"],
            ["--ensemble-worker", "--acp"],
        ] {
            let parsed = parse(&flags).unwrap();
            assert!(parsed.acp && parsed.ensemble_worker);
        }
        for flags in [
            &["--ensemble-worker"][..],
            &["--ensemble-worker", "--continue"],
            &["--acp", "--ensemble-worker", "-c"],
            &["skills", "--ensemble-worker"],
            &["sessions", "--ensemble-worker"],
        ] {
            assert!(parse(flags).is_err());
        }
    }

    #[test]
    fn removed_session_recovery_command_is_rejected() {
        for args in [
            &["sessions"][..],
            &[
                "sessions",
                "recover-models",
                "root",
                "--build",
                "test",
                "model",
                "--plan",
                "test",
                "model",
            ],
        ] {
            let error = parse(args).unwrap_err();
            assert!(error.to_string().contains("unknown"), "{error}");
        }
    }

    #[test]
    fn parse_args_rejects_unknown_arguments() {
        let error = parse(&["--bogus"]).expect_err("unknown arguments should fail");
        let message = error.to_string();
        assert!(message.contains("--bogus"));
        assert!(message.contains("usage"));
        for mode in [
            "--continue",
            "-c",
            "--acp",
            "--ensemble-worker",
            "skills",
            "clean",
        ] {
            assert!(message.contains(mode), "{message}");
        }
    }
}
