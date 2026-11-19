use crate::{
    actions,
    cli::Cli,
    controller::SessionController,
    engine::{EngineRuntime, LOG_FILE, LogGuard},
    ui::{assets::AppAssets, theme, window::DesktopWindow},
};
use gpui_kit::{AppContext as _, *};
use std::{cell::Cell, rc::Rc};

#[cfg(windows)]
fn application() -> anyhow::Result<Application> {
    use anyhow::Context as _;
    // Kit's convenience wrapper panics on backend initialization failure.
    // Surface the same backend's error to the launcher instead.
    let platform = gpui_windows::WindowsPlatform::new(false)
        .context("Could not initialize the Windows desktop backend")?;
    Ok(Application::with_platform(Rc::new(platform)))
}

#[cfg(target_os = "macos")]
fn application() -> anyhow::Result<Application> {
    Ok(gpui_kit::application())
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let workspace = cli.workspace()?;
    let mut config = zevria_host::Config::load().map_err(|e| format!("{e:#}"));
    let mut guard = None;
    let mut tokens = theme::Selection::System;
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
    let launch_error = Rc::new(Cell::new(None));
    let window_error = launch_error.clone();
    application()?
        .with_assets(AppAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::select(tokens, cx);
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
                    traffic_light_position: if cfg!(target_os = "macos") {
                        Some(point(px(16.), px(13.)))
                    } else {
                        None
                    },
                    ..gpui_kit::component::TitleBar::title_bar_options()
                }),
                window_min_size: Some(size(px(720.), px(520.))),
                ..gpui_kit::component::TitleBar::window_options()
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
                    window_error.set(Some(error.context("Could not open desktop window")));
                    if cx.has_global::<LogGuard>() {
                        cx.global_mut::<LogGuard>().0.take();
                    }
                    cx.quit();
                }
            }
        });
    launch_error.take().map_or(Ok(()), Err)
}
