//! Terminal/event loop, channel draining and execution of typed UI effects.

use crate::{
    app::UiAction,
    command::SlashCommand,
    workspace::{ClipboardOrigin, SessionViews},
};
use futures_util::StreamExt as _;
use ratatui::crossterm::event::EventStream;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedSender, error::TryRecvError};
use zevria_model::models::{ModelSelectionScope, SessionModels};
use zevria_session_api::{SessionCommand, SessionEvent, SessionEventReceiver, SessionUpdate};
use zevria_transcript::transcript::{SessionSummary, list_sessions};
use zevria_workflow::PlanHandoff;

#[cfg(test)]
#[path = "session_replacement_tests.rs"]
mod session_replacement_tests;

const MAX_EVENT_BURST: usize = 64;

/// Workspace facts the runtime needs to execute slash commands.
pub struct UiContext {
    /// `{workspace}/.zevria/sessions`, listed by `/resume`.
    pub sessions_dir: PathBuf,
    /// The live session, excluded from the resume picker.
    pub current_session_id: String,
}

/// Why the UI loop ended.
#[derive(Debug, PartialEq)]
pub enum UiOutcome {
    Quit,
    /// Rebuild the whole session stack on the transcript at this path.
    Resume(PathBuf),
    /// Rebuild an empty root session, retaining both committed mode selections.
    New {
        models: SessionModels,
    },
    /// Rebuild the session stack and deliver this typed handoff through
    /// `SessionCommand::StartFromPlan`, retaining both mode selections.
    Fresh {
        handoff: PlanHandoff,
        models: SessionModels,
    },
}

/// Run the terminal event loop until the user quits or switches sessions.
pub async fn run_ui(
    terminal: &mut ratatui::DefaultTerminal,
    mut views: SessionViews,
    commands: &UnboundedSender<SessionCommand>,
    updates_rx: &mut SessionEventReceiver,
    context: &UiContext,
) -> anyhow::Result<UiOutcome> {
    let mut events = EventStream::new();
    let mut engine_done = false;
    let mut clipboard = crate::clipboard::ClipboardService::new();
    let mut files = crate::workspace_files::FileSearchService::new(
        views.startup_workspace.clone(),
        views.file_service_id,
    );
    let mut clipboard_id = 0u64;
    let mut pending_paste: Option<(u64, ClipboardOrigin, u64, usize, Instant)> = None;
    let mut clock = tokio::time::interval(Duration::from_millis(250));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    views.overlays.skills.refresh();

    loop {
        if engine_done {
            views.root.mode_selection_disconnected();
        }
        while let Some(command) = views.overlays.models.commands.pop_front() {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before model management"))?;
        }
        while let Some(command) = views.overlays.skills.commands.pop_front() {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before skill management"))?;
        }
        files.set_request(views.file_search_request());
        views.observe_clock(Instant::now());
        terminal.draw(|frame| views.render(frame))?;

        tokio::select! {
            result = files.receiver.changed() => {
                if result.is_ok() {
                    let completion = files.receiver.borrow_and_update().clone();
                    if let Some(completion) = completion {
                        views.file_search_completed(completion);
                    }
                }
            }
            _ = clock.tick(), if views.clock_required() || pending_paste.is_some() => {
                if pending_paste.as_ref().is_some_and(|(_, _, _, _, started)| started.elapsed() >= crate::clipboard::TIMEOUT)
                    && let Some((_, origin, generation, cursor, _)) = pending_paste.take()
                {
                    views.clipboard_completed(origin, generation, cursor, crate::clipboard::ClipboardResult::Error("Clipboard read timed out. The draft is unchanged; retry after the native operation finishes.".into()));
                }
            }
            completion = clipboard.receiver.recv() => {
                if let Some((id, result)) = completion
                    && pending_paste.as_ref().is_some_and(|(pending, ..)| *pending == id)
                    && let Some((_, origin, generation, cursor, _)) = pending_paste.take()
                {
                    views.clipboard_completed(origin, generation, cursor, result);
                }
            }
            maybe_update = updates_rx.recv(), if !engine_done => match maybe_update {
                Some(update) => {
                    if let Some(outcome) = apply_session_update(&mut views, update)? {
                        return Ok(outcome);
                    }
                    if let Some(outcome) =
                        drain_update_burst(&mut views, updates_rx, &mut engine_done)?
                    {
                        return Ok(outcome);
                    }
                }
                None => {
                    engine_done = true;
                    views.root.mode_selection_disconnected();
                }
            },
            maybe_event = events.next() => match maybe_event {
                Some(Ok(event)) => {
                    match views.handle_event(event) {
                        Some(UiAction::ReadClipboard { generation, cursor }) => {
                            if let Some(origin) = views.clipboard_origin() {
                                clipboard_id = clipboard_id.wrapping_add(1);
                                if pending_paste.is_none() && clipboard.start(clipboard_id) {
                                    pending_paste = Some((clipboard_id, origin, generation, cursor, Instant::now()));
                                } else {
                                    views.clipboard_completed(origin, generation, cursor, crate::clipboard::ClipboardResult::Error("A native clipboard read is still running; retry when it finishes.".into()));
                                }
                            }
                        }
                        Some(UiAction::SetMode { request_id, mode }) => {
                            views.send_mode_selection(commands, request_id, mode);
                        }
                        Some(UiAction::Submit { text, mode, behavior }) => {
                            commands.send(SessionCommand::Turn(zevria_session_api::TurnCommand::Submit { text, mode, behavior })).map_err(|_| {
                                anyhow::anyhow!("the session engine stopped before accepting the prompt")
                            })?;
                        }
                        Some(UiAction::EditTranscript(edit)) => {
                            commands
                                .send(SessionCommand::Turn(zevria_session_api::TurnCommand::EditTranscript(edit)))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting the transcript edit"
                                    )
                                })?;
                        }
                        Some(UiAction::InvokeSkill { name, args, mode }) => {
                            commands
                                .send(SessionCommand::Turn(zevria_session_api::TurnCommand::InvokeSkill { name, args, mode }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting the skill invocation"
                                    )
                                })?;
                        }
                        Some(UiAction::Compact { mode }) => {
                            commands
                                .send(SessionCommand::Turn(zevria_session_api::TurnCommand::Compact { mode }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting compaction"
                                    )
                                })?;
                        }
                        Some(UiAction::RunEnsemble { workflow, prompt }) => {
                            commands
                                .send(SessionCommand::Turn(zevria_session_api::TurnCommand::RunEnsemble { workflow, prompt }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting the ensemble command"
                                    )
                                })?;
                        }
                        Some(UiAction::WorkerControl(control)) => {
                            commands.send(SessionCommand::Control(zevria_session_api::ControlCommand::Worker(control)))
                                .map_err(|_| anyhow::anyhow!("the session engine stopped before accepting worker control"))?;
                        }
                        Some(UiAction::CancelTurn { turn_id }) => {
                            commands
                                .send(SessionCommand::Control(zevria_session_api::ControlCommand::CancelTurn { turn_id }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting cancellation"
                                    )
                                })?;
                        }
                        Some(UiAction::AnswerQuestion {
                            request_id,
                            response,
                        }) => {
                            commands
                                .send(SessionCommand::Control(zevria_session_api::ControlCommand::AnswerQuestion {
                                    request_id,
                                    response,
                                }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting the question response"
                                    )
                                })?;
                        }
                        Some(UiAction::ResolvePlan { expected, decision }) => {
                            commands
                                .send(SessionCommand::Turn(zevria_session_api::TurnCommand::ResolvePlan { expected, decision }))
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "the session engine stopped before accepting the Plan decision"
                                    )
                                })?;
                        }
                        Some(UiAction::Copy { text }) => copy_to_clipboard(&text)?,
                        Some(UiAction::RunCommand(command)) => match command {
                            SlashCommand::Confirm | SlashCommand::Unconfirm | SlashCommand::Baseline | SlashCommand::Unbaseline | SlashCommand::Retry | SlashCommand::CancelPrompt | SlashCommand::Abandon => { views.root.push_error("Worker commands are available only in a live Plan worker pane.".into()); }
                            SlashCommand::Resume => open_resume_picker(&mut views, context),
                            SlashCommand::New => return new_session_outcome(&views.root),
                            SlashCommand::Skills => views.overlays.show_skills(),
                            SlashCommand::Model | SlashCommand::ModelSession => {
                                let scope = if command == SlashCommand::ModelSession {
                                    ModelSelectionScope::SessionOnly
                                } else {
                                    ModelSelectionScope::SessionAndDefault
                                };
                                views.open_model_picker(scope);
                            },
                            SlashCommand::Build
                            | SlashCommand::Orchestrate
                            | SlashCommand::Plan
                            | SlashCommand::Compact
                            | SlashCommand::Implement
                            | SlashCommand::ImplementFresh
                            | SlashCommand::EnsemblePlan
                            | SlashCommand::EnsembleReview => unreachable!(
                                "engine-backed commands are converted to typed UiActions"
                            ),
                        },
                        Some(UiAction::ResumeSession { path }) => {
                            return Ok(UiOutcome::Resume(path));
                        }
                        Some(UiAction::Quit) => return Ok(UiOutcome::Quit),
                        // Pane switches are consumed inside the view manager.
                        Some(UiAction::OpenSubtask { .. })
                        | Some(UiAction::OpenAgentRun { .. })
                        | None => {}
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(UiOutcome::Quit),
            },
        }
    }
}

fn new_session_outcome(root: &crate::App) -> anyhow::Result<UiOutcome> {
    Ok(UiOutcome::New {
        models: root.session_models()?,
    })
}

fn apply_session_update(
    views: &mut SessionViews,
    update: SessionUpdate,
) -> anyhow::Result<Option<UiOutcome>> {
    match update {
        SessionUpdate::Lifecycle(SessionEvent::FreshPlanHandoffRequested { handoff }) => {
            Ok(Some(UiOutcome::Fresh {
                handoff,
                models: views.root.session_models()?,
            }))
        }
        SessionUpdate::Lifecycle(event) => {
            views.apply(event);
            Ok(None)
        }
        SessionUpdate::Streams(batch) => {
            views.apply_streams(&batch);
            Ok(None)
        }
    }
}

pub(crate) fn drain_update_burst(
    views: &mut SessionViews,
    updates_rx: &mut SessionEventReceiver,
    engine_done: &mut bool,
) -> anyhow::Result<Option<UiOutcome>> {
    for _ in 0..MAX_EVENT_BURST {
        match updates_rx.try_recv() {
            Ok(update) => {
                if let Some(outcome) = apply_session_update(views, update)? {
                    return Ok(Some(outcome));
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                *engine_done = true;
                views.root.mode_selection_disconnected();
                break;
            }
        }
    }
    Ok(None)
}

/// List this workspace's resumable sessions — excluding the live one — and
/// open the picker over them. Listing failures surface as a root-pane notice;
/// the UI keeps running either way.
fn open_resume_picker(views: &mut SessionViews, context: &UiContext) {
    match list_sessions(&context.sessions_dir) {
        Ok(sessions) => {
            let other_sessions: Vec<SessionSummary> = sessions
                .into_iter()
                .filter(|summary| summary.id != context.current_session_id)
                .collect();
            views.open_session_picker(other_sessions);
        }
        Err(error) => views.show_root_error(format!("Failed to list sessions: {error:#}")),
    }
}

/// Ask the terminal to put `text` on the system clipboard via OSC 52.
/// Emitting the sequence between draws is safe — it doesn't move the cursor.
fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
    )?;
    Ok(())
}
