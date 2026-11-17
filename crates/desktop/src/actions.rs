use gpui_kit::{App, KeyBinding, Menu, MenuItem};
gpui_kit::actions!(
    zevria_desktop,
    [
        Quit,
        OpenWorkspace,
        NewSession,
        ToggleSidebar,
        Cancel,
        Stop,
        Submit,
        Complete,
        CompletionUp,
        CompletionDown,
        ExpandBlock,
        CollapseBlock,
        ExpandAllThinking,
        CollapseAllThinking,
        ToggleFinalOutputs,
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
pub fn install(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-o", OpenWorkspace, None),
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("ctrl-cmd-s", ToggleSidebar, Some("ZevriaDesktop")),
        KeyBinding::new("cmd-.", Stop, None),
        KeyBinding::new("alt-cmd-right", ExpandBlock, Some("ZevriaDesktop")),
        KeyBinding::new("alt-cmd-left", CollapseBlock, Some("ZevriaDesktop")),
        KeyBinding::new(
            "alt-shift-cmd-right",
            ExpandAllThinking,
            Some("ZevriaDesktop"),
        ),
        KeyBinding::new(
            "alt-shift-cmd-left",
            CollapseAllThinking,
            Some("ZevriaDesktop"),
        ),
        KeyBinding::new("shift-cmd-r", ToggleFinalOutputs, Some("ZevriaDesktop")),
        KeyBinding::new("alt-cmd-up", PreviousTurn, Some("ZevriaDesktop")),
        KeyBinding::new("alt-cmd-down", NextTurn, Some("ZevriaDesktop")),
        KeyBinding::new("alt-shift-cmd-up", JumpToBeginning, Some("ZevriaDesktop")),
        KeyBinding::new("alt-shift-cmd-down", JumpToLatest, Some("ZevriaDesktop")),
        // Kit's deeper Input context otherwise wins with add-cursor-above/below.
        // Register after Kit, as for the completion bindings below, to win ties.
        KeyBinding::new("alt-cmd-up", PreviousTurn, Some("ZevriaDesktop > Input")),
        KeyBinding::new("alt-cmd-down", NextTurn, Some("ZevriaDesktop > Input")),
        KeyBinding::new(
            "alt-shift-cmd-up",
            JumpToBeginning,
            Some("ZevriaDesktop > Input"),
        ),
        KeyBinding::new(
            "alt-shift-cmd-down",
            JumpToLatest,
            Some("ZevriaDesktop > Input"),
        ),
        KeyBinding::new("cmd-enter", ConfirmDecision, Some("ZevriaDesktop")),
        KeyBinding::new("shift-cmd-m", ToggleMode, Some("ZevriaDesktop")),
        KeyBinding::new("ctrl-cmd-m", OpenModels, Some("ZevriaDesktop")),
        KeyBinding::new("ctrl-cmd-m", OpenModels, Some("ZevriaDesktop > Input")),
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
        KeyBinding::new("escape", Cancel, Some("ZevriaDesktop")),
        KeyBinding::new("tab", Complete, Some("ZevriaCompletion > Input")),
        KeyBinding::new("up", CompletionUp, Some("ZevriaCompletion > Input")),
        KeyBinding::new("down", CompletionDown, Some("ZevriaCompletion > Input")),
    ]);
    cx.set_menus(menus(false));
}

pub fn menus(final_outputs: bool) -> Vec<Menu> {
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
            MenuItem::action("Toggle sidebar", ToggleSidebar),
            MenuItem::separator(),
            MenuItem::action("Expand block", ExpandBlock),
            MenuItem::action("Collapse block", CollapseBlock),
            MenuItem::action("Expand all thinking", ExpandAllThinking),
            MenuItem::action("Collapse all thinking", CollapseAllThinking),
            MenuItem::separator(),
            MenuItem::action("Show final outputs only", ToggleFinalOutputs).checked(final_outputs),
            MenuItem::separator(),
            MenuItem::action("Previous turn", PreviousTurn),
            MenuItem::action("Next turn", NextTurn),
            MenuItem::action("Jump to beginning", JumpToBeginning),
            MenuItem::action("Jump to latest", JumpToLatest),
        ]),
    ]
}
