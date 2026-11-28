//! Typed capturing surfaces. Suspended dialogs retain their state beneath help
//! or engine questions; paint order and input ownership use the same stack.
use crate::{
    app::UiAction,
    hints::{Eligibility, HelpOverlay},
    input::{Action, ChordState, KeyContext, Surface, SurfaceKind, UserInput},
    models::ModelPicker,
    picker::{PickerAction, SessionPicker},
    question::{QuestionDialog, QuestionDialogAction},
    skills::SkillManager,
    subtasks::{SubtaskRow, SubtasksOverlay},
};
use ratatui::{Frame, layout::Rect};

pub(crate) enum Overlay {
    Picker(SessionPicker),
    Models,
    Skills,
    Jobs,
    Mcp,
    Subtasks,
    SideChats(crate::side_chats::SideChatsOverlay),
    Question(QuestionDialog),
    Help(HelpOverlay),
}
impl Overlay {
    fn kind(&self) -> SurfaceKind {
        match self {
            Self::Picker(_) => SurfaceKind::Sessions,
            Self::Models => SurfaceKind::Models,
            Self::Skills => SurfaceKind::Skills,
            Self::Jobs => SurfaceKind::Jobs,
            Self::Mcp => SurfaceKind::Mcp,
            Self::Subtasks => SurfaceKind::Subtasks,
            Self::SideChats(_) => SurfaceKind::SideChats,
            Self::Question(_) => SurfaceKind::Question,
            Self::Help(_) => SurfaceKind::Help,
        }
    }
}

pub(crate) struct SurfaceSnapshot {
    pub owner: Surface,
    layers: Vec<Surface>,
}
#[derive(Default)]
pub(crate) struct OverlayController {
    stack: Vec<Overlay>,
    // Correlated management requests live beyond the modal that initiated them.
    pub(crate) models: ModelPicker,
    pub(crate) skills: SkillManager,
    pub(crate) mcp: crate::mcp::McpOverlay,
    pub(crate) jobs: crate::jobs::JobsOverlay,
    pub(crate) subtasks: SubtasksOverlay,
}
impl OverlayController {
    fn replace(&mut self, overlay: Overlay) {
        // Jobs and subtasks share one slot; opening one replaces the other.
        self.stack
            .retain(|entry| entry.kind().layer() != overlay.kind().layer());
        self.stack.push(overlay);
        self.stack.sort_by_key(|entry| entry.kind().layer());
    }
    pub(crate) fn show_sessions(&mut self, picker: SessionPicker) {
        self.replace(Overlay::Picker(picker));
    }
    pub(crate) fn show_models(
        &mut self,
        mode: zevria_foundation::SessionMode,
        scope: zevria_model::models::ModelSelectionScope,
    ) {
        self.models.show(mode, scope);
        self.replace(Overlay::Models);
    }
    pub(crate) fn show_jobs(&mut self, inspector: &mut zevria_presentation::jobs::JobsInspector) {
        self.subtasks.close();
        self.jobs.show(inspector);
        self.replace(Overlay::Jobs);
    }
    pub(crate) fn show_subtasks(&mut self, rows: Vec<SubtaskRow>) {
        self.subtasks.show(rows);
        self.replace(Overlay::Subtasks);
    }
    pub(crate) fn show_side_chats(&mut self, rows: Vec<zevria_foundation::SideChatDescriptor>) {
        self.replace(Overlay::SideChats(
            crate::side_chats::SideChatsOverlay::new(rows),
        ));
    }
    pub(crate) fn update_side_chats(&mut self, rows: Vec<zevria_foundation::SideChatDescriptor>) {
        if let Some(Overlay::SideChats(side)) = self
            .stack
            .iter_mut()
            .find(|entry| matches!(entry, Overlay::SideChats(_)))
        {
            side.replace_rows(rows);
        }
    }
    pub(crate) fn close_side_chats(&mut self) {
        self.stack
            .retain(|entry| !matches!(entry, Overlay::SideChats(_)));
    }
    pub(crate) fn show_mcp(&mut self) {
        self.mcp.show();
        self.replace(Overlay::Mcp);
    }
    pub(crate) fn show_skills(&mut self) {
        self.skills.show();
        self.replace(Overlay::Skills);
    }
    pub(crate) fn question(&self) -> Option<&QuestionDialog> {
        self.stack.iter().find_map(|entry| {
            if let Overlay::Question(question) = entry {
                Some(question)
            } else {
                None
            }
        })
    }
    pub(crate) fn set_question(&mut self, question: Option<QuestionDialog>) {
        self.stack
            .retain(|entry| !matches!(entry, Overlay::Question(_) | Overlay::Help(_)));
        if let Some(question) = question {
            self.replace(Overlay::Question(question));
        }
    }
    pub(crate) fn show_help(&mut self, context: KeyContext, eligibility: &Eligibility) {
        self.replace(Overlay::Help(HelpOverlay::new(context, eligibility)));
    }
    fn visible(&self, overlay: &Overlay, jobs_open: bool) -> bool {
        match overlay {
            Overlay::Models => self.models.is_open(),
            Overlay::Skills => self.skills.is_open(),
            Overlay::Jobs => jobs_open,
            Overlay::Mcp => self.mcp.inspector.open,
            Overlay::Subtasks => self.subtasks.is_open(),
            _ => true,
        }
    }
    pub(crate) fn owns(&self, kind: SurfaceKind, jobs_open: bool) -> bool {
        self.stack
            .iter()
            .any(|entry| entry.kind() == kind && self.visible(entry, jobs_open))
    }

    pub(crate) fn snapshot(&self, pane: Surface, jobs_open: bool) -> SurfaceSnapshot {
        let mut layers = vec![pane];
        layers.extend(
            self.stack
                .iter()
                .filter(|entry| self.visible(entry, jobs_open))
                .map(|entry| Surface::new(pane.id.pane, entry.kind())),
        );
        layers.sort_by_key(|surface| surface.layer);
        SurfaceSnapshot {
            owner: crate::input::resolve(layers.iter().copied()),
            layers,
        }
    }
    pub(crate) fn invalidate_geometry(&mut self) {
        self.models.invalidate_geometry();
        self.skills.invalidate_geometry();
        self.jobs.invalidate_geometry();
        self.subtasks.invalidate_geometry();
        for entry in &mut self.stack {
            match entry {
                Overlay::Picker(picker) => picker.invalidate_geometry(),
                Overlay::SideChats(side) => side.invalidate_geometry(),
                Overlay::Question(question) => question.invalidate_geometry(),
                Overlay::Help(help) => help.invalidate_geometry(),
                _ => {}
            }
        }
    }
    pub(crate) fn render(
        &mut self,
        snapshot: &SurfaceSnapshot,
        jobs: &zevria_presentation::jobs::JobsInspector,
        frame: &mut Frame,
        bounds: Rect,
    ) {
        for surface in &snapshot.layers {
            let Some(entry) = self
                .stack
                .iter_mut()
                .find(|entry| entry.kind() == surface.id.kind)
            else {
                continue;
            };
            match entry {
                Overlay::Picker(picker) => picker.render(frame, bounds),
                Overlay::SideChats(side) => side.render(frame, bounds),
                Overlay::Models => self.models.render(frame, bounds),
                Overlay::Skills => self.skills.render(frame, bounds),
                Overlay::Jobs => self.jobs.render(jobs, frame, bounds),
                Overlay::Mcp => self.mcp.render(frame, bounds),
                Overlay::Subtasks => self.subtasks.render(frame, bounds),
                Overlay::Question(question) => question.render(frame, bounds),
                Overlay::Help(help) => help.render(frame, bounds),
            }
        }
    }
    pub(crate) fn handle_input(
        &mut self,
        owner: Surface,
        input: UserInput,
        jobs: &mut zevria_presentation::jobs::JobsInspector,
        busy: bool,
        mode: zevria_foundation::SessionMode,
    ) -> Option<UiAction> {
        let index = self
            .stack
            .iter()
            .position(|entry| entry.kind() == owner.id.kind)
            .expect("capturing overlay owner");
        let context = match &self.stack[index] {
            Overlay::Models => self.models.context(),
            Overlay::Skills => self.skills.context(),
            Overlay::Jobs => self.jobs.context(jobs),
            Overlay::Mcp => KeyContext::OverlayList,
            Overlay::Subtasks => KeyContext::SubtasksList,
            Overlay::SideChats(_) => KeyContext::SideChatsList,
            Overlay::Question(question) => question.context(),
            Overlay::Picker(_) => KeyContext::OverlayList,
            Overlay::Help(_) => KeyContext::Help,
        };
        if !matches!(self.stack[index], Overlay::Help(_) | Overlay::Mcp)
            && let UserInput::Key(key) = &input
            && ChordState::default().resolve(context, *key) == Some(Action::Help)
        {
            let eligibility = match &self.stack[index] {
                Overlay::Question(question) => question.hint_eligibility(),
                Overlay::Models => self.models.hint_eligibility(),
                Overlay::Skills => self.skills.hint_eligibility(),
                Overlay::Jobs => self.jobs.hint_eligibility(jobs),
                Overlay::Subtasks => self.subtasks.hint_eligibility(),
                Overlay::SideChats(side) => side.hint_eligibility(),
                Overlay::Picker(picker) => picker.hint_eligibility(),
                Overlay::Help(_) | Overlay::Mcp => unreachable!("help is not opened here"),
            };
            self.show_help(context, &eligibility);
            return None;
        }
        let mut close = false;
        let action = match &mut self.stack[index] {
            Overlay::Help(help) => {
                if let UserInput::Key(key) = input {
                    close = help.handle_input(key);
                }
                None
            }
            Overlay::Question(question) => {
                let action = match input {
                    UserInput::Key(key) => question.handle_key(key.code, key.modifiers),
                    UserInput::Paste(text) => {
                        question.paste(&text);
                        QuestionDialogAction::Handled
                    }
                };
                match action {
                    QuestionDialogAction::Handled => None,
                    QuestionDialogAction::Respond {
                        request_id,
                        response,
                    } => {
                        close = true;
                        Some(UiAction::Session(
                            zevria_session_api::SessionCommand::Control(
                                zevria_session_api::ControlCommand::AnswerQuestion {
                                    request_id,
                                    response,
                                },
                            ),
                        ))
                    }
                }
            }
            Overlay::Models => {
                match input {
                    UserInput::Key(key) => self.models.handle_input(key),
                    UserInput::Paste(text) => self.models.paste(&text),
                }
                None
            }
            Overlay::Mcp => {
                let action = self.mcp.input(input, busy, mode).map(|command| {
                    UiAction::Session(zevria_session_api::SessionCommand::Turn(command))
                });
                close = !self.mcp.inspector.open;
                action
            }
            Overlay::Jobs => {
                if let UserInput::Key(key) = input {
                    self.jobs.handle_input(jobs, key);
                    close = !jobs.is_open();
                }
                None
            }
            Overlay::Subtasks => {
                let action = match input {
                    UserInput::Key(key) => self
                        .subtasks
                        .handle_input(key)
                        .map(|id| UiAction::OpenSubtask { id }),
                    UserInput::Paste(_) => None,
                };
                close = !self.subtasks.is_open();
                action
            }
            Overlay::SideChats(side) => match input {
                UserInput::Key(key) => {
                    let (closed, action) = side.handle_key(key);
                    close = closed;
                    action
                }
                UserInput::Paste(_) => None,
            },
            Overlay::Skills => {
                match input {
                    UserInput::Key(key) => self.skills.handle_input(key),
                    UserInput::Paste(text) => self.skills.paste(&text),
                }
                None
            }
            Overlay::Picker(picker) => match input {
                UserInput::Paste(_) => None,
                UserInput::Key(key) => match picker.handle_input(key) {
                    PickerAction::Handled => None,
                    PickerAction::Close => {
                        close = true;
                        None
                    }
                    PickerAction::Choose(path) => {
                        close = true;
                        Some(UiAction::ResumeSession { path })
                    }
                },
            },
        };
        if close {
            self.stack.remove(index);
        }
        action
    }
}
