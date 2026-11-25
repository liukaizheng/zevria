//! Terminal/event loop, channel draining and execution of typed UI effects.

use crate::{
    app::UiAction,
    command::SlashCommand,
    input::InputReader,
    workspace::{ClipboardOrigin, SessionViews},
};
use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedSender, error::TryRecvError};
use zevria_model::models::{ModelSelectionScope, SessionModels};
use zevria_session_api::PlanEvent;
use zevria_session_api::{SessionCommand, SessionEvent, SessionEventReceiver, SessionUpdate};
use zevria_transcript::transcript::SessionSummary;
use zevria_workflow::PlanHandoff;

#[cfg(test)]
#[path = "session_replacement_tests.rs"]
mod session_replacement_tests;

const MAX_EVENT_BURST: usize = 64;
const MAX_STARTUP_EVENT_BUFFER: usize = 64;

/// Long-lived terminal event reader shared across session startup, UI runs,
/// and session replacements so `Ctrl-C` can interrupt startup without dropping
/// typeahead or spawning duplicate crossterm reader threads. The shared reader
/// also reassembles Windows paste bursts before they reach either consumer.
pub struct TerminalEvents {
    reader: InputReader,
    buffered: VecDeque<Event>,
}

impl Default for TerminalEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalEvents {
    pub fn new() -> Self {
        Self {
            reader: InputReader::new(),
            buffered: VecDeque::new(),
        }
    }

    /// Wait for a normalized `Ctrl-C` key press (or input stream EOF) while
    /// buffering other events for replay when `run_ui` starts.
    pub async fn wait_for_interrupt(&mut self) -> anyhow::Result<()> {
        if let Some(pos) = self.buffered.iter().position(is_ctrl_c) {
            self.buffered.drain(..=pos);
            return Ok(());
        }
        loop {
            match self.reader.next().await {
                Some(Ok(event)) => {
                    if buffer_or_interrupt(&mut self.buffered, event) {
                        return Ok(());
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            }
        }
    }

    async fn next(&mut self) -> Option<std::io::Result<Event>> {
        if let Some(event) = self.buffered.pop_front() {
            return Some(Ok(event));
        }
        self.reader.next().await
    }
}

fn is_ctrl_c(event: &Event) -> bool {
    matches!(
        crate::input::normalize(event.clone()),
        crate::input::InputEvent::User(crate::input::UserInput::Key(key))
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL
    )
}

fn buffer_or_interrupt(buffered: &mut VecDeque<Event>, event: Event) -> bool {
    if is_ctrl_c(&event) {
        return true;
    }
    if buffered.len() < MAX_STARTUP_EVENT_BUFFER {
        buffered.push_back(event);
    }
    false
}

fn send_turn_action(
    commands: &UnboundedSender<SessionCommand>,
    command: zevria_session_api::TurnCommand,
) -> anyhow::Result<()> {
    use zevria_session_api::TurnCommand;
    let operation = match &command {
        TurnCommand::ContinueAfterJobs { .. } => "the background job continuation",
        TurnCommand::Submit { .. } => "the prompt",
        TurnCommand::EditTranscript(_) => "the transcript edit",
        TurnCommand::InvokeMcpPrompt { .. } => "the MCP prompt",
        TurnCommand::InvokeSkill { .. } => "the skill invocation",
        TurnCommand::RevisePlanWithMessage { .. } | TurnCommand::RevisePlanWithSkill { .. } => {
            "the Plan revision"
        }
        TurnCommand::Compact { .. } => "compaction",
        TurnCommand::RunEnsemble { .. } => "the ensemble command",
        TurnCommand::ResolvePlan { .. } => "the Plan decision",
        _ => "the turn command",
    };
    commands
        .send(SessionCommand::Turn(command))
        .map_err(|_| anyhow::anyhow!("the session engine stopped before accepting {operation}"))
}

pub(crate) fn dispatch_turn_action(
    views: &mut SessionViews,
    commands: &UnboundedSender<SessionCommand>,
    engine_done: &mut bool,
    action: UiAction,
) -> anyhow::Result<()> {
    if *engine_done && views.workspace.root.has_pending_prompt() {
        views.workspace.root.engine_disconnected();
        return Ok(());
    }
    let UiAction::Session(SessionCommand::Turn(command)) = action else {
        anyhow::bail!("queued work must be a turn command");
    };
    match send_turn_action(commands, command) {
        Err(_) if views.workspace.root.has_pending_prompt() => {
            *engine_done = true;
            views.workspace.root.engine_disconnected();
            Ok(())
        }
        result => result,
    }
}

/// Workspace facts the runtime needs to execute slash commands.
pub struct UiContext {
    /// `{workspace}/.zevria/sessions`, listed by `/resume`.
    pub sessions_dir: PathBuf,
    /// The live session, excluded from the resume picker.
    pub current_session_id: String,
    /// Host-owned discovery; presentation never knows the host implementation.
    pub session_catalog: Box<dyn Fn() -> anyhow::Result<Vec<SessionSummary>> + Send + Sync>,
}

/// Why the UI loop ended.
#[derive(Debug, PartialEq)]
// Owned replacement data crosses the terminal teardown boundary only once.
#[allow(clippy::large_enum_variant)]
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
    events: &mut TerminalEvents,
    mut views: SessionViews,
    commands: &UnboundedSender<SessionCommand>,
    updates_rx: &mut SessionEventReceiver,
    context: &UiContext,
) -> anyhow::Result<UiOutcome> {
    let mut engine_done = false;
    let mut dirty = true;
    let mut clipboard = crate::clipboard::ClipboardService::new();
    let mut files = crate::workspace_files::FileSearchService::new(
        views.startup_workspace.clone(),
        views.file_service_id,
    );
    let mut clipboard_id = 0u64;
    let mut pending_paste: Option<(u64, ClipboardOrigin, u64, usize, Instant)> = None;
    let mut clock = tokio::time::interval(Duration::from_millis(250));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    views.workspace.controls.skills.inspector.refresh();

    loop {
        views
            .workspace
            .root
            .jobs_inspector_mut()
            .tick(Instant::now());
        while let Some(command) = views
            .workspace
            .root
            .jobs_inspector_mut()
            .commands
            .pop_front()
        {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before job control"))?;
        }
        while let Some(command) = views.workspace.controls.models.commands.pop_front() {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before model management"))?;
        }
        while let Some(command) = views
            .workspace
            .controls
            .skills
            .inspector
            .commands
            .pop_front()
        {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before skill management"))?;
        }
        while let Some(command) = views.workspace.controls.mcp.inspector.commands.pop_front() {
            commands
                .send(command)
                .map_err(|_| anyhow::anyhow!("session engine stopped before MCP management"))?;
        }
        if !engine_done && let Some(action) = views.dispatch_queued_root_work() {
            dispatch_turn_action(&mut views, commands, &mut engine_done, action)?;
            dirty = true;
        }
        files.set_request(views.file_search_request());
        if dirty {
            views.observe_clock(Instant::now());
            terminal.draw(|frame| views.render(frame))?;
            dirty = false;
        }

        tokio::select! {
            result = files.receiver.changed() => {
                if result.is_ok() {
                    let completion = files.receiver.borrow_and_update().clone();
                    if let Some(completion) = completion {
                        views.file_search_completed(completion);
                        dirty = true;
                    }
                }
            }
            _ = clock.tick(), if views.clock_required() || pending_paste.is_some() => {
                dirty |= views.clock_required();
                if pending_paste.as_ref().is_some_and(|(_, _, _, _, started)| started.elapsed() >= crate::clipboard::TIMEOUT)
                    && let Some((_, origin, generation, cursor, _)) = pending_paste.take()
                {
                    dirty = true;
                    views.clipboard_completed(origin, generation, cursor, crate::clipboard::ClipboardResult::Error("Clipboard read timed out. The draft is unchanged; retry after the native operation finishes.".into()));
                }
            }
            completion = clipboard.receiver.recv() => {
                if let Some((id, result)) = completion
                    && pending_paste.as_ref().is_some_and(|(pending, ..)| *pending == id)
                    && let Some((_, origin, generation, cursor, _)) = pending_paste.take()
                {
                    views.clipboard_completed(origin, generation, cursor, result);
                    dirty = true;
                }
            }
            maybe_update = updates_rx.recv(), if !engine_done => match maybe_update {
                Some(update) => {
                    dirty = true;
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
                    dirty = true;
                    views.workspace.root.engine_disconnected();
                }
            },
            maybe_event = events.next() => match maybe_event {
                Some(Ok(event)) => {
                    dirty = true;
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
                        Some(UiAction::Session(command)) => {
                            use zevria_session_api::{ControlCommand, ManagementCommand};
                            match command {
                                SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) => {
                                    views.send_mode_selection(commands, request_id, mode);
                                }
                                command @ SessionCommand::Turn(_) => {
                                    dispatch_turn_action(&mut views, commands, &mut engine_done, UiAction::Session(command))?;
                                }
                                command @ SessionCommand::Control(ControlCommand::CancelTurn { .. }) => {
                                    if commands.send(command).is_err() {
                                        if !views.workspace.root.has_pending_prompt() {
                                            anyhow::bail!("the session engine stopped before accepting cancellation");
                                        }
                                        engine_done = true;
                                        views.workspace.root.engine_disconnected();
                                    }
                                }
                                command => commands.send(command).map_err(|_| anyhow::anyhow!("the session engine stopped before accepting the command"))?,
                            }
                        }
                        Some(UiAction::Copy { text }) => copy_to_clipboard(&text)?,
                        Some(UiAction::RunCommand(command)) => match command {
                            SlashCommand::Confirm | SlashCommand::Unconfirm | SlashCommand::Baseline | SlashCommand::Unbaseline | SlashCommand::Retry | SlashCommand::CancelPrompt | SlashCommand::Abandon => { views.workspace.root.push_error("Worker commands are available only in a live Plan worker pane.".into()); }
                            SlashCommand::Resume => open_resume_picker(&mut views, context),
                            SlashCommand::New => return new_session_outcome(&views.workspace.root),
                            SlashCommand::Skills => views.workspace.controls.show_skills(),
                            SlashCommand::Mcp => views.workspace.controls.show_mcp(),
                            SlashCommand::Prompt => unreachable!("MCP prompts are classified separately"),
                            SlashCommand::Jobs => views.show_jobs(),
                            SlashCommand::Subtasks => views.show_subtasks(),
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
                        // Pane switches and job controls are consumed inside the view manager.
                        Some(UiAction::OpenSubtask { .. })
                        | Some(UiAction::OpenAgentRun { .. })
                        | Some(UiAction::JobControl { .. })
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
        SessionUpdate::Lifecycle(SessionEvent::Plan(PlanEvent::FreshHandoffRequested {
            handoff,
        })) => Ok(Some(UiOutcome::Fresh {
            handoff: *handoff,
            models: views.workspace.root.session_models()?,
        })),
        SessionUpdate::Lifecycle(event) => {
            views.apply(event);
            Ok(None)
        }
        SessionUpdate::Streams(batch) => {
            views.apply_streams(batch);
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
                views.workspace.root.engine_disconnected();
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
    match (context.session_catalog)() {
        Ok(sessions) => views.open_session_picker(sessions),
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

#[cfg(test)]
mod startup_interrupt_tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyEventKind};

    #[test]
    fn startup_interrupt_recognizes_normalized_ctrl_c_and_buffers_other_events() {
        let mut buffered = VecDeque::new();
        let typed = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let resize = Event::Resize(120, 40);
        let pasted = Event::Paste("first\nsecond".into());
        let release_ctrl_c = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        ));
        let super_ctrl_c = Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SUPER,
        ));
        assert!(!buffer_or_interrupt(&mut buffered, typed.clone()));
        assert!(!buffer_or_interrupt(&mut buffered, resize.clone()));
        assert!(!buffer_or_interrupt(&mut buffered, pasted.clone()));
        assert!(!buffer_or_interrupt(&mut buffered, release_ctrl_c.clone()));
        assert!(!buffer_or_interrupt(&mut buffered, super_ctrl_c.clone()));
        assert_eq!(buffered.len(), 5);
        assert_eq!(buffered[0], typed);
        assert_eq!(buffered[1], resize);
        assert_eq!(buffered[2], pasted);

        for press in [
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL)),
        ] {
            assert!(buffer_or_interrupt(&mut buffered, press));
        }
        assert_eq!(buffered.len(), 5);
    }

    #[test]
    fn startup_event_buffer_is_bounded() {
        let mut buffered = VecDeque::new();
        for _ in 0..(MAX_STARTUP_EVENT_BUFFER + 16) {
            assert!(!buffer_or_interrupt(&mut buffered, Event::Resize(80, 24)));
        }
        assert_eq!(buffered.len(), MAX_STARTUP_EVENT_BUFFER);
    }
}
