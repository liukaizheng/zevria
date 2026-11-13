//! Desktop-local root/child routing. All per-pane event semantics are shared.
pub mod question;
use question::QuestionForm;
use std::{path::PathBuf, sync::Arc, time::Instant};
use zevria_foundation::{
    ModelRole, RequestBehavior, SessionMode, SubtaskId, SubtaskKind, SubtaskLaunchMetadata,
    SubtaskStatus,
};
use zevria_host::RestoredSession;
use zevria_presentation::command::{ClassifiedInput, CommandRegistry, SlashCommand};
use zevria_presentation::{PaneScope, Reduction, RestorationInput, SessionModel};
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
pub struct WorkspaceModel {
    pub root: SessionModel,
    pub children: Vec<ChildPane>,
    pub question: Option<QuestionForm>,
    pub registry: CommandRegistry,
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub selected_child: Option<SubtaskId>,
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
    Notice(String),
    Retained,
}
impl Default for WorkspaceModel {
    fn default() -> Self {
        Self {
            root: SessionModel::default(),
            children: Vec::new(),
            question: None,
            registry: CommandRegistry::default(),
            session_id: String::new(),
            transcript_path: PathBuf::new(),
            selected_child: None,
        }
    }
}
impl WorkspaceModel {
    pub fn restore(restored: RestoredSession) -> anyhow::Result<Self> {
        let mut workspace = Self {
            session_id: restored.session_id,
            transcript_path: restored.transcript_path,
            ..Self::default()
        };
        let skills = zevria_instructions::skill::SkillContext {
            catalog: restored.skill_catalog,
            pins: restored.active_skills,
            mode_enabled: true,
        }
        .completions();
        workspace.registry = CommandRegistry::new(skills);
        let restored_launches = restored
            .transcript_items
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
        workspace.root.restore(RestorationInput {
            items: restored.transcript_items,
            workflow: restored.plan_state,
            selected_mode: restored.selected_mode,
            model_profiles: ModelRole::ALL
                .into_iter()
                .map(|role| (role, restored.model_contexts[role.index()].profile.clone()))
                .collect(),
            contexts: restored.model_snapshots,
            reasoning: restored.reasoning_levels,
            persistence_error: None,
        });
        for notice in restored.startup_notices {
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
        Ok(workspace)
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
                let root = batch
                    .root
                    .as_ref()
                    .map(|stream| self.root.apply_stream(stream));
                for (id, stream) in batch.subtasks {
                    self.child(&id).model.apply_stream(&stream);
                }
                WorkspaceReduction { root, fresh: None }
            }
            SessionUpdate::Lifecycle(SessionEvent::SubtaskSession { id, event }) => {
                self.child(&id).model.reduce(*event);
                WorkspaceReduction::default()
            }
            SessionUpdate::Lifecycle(mut event) => {
                let mut fresh = None;
                match &mut event {
                    SessionEvent::QuestionAsked { turn_id, request }
                        if self.root.accepts_progress(*turn_id) =>
                    {
                        self.question = Some(QuestionForm::new(*turn_id, request.clone()))
                    }
                    SessionEvent::QuestionClosed {
                        turn_id,
                        request_id,
                    } if self
                        .question
                        .as_ref()
                        .is_some_and(|q| q.turn_id == *turn_id && &q.request.id == request_id) =>
                    {
                        self.question = None
                    }
                    SessionEvent::SubtaskLaunched { descriptor, .. } => {
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
                    SessionEvent::SubtaskStatus { id, status, .. } => {
                        let child = self.child(id);
                        if child.status.is_none_or(|previous| {
                            !previous.is_terminal() && *status != SubtaskStatus::Starting
                        }) {
                            child.status = Some(*status);
                        }
                        *status = child.status.expect("child status installed");
                    }
                    SessionEvent::ToolResults { metadata, .. } => {
                        for entry in metadata.iter().flat_map(|m| m.subtasks()) {
                            if let Some(launch) = &entry.launch {
                                self.install_launch(launch, Some(entry.status));
                            }
                        }
                    }
                    SessionEvent::FreshPlanHandoffRequested { handoff } => {
                        fresh = Some(handoff.clone())
                    }
                    _ => {}
                }
                let terminal = match &event {
                    SessionEvent::TurnCompleted { turn_id, .. }
                    | SessionEvent::TurnRecovered { turn_id }
                    | SessionEvent::TurnFailed { turn_id, .. }
                    | SessionEvent::TurnRejected { turn_id, .. }
                    | SessionEvent::TurnCancelled { turn_id } => Some(*turn_id),
                    _ => None,
                }
                .filter(|id| self.root.accepts_terminal(*id));
                if terminal
                    .is_some_and(|id| self.question.as_ref().is_some_and(|q| q.turn_id == id))
                {
                    self.question = None;
                }
                let root = self.root.reduce(event);
                WorkspaceReduction {
                    root: Some(root),
                    fresh,
                }
            }
        }
    }
    pub fn visible(&self) -> &SessionModel {
        self.selected_child
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
    pub fn disconnected(&mut self) -> Reduction {
        self.question = None;
        self.root.disconnected()
    }
    pub fn route_input(&mut self, input: &str, orchestrate: bool) -> InputRoute {
        if self.root.is_busy() {
            return InputRoute::Retained;
        }
        let classified = match self
            .registry
            .classify_prompt(&zevria_content::UserPrompt::from_text(input))
        {
            Ok(value) => value,
            Err(error) => return InputRoute::Notice(error.to_string()),
        };
        let command = match classified {
            ClassifiedInput::Message(text) => self
                .root
                .submit(
                    text,
                    if orchestrate && self.root.next_mode() == SessionMode::Build {
                        RequestBehavior::Orchestrate
                    } else {
                        RequestBehavior::Standard
                    },
                )
                .map(SessionCommand::Turn),
            ClassifiedInput::Orchestrate(text) => {
                if self.root.next_mode() != SessionMode::Build {
                    return InputRoute::Notice(
                        "Orchestration is available only in Build mode; the draft was retained."
                            .into(),
                    );
                }
                self.root
                    .submit(text, RequestBehavior::Orchestrate)
                    .map(SessionCommand::Turn)
            }
            ClassifiedInput::Skill { name, args } => {
                self.root.invoke_skill(name, args).map(SessionCommand::Turn)
            }
            ClassifiedInput::Builtin(SlashCommand::Compact) => {
                self.root.compact().map(SessionCommand::Turn)
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
            ClassifiedInput::Builtin(SlashCommand::New) => return InputRoute::New,
            ClassifiedInput::Builtin(SlashCommand::Resume) => return InputRoute::Resume,
            ClassifiedInput::Builtin(_) | ClassifiedInput::Ensemble { .. } => {
                return InputRoute::Notice(
                    "This command is not available in the desktop yet; the draft was retained."
                        .into(),
                );
            }
        };
        command.map_or(InputRoute::Retained, |command| {
            InputRoute::Command(Box::new(command))
        })
    }
}
