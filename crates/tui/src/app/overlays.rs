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
};
use ratatui::{Frame, layout::Rect};

pub(crate) enum Overlay {
    Picker(SessionPicker),
    Models,
    Skills,
    Question(QuestionDialog),
    Help(HelpOverlay),
}
impl Overlay {
    fn kind(&self) -> SurfaceKind {
        match self {
            Self::Picker(_) => SurfaceKind::Sessions,
            Self::Models => SurfaceKind::Models,
            Self::Skills => SurfaceKind::Skills,
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
}
impl OverlayController {
    fn replace(&mut self, overlay: Overlay) {
        self.stack.retain(|entry| entry.kind() != overlay.kind());
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
    fn visible(&self, overlay: &Overlay) -> bool {
        match overlay {
            Overlay::Models => self.models.is_open(),
            Overlay::Skills => self.skills.is_open(),
            _ => true,
        }
    }
    pub(crate) fn owns(&self, kind: SurfaceKind) -> bool {
        self.stack
            .iter()
            .any(|entry| entry.kind() == kind && self.visible(entry))
    }

    pub(crate) fn snapshot(&self, pane: Surface) -> SurfaceSnapshot {
        let mut layers = vec![pane];
        layers.extend(
            self.stack
                .iter()
                .filter(|entry| self.visible(entry))
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
        for entry in &mut self.stack {
            match entry {
                Overlay::Picker(picker) => picker.invalidate_geometry(),
                Overlay::Question(question) => question.invalidate_geometry(),
                Overlay::Help(help) => help.invalidate_geometry(),
                _ => {}
            }
        }
    }
    pub(crate) fn render(&mut self, snapshot: &SurfaceSnapshot, frame: &mut Frame, bounds: Rect) {
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
                Overlay::Models => self.models.render(frame, bounds),
                Overlay::Skills => self.skills.render(frame, bounds),
                Overlay::Question(question) => question.render(frame, bounds),
                Overlay::Help(help) => help.render(frame, bounds),
            }
        }
    }
    pub(crate) fn handle_input(&mut self, owner: Surface, input: UserInput) -> Option<UiAction> {
        let index = self
            .stack
            .iter()
            .position(|entry| entry.kind() == owner.id.kind)
            .expect("capturing overlay owner");
        let context = match &self.stack[index] {
            Overlay::Models => self.models.context(),
            Overlay::Skills => self.skills.context(),
            Overlay::Question(question) => question.context(),
            Overlay::Picker(_) => KeyContext::OverlayList,
            Overlay::Help(_) => KeyContext::Help,
        };
        if !matches!(self.stack[index], Overlay::Help(_))
            && let UserInput::Key(key) = &input
            && ChordState::default().resolve(context, *key) == Some(Action::Help)
        {
            let eligibility = match &self.stack[index] {
                Overlay::Question(question) => question.hint_eligibility(),
                Overlay::Models => self.models.hint_eligibility(),
                Overlay::Skills => self.skills.hint_eligibility(),
                Overlay::Picker(picker) => picker.hint_eligibility(),
                Overlay::Help(_) => unreachable!("help closes rather than nesting"),
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
                        Some(UiAction::AnswerQuestion {
                            request_id,
                            response,
                        })
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
