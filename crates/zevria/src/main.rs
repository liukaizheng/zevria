//! The Zevria composition root: CLI/configuration, compile-time adapters, and
//! the terminal runtime.

#[cfg(test)]
use zevria_ensemble as ensemble;
use zevria_host::{acp_host, clean, config, runtime, skills, theme_store};
mod cli;
mod mcp_cli;
#[cfg(test)]
mod session_model_tests;
mod skills_cli;
mod theme_cli;

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use zevria_foundation::logging;
use zevria_transcript::transcript;
use zevria_tui::{App, SessionViews, TerminalEvents, UiContext, UiOutcome, run_ui};
use zevria_tui_widgets::render_startup_frame;

use crate::config::Config;

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
/// Enter fallback. The Windows console API never reports pastes, so the TUI
/// input reader reassembles key-record bursts into pastes there instead.
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

#[cfg(test)]
#[path = "sessions_cli_tests.rs"]
mod sessions_cli_tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = match cli::startup(std::env::args().skip(1).collect()).await? {
        cli::Startup::Run(args) => args,
        cli::Startup::Exit(code) => std::process::exit(code),
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
    if let Some(command) = args.mcp {
        let code = command.run(&workspace).await?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    cli::diagnostics(&workspace)?;
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
            base_url = %profile.redacted_base_url(),
            protocol = profile.protocol.as_str(),
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
        runtime::SessionStart::Resume(zevria_host::sessions::latest(&workspace)?.with_context(
            || {
                format!(
                    "no previous session to continue in {}",
                    sessions_dir.display()
                )
            },
        )?)
    } else {
        runtime::SessionStart::New {
            inherited_models: None,
        }
    };

    let mut terminal = ratatui::try_init()?;
    let mut terminal_events = TerminalEvents::new();
    let mut alternate_scroll = AlternateScrollGuard::enable();
    let mut bracketed_paste = BracketedPasteGuard::enable();
    let mut keyboard_enhancement = KeyboardEnhancementGuard::push();
    // Session switches tear the whole stack down and rebuild it here:
    // `/resume` loads the chosen transcript like `--continue`, `/new` starts
    // an empty root with no opening prompt, and `/implement-fresh` seeds a
    // fresh root with the approved Plan handoff. Both replacements retain the
    // source root's committed Build/Plan selections, not its conversation.
    let session_outcome = loop {
        match run_session(
            &mut terminal,
            &mut terminal_events,
            &app_config,
            &workspace,
            next_start,
        )
        .await
        {
            Ok(outcome) => match replacement_start(outcome) {
                Some(start) => next_start = start,
                None => break Ok(()),
            },
            Err(error) => break Err(error),
        }
    };
    drop(terminal_events);
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
    terminal_events: &mut TerminalEvents,
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

    let runtime::StartedSession {
        mut running,
        restored: restoration,
    } = {
        let starting = runtime::start_session(app_config, workspace, start);
        tokio::pin!(starting);
        tokio::select! {
            biased;
            interrupted = terminal_events.wait_for_interrupt() => {
                interrupted?;
                return Ok(UiOutcome::Quit);
            }
            started = &mut starting => started?,
        }
    };
    let model_profiles = zevria_foundation::ModelRole::ALL.into_iter().map(|role| {
        (
            role,
            restoration.model_contexts[role.index()].profile.clone(),
        )
    });
    let root_app = App::new()
        .with_mcp_prompts(&restoration.mcp_prompts)
        .with_skill_context(&zevria_instructions::skill::SkillContext {
            catalog: restoration.skill_catalog.clone(),
            pins: restoration.active_skills,
            mode_enabled: true,
        });
    let mut views = SessionViews::new_restored(
        root_app,
        workspace.to_path_buf(),
        zevria_tui::RestorationInput {
            items: restoration.transcript_items,
            workflow: restoration.plan_state,
            selected_mode: restoration.selected_mode,
            model_profiles: model_profiles.collect(),
            contexts: restoration.model_snapshots,
            reasoning: restoration.reasoning_levels,
            persistence_error: None,
        },
    );
    for notice in &restoration.startup_notices {
        views.show_root_error(notice.clone());
    }

    for child in restoration.subtasks {
        views.restore_child(child.id, child.launch, child.items);
    }
    for agent in restoration.agent_runs {
        views.restore_agent_run(agent.records);
    }

    let catalog_workspace = workspace.to_path_buf();
    let catalog_session = restoration.session_id.clone();
    let ui_context = UiContext {
        sessions_dir: restoration.sessions_dir.clone(),
        current_session_id: restoration.session_id.clone(),
        session_catalog: Box::new(move || {
            zevria_host::sessions::resumable(&catalog_workspace, Some(&catalog_session))
        }),
    };
    let command_tx = running.command_sender();
    let mut event_rx = running.take_event_receiver()?;
    let mut exit_rx = running.take_exit_receiver()?;
    #[allow(clippy::large_enum_variant)] // One stack-local teardown outcome.
    enum ActiveSessionExit {
        Ui(anyhow::Result<UiOutcome>),
        Runtime(Option<runtime::RuntimeTaskExit>),
    }

    let active_exit = {
        let ui = run_ui(
            terminal,
            terminal_events,
            views,
            &command_tx,
            &mut event_rx,
            &ui_context,
        );
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
    let cleanup = {
        let shutdown = running.shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            result = &mut shutdown => result,
            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let _ = draw_status_frame(terminal, "Shutting down…");
                shutdown.await
            }
        }
    };
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
    fn a_background_task_ending_cleanly_is_still_an_unexpected_runtime_failure() {
        let error = unexpected_runtime_exit(runtime::RuntimeTaskExit {
            component: "session engine",
            error: None,
        });
        assert!(error.to_string().contains("stopped unexpectedly"));
        assert!(error.to_string().contains("session engine"));
    }
}
