use gpui_kit::{App, KeyBinding, Menu, MenuItem};
gpui_kit::actions!(
    zevria_desktop,
    [
        Quit,
        OpenWorkspace,
        NewSession,
        ToggleSidebar,
        Cancel,
        Submit,
        Complete,
        CompletionUp,
        CompletionDown
    ]
);
pub fn install(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-o", OpenWorkspace, None),
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("ctrl-cmd-s", ToggleSidebar, Some("ZevriaDesktop")),
        KeyBinding::new("cmd-.", Cancel, None),
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
        Menu::new("View").items([MenuItem::action("Toggle Sidebar", ToggleSidebar)]),
    ]);
}
