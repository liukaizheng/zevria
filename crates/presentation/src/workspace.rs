//! Frontend-neutral workspace ownership, event routing, and command admission.
use crate::command::{ClassifiedInput, CommandRegistry, SlashCommand};
use crate::jobs::JobsInspector;
use crate::question::QuestionForm;
use crate::{PaneScope, Reduction, RestorationInput, SessionModel};
use std::{path::PathBuf, sync::Arc, time::Instant};
use zevria_foundation::{
    ModelRole, QuestionRequest, QuestionRequestId, RequestBehavior, SessionMode, SubtaskId,
    SubtaskKind, SubtaskLaunchMetadata, SubtaskStatus, TurnId,
};
use zevria_session_api::{
    JobEvent, ManagementEvent, PlanEvent, QuestionEvent, SubtaskEvent, TurnEvent,
};
use zevria_session_api::{SessionCommand, SessionEvent, SessionUpdate};
use zevria_workflow::PlanHandoff;

pub struct ChildPane {
    pub id: SubtaskId,
    pub title: String,
    pub workspace: Option<String>,
    pub kind: SubtaskKind,
    pub status: Option<SubtaskStatus>,
    pub model: SessionModel,
}
/// One root and its inspect panes, with frontend-specific pane and input
/// adapters. Rendering, focus, and widget state belong to those adapters.
pub struct WorkspaceModel<R = SessionModel, S = ChildPane, A = (), O = WorkspaceControls> {
    pub root: R,
    pub children: Vec<S>,
    pub agents: Vec<A>,
    pub controls: O,
    pub session_id: String,
    pub transcript_path: PathBuf,
}

impl<R, S, A, O> WorkspaceModel<R, S, A, O> {
    pub fn new(root: R, controls: O) -> Self {
        Self {
            root,
            children: Vec::new(),
            agents: Vec::new(),
            controls,
            session_id: String::new(),
            transcript_path: PathBuf::new(),
        }
    }
}

pub struct WorkspaceControls {
    pub question: Option<QuestionForm>,
    pub models: crate::models::ModelPicker,
    pub mcp: crate::mcp::McpInspector,
    pub skills: crate::skills::SkillInspector,
    pub subtasks_open: bool,
    pub registry: CommandRegistry,
    pub selected_child: Option<SubtaskId>,
}

impl Default for WorkspaceControls {
    fn default() -> Self {
        Self {
            question: None,
            models: Default::default(),
            mcp: Default::default(),
            skills: Default::default(),
            subtasks_open: false,
            registry: CommandRegistry::default(),
            selected_child: None,
        }
    }
}

pub struct WorkspaceRestoration {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub session: RestorationInput,
    pub skills: Vec<zevria_instructions::skill::SkillMeta>,
    pub mcp_prompts: Vec<(
        zevria_foundation::McpServerName,
        zevria_foundation::McpPromptInfo,
    )>,
    pub notices: Vec<String>,
    pub subtasks: Vec<RestoredChild>,
}

pub struct RestoredChild {
    pub id: SubtaskId,
    pub launch: Option<SubtaskLaunchMetadata>,
    pub items: Vec<zevria_transcript::TranscriptItem>,
}

#[derive(Default)]
pub struct WorkspaceReduction {
    pub root: Option<Reduction>,
    pub fresh: Option<PlanHandoff>,
}
pub enum InputRoute {
    Command(Box<SessionCommand>),
    New,
    Resume,
    Models(zevria_model::models::ModelSelectionScope),
    Jobs,
    Mcp,
    McpPrompt(Box<SessionCommand>),
    Subtasks,
    Notice(String),
    Retained,
}
/// Admission shared by native composers. Editors decide when to stage/recover
/// a draft and queues decide when to dispatch; neither reimplements these commands.
pub fn admit_work(
    root: &mut SessionModel,
    input: ClassifiedInput,
    behavior: RequestBehavior,
    mcp_mode: SessionMode,
) -> InputRoute {
    use zevria_session_api::TurnCommand;
    if root.ready_revision_target().is_some()
        && matches!(
            &input,
            ClassifiedInput::Orchestrate(_) | ClassifiedInput::Ensemble { .. }
        )
    {
        return InputRoute::Notice("This plan is awaiting review; send ordinary feedback to revise it, or reopen the review with p.".into());
    }
    let command = match input {
        ClassifiedInput::Message(text) => root.submit(text, behavior),
        ClassifiedInput::Orchestrate(_) if root.next_mode() != SessionMode::Build => {
            return InputRoute::Notice(
                "Orchestration is available only in Build mode; the draft was retained.".into(),
            );
        }
        ClassifiedInput::Orchestrate(text) => root.submit(text, RequestBehavior::Orchestrate),
        ClassifiedInput::Skill { name, args } => root.invoke_skill(name, args),
        ClassifiedInput::Ensemble { workflow, prompt } => root.run_ensemble(workflow, prompt),
        ClassifiedInput::Builtin(SlashCommand::Compact) => root.compact(),
        ClassifiedInput::McpPrompt {
            server,
            prompt,
            arguments,
        } => {
            if !root.begin_mcp_prompt(mcp_mode) {
                return InputRoute::Notice(
                    "Finish reviewing the current Plan before running an MCP prompt.".into(),
                );
            }
            return InputRoute::McpPrompt(Box::new(SessionCommand::Turn(
                TurnCommand::InvokeMcpPrompt {
                    server,
                    prompt,
                    arguments,
                    mode: mcp_mode,
                },
            )));
        }
        ClassifiedInput::Builtin(_) => return InputRoute::Retained,
    };
    command.map_or(InputRoute::Retained, |command| {
        InputRoute::Command(Box::new(SessionCommand::Turn(command)))
    })
}

/// Root question lifecycle dispatch without owning frontend focus or editors.
pub enum QuestionRoute {
    Open {
        turn_id: TurnId,
        request: QuestionRequest,
    },
    Close,
    Ignore,
}

pub fn route_question(
    event: QuestionEvent,
    active: Option<(TurnId, &QuestionRequestId)>,
    accepts_progress: impl FnOnce(TurnId) -> bool,
) -> QuestionRoute {
    match event {
        QuestionEvent::Asked { turn_id, request } if accepts_progress(turn_id) => {
            QuestionRoute::Open { turn_id, request }
        }
        QuestionEvent::Closed {
            turn_id,
            request_id,
        } if active == Some((turn_id, &request_id)) => QuestionRoute::Close,
        _ => QuestionRoute::Ignore,
    }
}

pub fn accepted_terminal_turn(
    event: &SessionEvent,
    accepts_terminal: impl FnOnce(TurnId) -> bool,
) -> Option<TurnId> {
    match event {
        SessionEvent::Turn(
            TurnEvent::Completed { turn_id, .. }
            | TurnEvent::Recovered { turn_id }
            | TurnEvent::Failed { turn_id, .. }
            | TurnEvent::Rejected { turn_id, .. }
            | TurnEvent::Cancelled { turn_id },
        ) => Some(*turn_id),
        _ => None,
    }
    .filter(|id| accepts_terminal(*id))
}

pub fn model_picker_gate(
    root_focused: bool,
    picker_open: bool,
    question_open: bool,
) -> Result<(), &'static str> {
    if !root_focused {
        Err("Return to the root conversation to change models.")
    } else if picker_open {
        Err("Model selection is already open.")
    } else if question_open {
        Err("Answer the current question before changing models.")
    } else {
        Ok(())
    }
}

impl Default for WorkspaceModel {
    fn default() -> Self {
        Self::new(SessionModel::default(), WorkspaceControls::default())
    }
}

impl WorkspaceModel {
    pub fn restore(restored: WorkspaceRestoration) -> Self {
        let mut workspace = Self {
            session_id: restored.session_id,
            transcript_path: restored.transcript_path,
            ..Self::default()
        };
        workspace.controls.registry = CommandRegistry::new(restored.skills);
        workspace.controls.mcp.prompts = restored.mcp_prompts;
        workspace
            .controls
            .registry
            .set_mcp_prompts(&workspace.controls.mcp.prompts);
        let restored_launches = restored
            .session
            .items
            .iter()
            .filter_map(|item| {
                if let zevria_transcript::transcript::TranscriptItem::ToolResults {
                    metadata, ..
                } = item
                {
                    Some(metadata)
                } else {
                    None
                }
            })
            .flat_map(|metadata| metadata.iter().flat_map(|entry| entry.subtasks()))
            .filter_map(|entry| {
                entry
                    .launch
                    .as_ref()
                    .map(|launch| (launch.clone(), entry.status))
            })
            .collect::<Vec<_>>();
        workspace.root.restore(restored.session);
        for notice in restored.notices {
            workspace.root.conversation.push_error(notice);
        }
        for (launch, status) in restored_launches {
            workspace.install_launch(&launch, Some(status));
        }
        for child in restored.subtasks {
            let pane = workspace.child(&child.id);
            pane.model.conversation.restore(child.items);
            if let Some(launch) = child.launch {
                workspace.install_launch(&launch, None);
            }
        }
        // Live ACP worker review/abandonment routing is intentionally not copied
        // from SessionViews. Root ensemble rows remain fully inspectable.
        workspace
    }
    fn child(&mut self, id: &SubtaskId) -> &mut ChildPane {
        if let Some(index) = self.children.iter().position(|child| &child.id == id) {
            return &mut self.children[index];
        }
        let mut model = SessionModel::default();
        model.model_profiles = Arc::clone(&self.root.model_profiles);
        for role in [ModelRole::Review, ModelRole::Explore, ModelRole::Builder] {
            if let Some(level) = self.root.session.reasoning(role) {
                model.install_reasoning(role, level);
            }
        }
        model.set_scope(PaneScope::Inspect {
            role: Some(ModelRole::Explore),
        });
        self.children.push(ChildPane {
            id: id.clone(),
            title: id.to_string(),
            workspace: None,
            kind: SubtaskKind::Explore,
            status: None,
            model,
        });
        self.children.last_mut().unwrap()
    }
    fn install_launch(&mut self, launch: &SubtaskLaunchMetadata, status: Option<SubtaskStatus>) {
        let child = self.child(&launch.id);
        child.title = launch.title.clone();
        child.kind = launch.kind;
        child.workspace = launch.workspace.clone();
        if !child.status.is_some_and(SubtaskStatus::is_terminal) && status.is_some() {
            child.status = status;
        }
        child.model.set_scope(PaneScope::Inspect {
            role: Some(launch.kind.model_role()),
        });
    }
    pub fn apply(&mut self, update: SessionUpdate) -> WorkspaceReduction {
        match update {
            SessionUpdate::Streams(batch) => {
                let root = batch.root.map(|stream| self.root.apply_stream(stream));
                for (id, stream) in batch.subtasks {
                    self.child(&id).model.apply_stream(stream);
                }
                WorkspaceReduction { root, fresh: None }
            }
            SessionUpdate::Lifecycle(SessionEvent::Subtask(SubtaskEvent::Session {
                id,
                event,
            })) => {
                let model = &mut self.child(&id).model;
                model.retain_unsaved_partial(&event);
                model.reduce(*event);
                WorkspaceReduction::default()
            }
            SessionUpdate::Lifecycle(SessionEvent::Question(event)) => {
                let active = self
                    .controls
                    .question
                    .as_ref()
                    .map(|q| (q.turn_id, &q.request.id));
                match route_question(event, active, |id| self.root.accepts_progress(id)) {
                    QuestionRoute::Open { turn_id, request } => {
                        self.controls.question = Some(QuestionForm::new(turn_id, request));
                    }
                    QuestionRoute::Close => self.controls.question = None,
                    QuestionRoute::Ignore => {}
                }
                WorkspaceReduction::default()
            }
            SessionUpdate::Lifecycle(mut event) => {
                if let Some(crate::skills::SkillChange::Completions(entries)) =
                    self.controls.skills.event(&event, 0)
                {
                    self.controls.registry.set_skills(entries);
                }
                if matches!(
                    &event,
                    SessionEvent::Manage(
                        ManagementEvent::SkillsChanged { .. }
                            | ManagementEvent::SkillsResult { .. }
                    )
                ) {
                    return WorkspaceReduction::default();
                }
                if self.controls.mcp.event(&event) {
                    self.controls
                        .registry
                        .set_mcp_prompts(&self.controls.mcp.prompts);
                    return WorkspaceReduction::default();
                }
                if let SessionEvent::Manage(ManagementEvent::ModelsResult { request_id, result }) =
                    &event
                {
                    if self.controls.models.accept(request_id, result) {
                        self.root.install_validated_model_result(result);
                        if let Some(notice) = self.controls.models.take_settlement_notice() {
                            self.root.conversation.push_error(notice);
                        }
                        if !self.controls.models.is_open() {
                            self.root.session.finish_model_management();
                        }
                    }
                    return WorkspaceReduction::default();
                }
                if matches!(
                    event,
                    SessionEvent::Job(JobEvent::Updated { .. } | JobEvent::ControlResult { .. })
                ) {
                    let root = self.root.reduce(event);
                    self.select_default_job();
                    return WorkspaceReduction {
                        root: Some(root),
                        fresh: None,
                    };
                }
                let mut fresh = None;
                match &mut event {
                    SessionEvent::Subtask(SubtaskEvent::Launched { descriptor, .. }) => {
                        let child = self.child(&descriptor.id);
                        child.title = descriptor.title.clone();
                        child.kind = descriptor.kind;
                        child.workspace = descriptor.workspace.clone();
                        if child.status.is_none() {
                            child.status = Some(SubtaskStatus::Starting);
                        }
                        descriptor.status = child.status.expect("launched child status");
                        child.model.set_scope(PaneScope::Inspect {
                            role: Some(descriptor.kind.model_role()),
                        });
                    }
                    SessionEvent::Subtask(SubtaskEvent::Status { id, status, .. }) => {
                        let child = self.child(id);
                        if child.status.is_none_or(|previous| {
                            !previous.is_terminal() && *status != SubtaskStatus::Starting
                        }) {
                            child.status = Some(*status);
                        }
                        *status = child.status.expect("child status installed");
                    }
                    SessionEvent::Turn(TurnEvent::ToolResults { metadata, .. }) => {
                        for entry in metadata.iter().flat_map(|m| m.subtasks()) {
                            if let Some(launch) = &entry.launch {
                                self.install_launch(launch, Some(entry.status));
                            }
                        }
                    }
                    SessionEvent::Plan(PlanEvent::FreshHandoffRequested { handoff }) => {
                        fresh = Some(handoff.as_ref().clone())
                    }
                    _ => {}
                }
                let terminal = accepted_terminal_turn(&event, |id| self.root.accepts_terminal(id));
                if terminal.is_some_and(|id| {
                    self.controls
                        .question
                        .as_ref()
                        .is_some_and(|q| q.turn_id == id)
                }) {
                    self.controls.question = None;
                }
                self.root.retain_unsaved_partial(&event);
                let root = self.root.reduce(event);
                WorkspaceReduction {
                    root: Some(root),
                    fresh,
                }
            }
        }
    }
    pub fn visible(&self) -> &SessionModel {
        self.controls
            .selected_child
            .as_ref()
            .and_then(|id| self.children.iter().find(|c| &c.id == id))
            .map_or(&self.root, |c| &c.model)
    }
    pub fn observe_clock(&mut self, now: Instant) {
        self.root.observe_clock(now);
        for child in &mut self.children {
            child.model.observe_clock(now);
        }
    }
    pub fn jobs_panel_visible(&self) -> bool {
        self.root.jobs_inspector().is_open()
            && self.controls.selected_child.is_none()
            && self.root.workflow.dialog().is_none()
    }
    pub fn subtasks_panel_visible(&self) -> bool {
        self.controls.subtasks_open
            && !self.children.is_empty()
            && self.root.workflow.dialog().is_none()
    }
    pub fn select_default_job(&mut self) {
        if self.root.jobs_inspector().is_open() && self.root.jobs_inspector().inspected().is_none()
        {
            let jobs = self.root.jobs();
            let id = jobs
                .iter()
                .find(|job| JobsInspector::can_terminate(job))
                .or_else(|| jobs.first())
                .map(|job| job.id.clone());
            if let Some(id) = id {
                self.root.jobs_inspector_mut().inspect(id);
            }
        }
    }
    pub fn disconnected(&mut self) -> Reduction {
        self.controls.question = None;
        self.controls.models.reset();
        self.controls.mcp = Default::default();
        self.controls.skills = Default::default();
        self.controls.subtasks_open = false;
        self.root.disconnected()
    }
    pub fn model_selection_available(&self) -> Result<(), &'static str> {
        model_picker_gate(
            self.controls.selected_child.is_none(),
            self.controls.models.is_open(),
            self.controls.question.is_some(),
        )?;
        self.root.model_selection_available()
    }
    pub fn open_model_picker(&mut self, scope: zevria_model::models::ModelSelectionScope) -> bool {
        if self.model_selection_available().is_err() {
            return false;
        }
        let Some(mode) = self.root.begin_model_management() else {
            return false;
        };
        self.controls.models.show(mode, scope)
    }
    pub fn route_input(&mut self, input: &str, orchestrate: bool) -> InputRoute {
        let classified = self
            .controls
            .registry
            .classify_prompt(&zevria_content::UserPrompt::from_text(input));
        match classified {
            Ok(ClassifiedInput::Builtin(SlashCommand::Jobs)) => return InputRoute::Jobs,
            Ok(ClassifiedInput::Builtin(SlashCommand::Mcp)) => return InputRoute::Mcp,
            Ok(ClassifiedInput::Builtin(SlashCommand::Subtasks)) => return InputRoute::Subtasks,
            _ => {}
        }
        if self.root.is_busy() {
            return InputRoute::Retained;
        }
        let classified = match classified {
            Ok(value) => value,
            Err(error) => return InputRoute::Notice(error.to_string()),
        };
        let command = match classified {
            work @ (ClassifiedInput::Message(_)
            | ClassifiedInput::Orchestrate(_)
            | ClassifiedInput::Skill { .. }
            | ClassifiedInput::McpPrompt { .. }
            | ClassifiedInput::Builtin(SlashCommand::Compact)) => {
                let mode = self.root.next_mode();
                let behavior = if orchestrate && mode == SessionMode::Build {
                    RequestBehavior::Orchestrate
                } else {
                    RequestBehavior::Standard
                };
                return admit_work(&mut self.root, work, behavior, mode);
            }
            ClassifiedInput::Builtin(SlashCommand::Build) => self
                .root
                .select_mode(SessionMode::Build)
                .map(SessionCommand::Manage),
            ClassifiedInput::Builtin(SlashCommand::Plan) => self
                .root
                .select_mode(SessionMode::Plan)
                .map(SessionCommand::Manage),
            ClassifiedInput::Builtin(
                command @ (SlashCommand::Implement | SlashCommand::ImplementFresh),
            ) => {
                let Some(artifact) = self.root.workflow.submitted_artifact() else {
                    return InputRoute::Notice("No submitted Plan is available.".into());
                };
                let version = artifact.version;
                self.root
                    .resolve_plan(
                        version,
                        if command == SlashCommand::Implement {
                            zevria_workflow::PlanDecision::ImplementCurrent
                        } else {
                            zevria_workflow::PlanDecision::ImplementFresh
                        },
                    )
                    .map(SessionCommand::Turn)
            }
            ClassifiedInput::Builtin(SlashCommand::Model) => {
                return InputRoute::Models(
                    zevria_model::models::ModelSelectionScope::SessionAndDefault,
                );
            }
            ClassifiedInput::Builtin(SlashCommand::ModelSession) => {
                return InputRoute::Models(zevria_model::models::ModelSelectionScope::SessionOnly);
            }
            ClassifiedInput::Builtin(SlashCommand::New) => return InputRoute::New,
            ClassifiedInput::Builtin(SlashCommand::Resume) => return InputRoute::Resume,
            ClassifiedInput::Builtin(_) | ClassifiedInput::Ensemble { .. } => {
                return InputRoute::Notice(
                    "This command is not available in this frontend; the draft was retained."
                        .into(),
                );
            }
        };
        command.map_or(InputRoute::Retained, |command| {
            InputRoute::Command(Box::new(command))
        })
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn routes_management_without_turn_and_explicit_prompt_as_turn() {
        let mut workspace = WorkspaceModel::default();
        assert!(matches!(
            workspace.route_input("/mcp", false),
            InputRoute::Mcp
        ));
        assert!(!workspace.root.is_busy());
        let InputRoute::McpPrompt(command) =
            workspace.route_input("/prompt docs:summary topic=release", false)
        else {
            panic!("MCP prompt route");
        };
        assert!(
            matches!(*command, SessionCommand::Turn(zevria_session_api::TurnCommand::InvokeMcpPrompt { server, prompt, arguments, mode: SessionMode::Build })
            if server.as_str() == "docs" && prompt == "summary" && arguments["topic"] == "release")
        );
        assert!(matches!(
            workspace.route_input("/mcp", false),
            InputRoute::Mcp
        ));
    }
}
