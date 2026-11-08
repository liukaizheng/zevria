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
        JumpToLatest,
        ConfirmDecision,
        ToggleMode
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
        KeyBinding::new("alt-cmd-down", JumpToLatest, Some("ZevriaDesktop")),
        KeyBinding::new("cmd-enter", ConfirmDecision, Some("ZevriaDesktop")),
        KeyBinding::new("shift-cmd-m", ToggleMode, Some("ZevriaDesktop")),
        KeyBinding::new("escape", Cancel, Some("ZevriaDesktop")),
        KeyBinding::new("tab", Complete, Some("ZevriaCompletion > Input")),
        KeyBinding::new("up", CompletionUp, Some("ZevriaCompletion > Input")),
        KeyBinding::new("down", CompletionDown, Some("ZevriaCompletion > Input")),
    ]);
    cx.set_menus([
        Menu::new("Zevria").items([MenuItem::action("Quit Zevria", Quit)]),
        Menu::new("File").items([
            MenuItem::action("Open workspace…", OpenWorkspace),
            MenuItem::action("New session", NewSession),
        ]),
        Menu::new("Session").items([
            MenuItem::action("Switch next-turn mode", ToggleMode),
            MenuItem::action("Confirm decision", ConfirmDecision),
            MenuItem::action("Stop", Stop),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle sidebar", ToggleSidebar),
            MenuItem::action("Expand block", ExpandBlock),
            MenuItem::action("Collapse block", CollapseBlock),
            MenuItem::action("Expand all thinking", ExpandAllThinking),
            MenuItem::action("Collapse all thinking", CollapseAllThinking),
            MenuItem::action("Jump to latest", JumpToLatest),
        ]),
    ]);
}
