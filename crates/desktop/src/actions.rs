use gpui_kit::{Action, App, KeyBinding, Keystroke, Menu, MenuItem, component::kbd::Kbd};
gpui_kit::actions!(
    zevria_desktop,
    [
        Quit,
        OpenWorkspace,
        NewSession,
        ToggleSidebar,
        ToggleJobs,
        ToggleSubtasks,
        ToggleSideChat,
        NewSideChat,
        Cancel,
        Stop,
        Submit,
        Complete,
        CompletionUp,
        CompletionDown,
        ToggleFold,
        Fold,
        Unfold,
        ExpandAllThinking,
        CollapseAllThinking,
        CollapseAll,
        ExpandAll,
        PreviousTurn,
        NextTurn,
        JumpToBeginning,
        JumpToLatest,
        ConfirmDecision,
        ToggleMode,
        OpenModels,
        ModelPrevious,
        ModelNext,
        ModelAccept,
        ModelBack,
        ModelFocusNext,
        ModelFocusPrevious
    ]
);
/// The same keys drive GPUI registration and rendered shortcut hints.
#[derive(Clone, Copy)]
pub(crate) struct Shortcut(pub(crate) &'static str);

impl Shortcut {
    const fn platform(macos: &'static str, windows: &'static str) -> Self {
        Self(if cfg!(target_os = "macos") {
            macos
        } else {
            windows
        })
    }

    pub const OPEN_WORKSPACE: Self = Self::platform("cmd-o", "ctrl-o");
    pub const NEW_SESSION: Self = Self::platform("cmd-n", "ctrl-n");
    pub const QUIT: Self = Self::platform("cmd-q", "ctrl-q");
    pub const TOGGLE_SIDEBAR: Self = Self::platform("ctrl-cmd-s", "ctrl-shift-s");
    pub const TOGGLE_JOBS: Self = Self::platform("shift-cmd-j", "ctrl-shift-y");
    pub const TOGGLE_SIDE_CHAT: Self = Self::platform("shift-cmd-s", "ctrl-shift-l");
    pub const TOGGLE_SUBTASKS: Self = Self::platform("shift-cmd-t", "ctrl-shift-t");
    pub const TOGGLE_MODE: Self = Self::platform("shift-cmd-m", "ctrl-shift-m");
    pub const OPEN_MODELS: Self = Self::platform("ctrl-cmd-m", "ctrl-shift-p");
    pub const STOP: Self = Self::platform("cmd-.", "ctrl-.");
    pub const ESCAPE: Self = Self("escape");
    pub const CONFIRM: Self = Self::platform("cmd-enter", "ctrl-enter");
    pub const COPY_DISCLOSURE: Self = Self::platform("cmd-c", "ctrl-c");
    pub const PREVIOUS_TURN: Self = Self::platform("alt-cmd-up", "ctrl-k");
    pub const NEXT_TURN: Self = Self::platform("alt-cmd-down", "ctrl-j");
    pub const JUMP_TO_BEGINNING: Self = Self::platform("alt-shift-cmd-up", "ctrl-shift-k");
    pub const JUMP_TO_LATEST: Self = Self::platform("alt-shift-cmd-down", "ctrl-shift-j");
    pub const FOLD: Self = Self::platform("alt-cmd-left", "ctrl-shift-f");
    pub const UNFOLD: Self = Self::platform("alt-cmd-right", "ctrl-shift-u");
    pub const COLLAPSE_THINKING: Self = Self::platform("alt-shift-cmd-left", "ctrl-shift-h");
    pub const EXPAND_THINKING: Self = Self::platform("alt-shift-cmd-right", "ctrl-shift-e");
    pub const COLLAPSE_ALL: Self = Self::platform("shift-cmd-r", "ctrl-shift-r");
    pub const EXPAND_ALL: Self = Self::platform("alt-cmd-r", "ctrl-shift-a");

    // Display Kit's existing textarea bindings without re-registering them.
    pub const ENTER: Self = Self("enter");
    pub const SHIFT_ENTER: Self = Self("shift-enter");

    pub fn keystroke(self) -> Keystroke {
        Keystroke::parse(self.0).expect("static desktop shortcut")
    }

    pub fn label(self) -> String {
        Kbd::format(&self.keystroke())
    }

    fn bind(self, action: impl Action, context: Option<&str>) -> KeyBinding {
        KeyBinding::new(self.0, action, context)
    }
}

fn bindings() -> impl IntoIterator<Item = KeyBinding> {
    let desktop = Some("ZevriaDesktop");
    let input = Some("ZevriaDesktop > Input");
    let transcript = Some("ZevriaTranscript");
    [
        Shortcut::QUIT.bind(Quit, None),
        Shortcut::OPEN_WORKSPACE.bind(OpenWorkspace, None),
        Shortcut::NEW_SESSION.bind(NewSession, None),
        Shortcut::TOGGLE_SIDEBAR.bind(ToggleSidebar, desktop),
        Shortcut::TOGGLE_JOBS.bind(ToggleJobs, desktop),
        Shortcut::TOGGLE_JOBS.bind(ToggleJobs, input),
        KeyBinding::new("escape", Cancel, Some("ZevriaJobs")),
        Shortcut::TOGGLE_SIDE_CHAT.bind(ToggleSideChat, desktop),
        Shortcut::TOGGLE_SIDE_CHAT.bind(ToggleSideChat, input),
        Shortcut::TOGGLE_SUBTASKS.bind(ToggleSubtasks, desktop),
        Shortcut::TOGGLE_SUBTASKS.bind(ToggleSubtasks, input),
        KeyBinding::new("escape", Cancel, Some("ZevriaSubtasks")),
        Shortcut::STOP.bind(Stop, None),
        Shortcut::UNFOLD.bind(Unfold, transcript),
        Shortcut::FOLD.bind(Fold, transcript),
        Shortcut::COPY_DISCLOSURE.bind(gpui_kit::component::input::Copy, Some("ZevriaDisclosure")),
        Shortcut::EXPAND_THINKING.bind(ExpandAllThinking, desktop),
        Shortcut::COLLAPSE_THINKING.bind(CollapseAllThinking, desktop),
        Shortcut::COLLAPSE_ALL.bind(CollapseAll, desktop),
        Shortcut::EXPAND_ALL.bind(ExpandAll, desktop),
        Shortcut::PREVIOUS_TURN.bind(PreviousTurn, desktop),
        Shortcut::NEXT_TURN.bind(NextTurn, desktop),
        Shortcut::JUMP_TO_BEGINNING.bind(JumpToBeginning, desktop),
        Shortcut::JUMP_TO_LATEST.bind(JumpToLatest, desktop),
        // Kit's deeper Input context otherwise wins with add-cursor-above/below
        // on macOS. Register after Kit, as with the completion bindings below.
        Shortcut::PREVIOUS_TURN.bind(PreviousTurn, input),
        Shortcut::NEXT_TURN.bind(NextTurn, input),
        Shortcut::JUMP_TO_BEGINNING.bind(JumpToBeginning, input),
        Shortcut::JUMP_TO_LATEST.bind(JumpToLatest, input),
        Shortcut::CONFIRM.bind(ConfirmDecision, desktop),
        // On Windows, Kit binds Ctrl+Enter to secondary Enter in Input. Dispatch
        // the decision action instead, even with a question editor focused. The
        // handler only confirms a visible decision and never submits a draft.
        #[cfg(windows)]
        Shortcut::CONFIRM.bind(ConfirmDecision, input),
        Shortcut::TOGGLE_MODE.bind(ToggleMode, desktop),
        Shortcut::OPEN_MODELS.bind(OpenModels, desktop),
        Shortcut::OPEN_MODELS.bind(OpenModels, input),
        KeyBinding::new("alt-left", ModelBack, Some("ZevriaModels")),
        KeyBinding::new("up", ModelPrevious, Some("ZevriaModels > Input")),
        KeyBinding::new("down", ModelNext, Some("ZevriaModels > Input")),
        KeyBinding::new("enter", ModelAccept, Some("ZevriaModels > Input")),
        KeyBinding::new("escape", Cancel, Some("ZevriaModels > Input")),
        KeyBinding::new("tab", ModelFocusNext, Some("ZevriaModels")),
        KeyBinding::new("shift-tab", ModelFocusPrevious, Some("ZevriaModels")),
        KeyBinding::new("tab", ModelFocusNext, Some("ZevriaModels > Input")),
        KeyBinding::new(
            "shift-tab",
            ModelFocusPrevious,
            Some("ZevriaModels > Input"),
        ),
        Shortcut::ESCAPE.bind(Cancel, desktop),
        KeyBinding::new("tab", Complete, Some("ZevriaCompletion > Input")),
        KeyBinding::new("up", CompletionUp, Some("ZevriaCompletion > Input")),
        KeyBinding::new("down", CompletionDown, Some("ZevriaCompletion > Input")),
    ]
}

pub fn install(cx: &mut App) {
    cx.bind_keys(bindings());
    #[cfg(target_os = "macos")]
    cx.set_menus(menus());
    // GPUI stores app menus on Windows but there is no system menu bar. Kit
    // renders these same actions in the window, dispatching to the prior focus.
    #[cfg(windows)]
    gpui_kit::base::GlobalState::global_mut(cx)
        .set_app_menus(menus().into_iter().map(Menu::owned).collect());
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use gpui_kit::AsKeystroke as _;

    #[test]
    fn registered_windows_shortcuts_avoid_fn_and_reserved_modifiers() {
        for binding in bindings() {
            for stroke in binding.keystrokes() {
                let key = stroke.as_keystroke();
                let function_key = key
                    .key
                    .strip_prefix('f')
                    .is_some_and(|n| n.parse::<u8>().is_ok());
                assert!(!function_key, "{binding:?}");
                assert!(
                    !matches!(key.key.as_str(), "pageup" | "pagedown" | "home" | "end"),
                    "{binding:?}"
                );
                assert!(!key.modifiers.platform, "{binding:?}");
                assert!(!(key.modifiers.control && key.modifiers.alt), "{binding:?}");
            }
        }
    }

    #[test]
    fn windows_labels_match_the_registered_keys() {
        for (shortcut, label) in [
            (Shortcut::QUIT, "Ctrl+Q"),
            (Shortcut::OPEN_WORKSPACE, "Ctrl+O"),
            (Shortcut::NEW_SESSION, "Ctrl+N"),
            (Shortcut::TOGGLE_SIDEBAR, "Ctrl+Shift+S"),
            (Shortcut::TOGGLE_JOBS, "Ctrl+Shift+Y"),
            (Shortcut::TOGGLE_SUBTASKS, "Ctrl+Shift+T"),
            (Shortcut::TOGGLE_SIDE_CHAT, "Ctrl+Shift+L"),
            (Shortcut::TOGGLE_MODE, "Ctrl+Shift+M"),
            (Shortcut::OPEN_MODELS, "Ctrl+Shift+P"),
            (Shortcut::STOP, "Ctrl+."),
            (Shortcut::CONFIRM, "Ctrl+Enter"),
            (Shortcut::COPY_DISCLOSURE, "Ctrl+C"),
            (Shortcut::PREVIOUS_TURN, "Ctrl+K"),
            (Shortcut::NEXT_TURN, "Ctrl+J"),
            (Shortcut::JUMP_TO_BEGINNING, "Ctrl+Shift+K"),
            (Shortcut::JUMP_TO_LATEST, "Ctrl+Shift+J"),
            (Shortcut::FOLD, "Ctrl+Shift+F"),
            (Shortcut::UNFOLD, "Ctrl+Shift+U"),
            (Shortcut::COLLAPSE_THINKING, "Ctrl+Shift+H"),
            (Shortcut::EXPAND_THINKING, "Ctrl+Shift+E"),
            (Shortcut::COLLAPSE_ALL, "Ctrl+Shift+R"),
            (Shortcut::EXPAND_ALL, "Ctrl+Shift+A"),
        ] {
            assert_eq!(shortcut.label(), label);
            assert!(
                bindings().into_iter().any(|binding| {
                    binding.keystrokes()[0].as_keystroke() == &shortcut.keystroke()
                }),
                "{label} was not bound"
            );
        }
    }
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Zevria").items([MenuItem::action("Quit Zevria", Quit)]),
        Menu::new("File").items([
            MenuItem::action("Open workspace…", OpenWorkspace),
            MenuItem::action("New session", NewSession),
        ]),
        Menu::new("Session").items([
            MenuItem::action("Switch next-turn mode", ToggleMode),
            MenuItem::action("Change model and thinking effort…", OpenModels),
            MenuItem::action("Confirm decision", ConfirmDecision),
            MenuItem::action("Stop", Stop),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Side Chat panel", ToggleSideChat),
            MenuItem::action("New Side Chat", NewSideChat),
            MenuItem::action("Toggle sidebar", ToggleSidebar),
            MenuItem::action("Toggle jobs panel", ToggleJobs),
            MenuItem::action("Toggle subtasks panel", ToggleSubtasks),
            MenuItem::separator(),
            MenuItem::action("Toggle fold", ToggleFold),
            MenuItem::action("Fold", Fold),
            MenuItem::action("Unfold", Unfold),
            MenuItem::action("Expand all thinking", ExpandAllThinking),
            MenuItem::action("Collapse all thinking", CollapseAllThinking),
            MenuItem::separator(),
            MenuItem::action("Collapse details", CollapseAll),
            MenuItem::action("Expand details", ExpandAll),
            MenuItem::separator(),
            MenuItem::action("Previous turn", PreviousTurn),
            MenuItem::action("Next turn", NextTurn),
            MenuItem::action("Jump to beginning", JumpToBeginning),
            MenuItem::action("Jump to latest", JumpToLatest),
        ]),
    ]
}
