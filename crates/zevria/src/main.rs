//! The Zevria composition root: CLI/configuration, compile-time adapters, and
//! the terminal runtime.

use zevria_app::{acp_host, clean, config, runtime, skills, theme_store};
#[cfg(test)]
use zevria_ensemble as ensemble;
mod launcher;
#[cfg(test)]
mod session_model_tests;
mod skills_cli;
mod theme_cli;

use std::io;
use std::path::Path;
use std::sync::Arc;
use zevria_transcript::subtask_launch_metadata;

use anyhow::Context as _;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use zevria_foundation::SubtaskId;
use zevria_foundation::logging;
use zevria_transcript::AgentRunTranscriptReader;
use zevria_transcript::agent_run_path;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_tui::{App, SessionViews, UiContext, UiOutcome, run_ui};
use zevria_tui_widgets::render_startup_frame;

use crate::config::Config;

/// Command-line options stay hand-rolled because the public surface is small.
#[derive(Debug, Default, PartialEq)]
struct CliArgs {
    /// Resume the workspace's most recent session instead of starting fresh.
    continue_session: bool,
    /// Serve ACP V1 over newline-delimited stdio without initializing a terminal.
    acp: bool,
    /// Isolated read-only ACP analysis worker, never an interactive root.
    ensemble_worker: bool,
    /// Purge the workspace's selected storage directories offline, without a prompt.
    clean: bool,
    skills: Option<skills_cli::SkillsCli>,
    theme: Option<theme_cli::ThemeCli>,
}

/// Scoped Kitty keyboard-protocol activation.
///
/// A legacy terminal reports Ctrl+Enter as the same carriage return as plain
/// Enter. Progressive keyboard enhancement makes the modifier unambiguous on
/// terminals such as Ghostty, Kitty, WezTerm, and recent Alacritty. Unknown
/// CSI commands are ignored by terminals without support, so failure to push
/// the mode is non-fatal and preserves their existing input behavior.
struct KeyboardEnhancementGuard {
    pushed: bool,
}

impl KeyboardEnhancementGuard {
    fn push() -> Self {
        let result = crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        match result {
            Ok(()) => Self { pushed: true },
            Err(error) => {
                tracing::warn!(%error, "failed to enable enhanced keyboard reporting");
                Self { pushed: false }
            }
        }
    }

    fn pop(&mut self) {
        if !std::mem::take(&mut self.pushed) {
            return;
        }
        if let Err(error) = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags) {
            tracing::warn!(%error, "failed to restore keyboard reporting mode");
        }
    }
}

impl Drop for KeyboardEnhancementGuard {
    fn drop(&mut self) {
        self.pop();
    }
}

/// Scoped bracketed-paste activation. Supporting terminals emit an atomic
/// [`crossterm::event::Event::Paste`] payload instead of character and Enter
/// key events; unsupported terminals and setup failures retain the multiline
/// Enter fallback.
struct BracketedPasteGuard {
    enabled: bool,
}

impl BracketedPasteGuard {
    fn enable() -> Self {
        match crossterm::execute!(io::stdout(), EnableBracketedPaste) {
            Ok(()) => Self { enabled: true },
            Err(error) => {
                tracing::warn!(%error, "failed to enable bracketed paste");
                Self { enabled: false }
            }
        }
    }

    fn disable(&mut self) {
        if !std::mem::take(&mut self.enabled) {
            return;
        }
        if let Err(error) = crossterm::execute!(io::stdout(), DisableBracketedPaste) {
            tracing::warn!(%error, "failed to restore bracketed paste mode");
        }
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        self.disable();
    }
}

/// Terminal command for DEC alternate-scroll mode. While the alternate screen
/// is active and mouse tracking is not captured, supporting terminals translate
/// wheel motion into ordinary Up/Down key events; unsupported terminals keep
/// the existing keyboard fallback.
struct EnableAlternateScroll;
struct DisableAlternateScroll;

impl crossterm::Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

impl crossterm::Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Scoped DEC alternate-scroll activation for the complete terminal lifetime.
struct AlternateScrollGuard {
    enabled: bool,
}

impl AlternateScrollGuard {
    fn enable() -> Self {
        match crossterm::execute!(io::stdout(), EnableAlternateScroll) {
            Ok(()) => Self { enabled: true },
            Err(error) => {
                tracing::warn!(%error, "failed to enable alternate scroll");
                Self { enabled: false }
            }
        }
    }

    fn disable(&mut self) {
        if !std::mem::take(&mut self.enabled) {
            return;
        }
        if let Err(error) = crossterm::execute!(io::stdout(), DisableAlternateScroll) {
            tracing::warn!(%error, "failed to restore alternate scroll mode");
        }
    }
}

impl Drop for AlternateScrollGuard {
    fn drop(&mut self) {
        self.disable();
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> anyhow::Result<CliArgs> {
    let usage = "usage: zevria [--continue | -c] | zevria --acp [--ensemble-worker] | zevria skills <command> | zevria theme <command> | zevria clean";
    let args: Vec<_> = args.collect();
    if args.first().is_some_and(|arg| arg == "clean") {
        anyhow::ensure!(
            args.len() == 1,
            "clean accepts no arguments and cannot be combined with other startup modes; {usage}"
        );
        return Ok(CliArgs {
            clean: true,
            ..CliArgs::default()
        });
    }
    if args.first().is_some_and(|arg| arg == "skills") {
        return Ok(CliArgs {
            skills: Some(skills_cli::SkillsCli::parse(&args[1..])?),
            ..CliArgs::default()
        });
    }
    if args.first().is_some_and(|arg| arg == "theme") {
        return Ok(CliArgs {
            theme: Some(theme_cli::ThemeCli::parse(&args[1..])?),
            ..CliArgs::default()
        });
    }
    let mut parsed = CliArgs::default();
    for arg in args {
        match arg.as_str() {
            "--continue" | "-c" => parsed.continue_session = true,
            "--acp" => parsed.acp = true,
            "--ensemble-worker" => parsed.ensemble_worker = true,
            _ => anyhow::bail!("unknown argument {arg:?}; {usage}"),
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

#[cfg(test)]
#[path = "sessions_cli_tests.rs"]
mod sessions_cli_tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = match launcher::dispatch(std::env::args().skip(1).collect()).await? {
        launcher::Launch::Local(args) => parse_args(args.into_iter())?,
        launcher::Launch::Exit(code) => std::process::exit(code),
    };
    if let Some(command) = args.theme {
        return command.run();
    }
    let workspace =
        std::env::current_dir().context("failed to determine the workspace directory")?;
    if args.clean {
        return clean::run(&workspace);
    }
    if let Some(command) = args.skills {
        return command.run(&workspace);
    }
    launcher::diagnostics(&workspace)?;
    zevria_foundation::shell::prepare_native().await?;
    let app_config = Config::load()?;
    let _log_guard = logging::init(app_config.log())?;
    tracing::info!(
        provider_catalog_entries = app_config.providers().len(),
        "starting zevria"
    );
    for (role, profile) in app_config.routing().assignments() {
        let assignment = app_config.modes().for_role(role);
        debug_assert_eq!(assignment.profile_ref(), profile.profile);
        tracing::info!(
            mode = role.name(),
            provider = %assignment.provider,
            model = %assignment.model,
            base_url = %profile.endpoint.redacted_base_url(),
            "configured model assignment"
        );
    }

    if args.acp {
        let acp_config = app_config.acp();
        let profile = if args.ensemble_worker {
            zevria_acp::ExecutionProfile::EnsembleWorker
        } else {
            zevria_acp::ExecutionProfile::Interactive
        };
        let factory = Arc::new(acp_host::AcpHostFactory::with_profile(
            Arc::new(app_config),
            profile,
        ));
        return zevria_acp::serve_stdio(acp_config, workspace, factory).await;
    }

    // ACP/headless startup never opens theme files or initializes renderer state.
    // Freeze the palette before the terminal, first frame, or syntax cache.
    if let Some(selector) = app_config.theme() {
        let definition = theme_store::ThemeStore::global()?.load(&selector.name)?;
        zevria_theme::install_theme(&definition)?;
    }

    // `--continue` resolves before the terminal takes over, so a missing
    // session still fails fast and legibly.
    let mut next_start = if args.continue_session {
        let sessions_dir = transcript::sessions_dir(&workspace);
        runtime::SessionStart::Resume(
            transcript::latest_session_file(&sessions_dir)?.with_context(|| {
                format!(
                    "no previous session to continue in {}",
                    sessions_dir.display()
                )
            })?,
        )
    } else {
        runtime::SessionStart::New {
            inherited_models: None,
        }
    };

    let mut terminal = ratatui::try_init()?;
    let mut alternate_scroll = AlternateScrollGuard::enable();
    let mut bracketed_paste = BracketedPasteGuard::enable();
    let mut keyboard_enhancement = KeyboardEnhancementGuard::push();
    // Session switches tear the whole stack down and rebuild it here:
    // `/resume` loads the chosen transcript like `--continue`, `/new` starts
    // an empty root with no opening prompt, and `/implement-fresh` seeds a
    // fresh root with the approved Plan handoff. Both replacements retain the
    // source root's committed Build/Plan selections, not its conversation.
    let session_outcome = loop {
        match run_session(&mut terminal, &app_config, &workspace, next_start).await {
            Ok(outcome) => match replacement_start(outcome) {
                Some(start) => next_start = start,
                None => break Ok(()),
            },
            Err(error) => break Err(error),
        }
    };
    keyboard_enhancement.pop();
    bracketed_paste.disable();
    alternate_scroll.disable();
    ratatui::restore();
    tracing::info!("shutting down");

    session_outcome
}

/// Translate transition data only after the outgoing stack has shut down.
fn replacement_start(outcome: UiOutcome) -> Option<runtime::SessionStart> {
    match outcome {
        UiOutcome::Quit => None,
        UiOutcome::Resume(path) => Some(runtime::SessionStart::Resume(path)),
        UiOutcome::New { models } => Some(runtime::SessionStart::New {
            inherited_models: Some(models),
        }),
        UiOutcome::Fresh { handoff, models } => Some(runtime::SessionStart::FromPlan {
            handoff,
            inherited_models: Some(models),
        }),
    }
}

/// One complete engine run over a single session. Provider, transcript,
/// engine, and supervisor ownership live in [`runtime::RunningSession`]; this
/// function restores only terminal presentation and drives the UI frontend.
async fn run_session(
    terminal: &mut ratatui::DefaultTerminal,
    app_config: &Config,
    workspace: &Path,
    start: runtime::SessionStart,
) -> anyhow::Result<UiOutcome> {
    draw_status_frame(
        terminal,
        match &start {
            runtime::SessionStart::Resume(_) => "Resuming session…",
            runtime::SessionStart::New { .. } | runtime::SessionStart::FromPlan { .. } => {
                "Connecting…"
            }
        },
    )?;

    let mut running = runtime::start_session(app_config, workspace, start).await?;
    let restoration = running.restoration();
    let model_profiles = zevria_foundation::ModelRole::ALL.into_iter().map(|role| {
        (
            role,
            restoration.model_contexts[role.index()].profile.clone(),
        )
    });
    let mut root_app = App::new().with_skill_context(&zevria_instructions::skill::SkillContext {
        catalog: restoration.skill_catalog.clone(),
        pins: transcript::replay_active_skills(&restoration.transcript_items)?,
        mode_enabled: true,
    });
    root_app.restore_session(zevria_tui::RestorationInput {
        items: restoration.transcript_items.clone(),
        workflow: restoration.plan_state.clone(),
        selected_mode: restoration.selected_mode,
        model_profiles: model_profiles.collect(),
        contexts: restoration.model_snapshots.clone(),
        reasoning: restoration.reasoning_levels,
        persistence_error: None,
    });
    for notice in &restoration.startup_notices {
        root_app.push_error(notice.clone());
    }

    let child_launches = subtask_launch_metadata(&restoration.transcript_items);
    let mut views = SessionViews::new(root_app, workspace.to_path_buf());
    views.seed_child_creation_order(&restoration.transcript_items);
    for (child_id, path) in transcript::subsession_files(&restoration.subsessions_dir)? {
        let items = transcript::load(&path)?;
        let metadata = child_launches.get(&child_id).cloned();
        views.restore_child(SubtaskId::new(child_id), metadata, items);
    }
    for item in &restoration.transcript_items {
        let TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::Started { start }) = item
        else {
            continue;
        };
        for descriptor in &start.agents {
            let path = agent_run_path(&restoration.agent_runs_root, &start.run_id, &descriptor.id);
            if path.exists() {
                views.restore_agent_run_stream(AgentRunTranscriptReader::open(&path)?)?;
            }
        }
    }

    let ui_context = UiContext {
        sessions_dir: restoration.sessions_dir.clone(),
        current_session_id: restoration.session_id.clone(),
    };
    let command_tx = running.command_sender();
    let mut event_rx = running.take_event_receiver()?;
    let mut exit_rx = running.take_exit_receiver()?;
    enum ActiveSessionExit {
        Ui(anyhow::Result<UiOutcome>),
        Runtime(Option<runtime::RuntimeTaskExit>),
    }

    let active_exit = {
        let ui = run_ui(terminal, views, &command_tx, &mut event_rx, &ui_context);
        tokio::pin!(ui);
        tokio::select! {
            biased;
            exit = exit_rx.recv() => ActiveSessionExit::Runtime(exit),
            result = &mut ui => ActiveSessionExit::Ui(result),
        }
    };
    let outcome = match active_exit {
        ActiveSessionExit::Ui(outcome) => outcome,
        ActiveSessionExit::Runtime(Some(exit)) => Err(unexpected_runtime_exit(exit)),
        ActiveSessionExit::Runtime(None) => Err(anyhow::anyhow!(
            "session background-task monitor closed unexpectedly"
        )),
    };

    drop(command_tx);
    drop(event_rx);
    drop(exit_rx);
    let cleanup = running.shutdown().await;
    if let Err(error) = cleanup {
        if outcome.is_ok() {
            return Err(error);
        }
        tracing::error!("session cleanup also failed: {error:#}");
    }
    outcome
}

fn unexpected_runtime_exit(exit: runtime::RuntimeTaskExit) -> anyhow::Error {
    match exit.error {
        Some(error) => anyhow::anyhow!("{} task failed: {error}", exit.component),
        None => anyhow::anyhow!(
            "{} stopped unexpectedly while the frontend was still running",
            exit.component
        ),
    }
}

/// Paint a minimal status frame so the initial connect and session switches
/// never show a stale or blank terminal while the provider reconnects.
fn draw_status_frame(terminal: &mut ratatui::DefaultTerminal, status: &str) -> anyhow::Result<()> {
    terminal.draw(|frame| render_startup_frame(frame, status))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_scroll_commands_write_exact_ansi_sequences() {
        let mut enable = String::new();
        crossterm::Command::write_ansi(&EnableAlternateScroll, &mut enable)
            .expect("alternate-scroll enable should serialize");
        assert_eq!(enable.as_bytes(), b"\x1b[?1007h");

        let mut disable = String::new();
        crossterm::Command::write_ansi(&DisableAlternateScroll, &mut disable)
            .expect("alternate-scroll disable should serialize");
        assert_eq!(disable.as_bytes(), b"\x1b[?1007l");
    }

    #[test]
    fn parse_args_accepts_the_continue_flags() {
        assert_eq!(
            parse_args(std::iter::empty::<String>()).expect("no arguments should parse"),
            CliArgs {
                continue_session: false,
                acp: false,
                ensemble_worker: false,
                clean: false,
                skills: None,
                theme: None,
            }
        );
        for flag in ["--continue", "-c"] {
            let args = parse_args([flag.to_string()].into_iter())
                .unwrap_or_else(|_| panic!("{flag} should parse"));
            assert!(args.continue_session);
            assert!(!args.acp);
        }
    }

    #[test]
    fn parse_args_accepts_only_bare_clean() {
        assert_eq!(
            parse_args(["clean".to_string()].into_iter()).unwrap(),
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
                let error = parse_args(args.into_iter().map(str::to_string)).unwrap_err();
                assert!(error.to_string().contains("usage"), "{args:?}: {error}");
            }
        }
        for args in [
            vec!["clean", "skills", "list"],
            vec!["skills", "list", "clean"],
            vec!["clean", "--acp", "--ensemble-worker"],
            vec!["--acp", "clean", "--ensemble-worker"],
            vec!["--acp", "--ensemble-worker", "clean"],
        ] {
            assert!(parse_args(args.into_iter().map(str::to_string)).is_err());
        }
    }

    #[test]
    fn parse_args_accepts_acp_and_rejects_continue_combination() {
        let args = parse_args(["--acp".to_string()].into_iter()).expect("--acp should parse");
        assert!(args.acp);
        assert!(!args.continue_session);

        for continue_flag in ["--continue", "-c"] {
            let error = parse_args(["--acp".to_string(), continue_flag.to_string()].into_iter())
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
            let parsed = parse_args(flags.into_iter().map(str::to_string)).unwrap();
            assert!(parsed.acp && parsed.ensemble_worker);
        }
        for flags in [
            vec!["--ensemble-worker"],
            vec!["--ensemble-worker", "--continue"],
            vec!["--acp", "--ensemble-worker", "-c"],
            vec!["skills", "--ensemble-worker"],
            vec!["sessions", "--ensemble-worker"],
        ] {
            assert!(parse_args(flags.into_iter().map(str::to_string)).is_err());
        }
    }

    #[test]
    fn parse_args_rejects_unknown_arguments() {
        let error = parse_args(["--bogus".to_string()].into_iter())
            .expect_err("unknown arguments should fail");
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

    #[test]
    fn a_background_task_ending_cleanly_is_still_an_unexpected_runtime_failure() {
        let error = unexpected_runtime_exit(runtime::RuntimeTaskExit {
            component: "session engine",
            error: None,
        });
        assert!(error.to_string().contains("stopped unexpectedly"));
        assert!(error.to_string().contains("session engine"));
    }
}
