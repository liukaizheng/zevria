use crate::{
    actions,
    cli::Cli,
    controller::SessionController,
    engine::{EngineRuntime, LOG_FILE, LogGuard},
    ui::{theme, window::DesktopWindow},
};
use gpui_kit::{AppContext as _, *};

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let workspace = cli.workspace()?;
    let mut config = zevria_host::Config::load().map_err(|e| format!("{e:#}"));
    let mut guard = None;
    let mut tokens = zevria_theme::ZEVRIA_DARK;
    if let Ok(loaded) = &config {
        match zevria_foundation::logging::init_named(loaded.log(), LOG_FILE).and_then(|log| {
            guard = Some(log);
            theme::prepare(loaded)
        }) {
            Ok(theme) => tokens = theme,
            Err(error) => config = Err(format!("{error:#}")),
        }
    }
    let runtime = EngineRuntime::new()?;
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::apply(tokens, cx);
            actions::install(cx);
            cx.set_global(runtime);
            if let Some(guard) = guard {
                cx.set_global(LogGuard(Some(guard)));
            }
            let controller = cx.new(|cx| SessionController::new(config, workspace, cx));
            let weak = controller.downgrade();
            cx.on_action(move |_: &actions::Quit, cx| {
                let _ = weak.update(cx, |this, cx| this.quit(cx));
            });
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1240.), px(840.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Zevria".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            match gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| DesktopWindow::new(controller.clone(), window, cx))
            }) {
                Ok(_) => {
                    controller.update(cx, |this, cx| this.start_initial(cli.continue_session, cx));
                    cx.activate(true);
                }
                Err(error) => {
                    tracing::error!("Could not open desktop window: {error:#}");
                    if cx.has_global::<LogGuard>() {
                        cx.global_mut::<LogGuard>().0.take();
                    }
                    cx.quit();
                }
            }
        });
    Ok(())
}
