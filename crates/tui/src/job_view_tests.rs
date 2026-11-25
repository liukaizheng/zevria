use super::*;
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyModifiers},
};
use std::time::{Duration, SystemTime};
use zevria_foundation::{CommandJobId, CommandJobMode, CommandJobStatus, CommandJobView};
use zevria_presentation::SessionModel;
use zevria_session_api::{
    CommandControlAction, CommandControlResult, ControlCommand, SessionCommand,
};
use zevria_session_api::{JobEvent, SessionEvent};

fn snapshot(mode: CommandJobMode) -> CommandJobSnapshot {
    CommandJobSnapshot {
        id: CommandJobId::new("job-inspect"),
        call_id: None,
        command: "rtk proxy cargo\n test".into(),
        mode,
        status: CommandJobStatus::Running,
        started_at: SystemTime::now() - Duration::from_millis(1234),
        finished_at: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

fn reply(
    overlay: &mut JobsOverlay,
    model: &mut SessionModel,
    snapshot: CommandJobSnapshot,
    stdout: String,
    stderr: String,
) {
    let Some(SessionCommand::Control(ControlCommand::Command(request))) =
        model.jobs_inspector_mut().commands.pop_front()
    else {
        panic!("expected an inspect request");
    };
    assert!(matches!(
        request.action,
        CommandControlAction::Inspect { .. }
    ));
    let anchor = overlay.selection_anchor(model.jobs_inspector());
    model.reduce(SessionEvent::Job(JobEvent::ControlResult {
        request_id: request.request_id,
        result: Box::new(CommandControlResult::Inspected(vec![CommandJobView {
            snapshot,
            stdout_tail: stdout,
            stderr_tail: stderr,
        }])),
    }));
    overlay.reconcile(model.jobs_inspector(), anchor);
}

fn press(overlay: &mut JobsOverlay, model: &mut SessionModel, key: KeyCode) {
    overlay.handle_input(
        model.jobs_inspector_mut(),
        KeyEvent::new(key, KeyModifiers::NONE),
    );
}

fn render(
    overlay: &mut JobsOverlay,
    model: &SessionModel,
    width: u16,
    height: u16,
) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| overlay.render(model.jobs_inspector(), frame, frame.area()))
        .unwrap();
    terminal
}

fn text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn inspection_follows_the_tail_until_panned_off_the_end() {
    let mut overlay = JobsOverlay::default();
    let mut model = SessionModel::default();
    let job = snapshot(CommandJobMode::Background);
    let numbered = |count| {
        (0..count)
            .map(|n| format!("line {n:03}\n"))
            .collect::<String>()
    };
    overlay.show(model.jobs_inspector_mut());
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        String::new(),
        String::new(),
    );
    press(&mut overlay, &mut model, KeyCode::Enter);
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        numbered(50),
        String::new(),
    );
    let initial = text(&render(&mut overlay, &model, 100, 12));
    assert!(initial.contains("line 049") && !initial.contains("line 000"));
    assert!(initial.contains("job-inspect"));
    assert_eq!(overlay.nav.viewport.top(), overlay.nav.viewport.max_top());

    let now = Instant::now();
    model
        .jobs_inspector_mut()
        .tick(now + Duration::from_secs(2));
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        numbered(60),
        String::new(),
    );
    assert!(text(&render(&mut overlay, &model, 100, 12)).contains("line 059"));
    press(&mut overlay, &mut model, KeyCode::Up);
    assert!(!overlay.follow);
    let top = overlay.nav.viewport.top();
    model
        .jobs_inspector_mut()
        .tick(now + Duration::from_secs(4));
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        numbered(70),
        String::new(),
    );
    assert!(!text(&render(&mut overlay, &model, 100, 12)).contains("line 069"));
    assert_eq!(overlay.nav.viewport.top(), top);
    press(&mut overlay, &mut model, KeyCode::End);
    assert!(text(&render(&mut overlay, &model, 100, 12)).contains("line 069"));
    press(&mut overlay, &mut model, KeyCode::Up);
    press(&mut overlay, &mut model, KeyCode::Down);
    assert!(overlay.follow, "reaching the end resumes following");
    press(&mut overlay, &mut model, KeyCode::Char('b'));
    assert!(!overlay.follow);
    assert!(model.jobs_inspector().inspected().is_none());
}

#[test]
fn styled_list_and_inspect_hide_shell_routing_and_render_bounded_streams() {
    let mut overlay = JobsOverlay::default();
    let mut model = SessionModel::default();
    let mut job = snapshot(CommandJobMode::Background);
    job.status = CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    };
    job.finished_at = Some(job.started_at + Duration::from_millis(1234));
    job.stdout_bytes = 1536 * 1024;
    job.stderr_bytes = 1536;
    overlay.show(model.jobs_inspector_mut());
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        String::new(),
        String::new(),
    );
    let list = render(&mut overlay, &model, 100, 20);
    let list_text = text(&list);
    assert!(list_text.contains("Jobs · 0 running · 1 finished"));
    assert!(list_text.contains("✓ cargo test"));
    assert!(list_text.contains("bg  1.2s  exited 0"));
    assert!(!list_text.contains("rtk") && !list_text.contains("job-inspect"));
    assert!(
        list.backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "✓" && cell.fg == theme().surfaces.selection_foreground)
    );
    let unselected = list_lines(std::slice::from_ref(&job), 80, 0);
    assert_eq!(
        unselected[0].spans[0].style.fg,
        Some(theme().feedback.success)
    );

    press(&mut overlay, &mut model, KeyCode::Enter);
    reply(
        &mut overlay,
        &mut model,
        job.clone(),
        "\u{1b}[31mred\u{1b}[0m\na\rb".into(),
        "warning".into(),
    );
    let inspect = render(&mut overlay, &model, 100, 20);
    let inspect_text = text(&inspect);
    assert!(inspect_text.contains("✓ cargo test · bg · exited 0 · 1.2s"));
    assert!(inspect_text.contains("job-inspect"));
    assert!(inspect_text.contains("stdout (last 8 KiB of 1.5 MiB):"));
    assert!(inspect_text.contains("stderr (1.5 KiB):"));
    assert!(inspect_text.contains("red") && !inspect_text.contains("[31m"));
    assert!(
        inspect_text
            .lines()
            .any(|line| line.trim_matches(|ch| ch == ' ' || ch == '│') == "b")
    );
    assert!(
        inspect
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "w" && cell.fg == theme().feedback.warning)
    );
    assert_eq!(model.jobs_inspector().jobs()[0].command, job.command);

    model
        .jobs_inspector_mut()
        .tick(Instant::now() + Duration::from_secs(2));
    reply(&mut overlay, &mut model, job, String::new(), String::new());
    let empty = text(&render(&mut overlay, &model, 100, 20));
    assert!(empty.contains("No output."));
    assert!(!empty.contains("stdout (") && !empty.contains("stderr ("));
}

#[test]
fn both_surfaces_render_in_tiny_terminals() {
    let mut overlay = JobsOverlay::default();
    let mut model = SessionModel::default();
    overlay.show(model.jobs_inspector_mut());
    reply(
        &mut overlay,
        &mut model,
        snapshot(CommandJobMode::Foreground),
        String::new(),
        String::new(),
    );
    for inspect in [false, true] {
        if inspect {
            press(&mut overlay, &mut model, KeyCode::Enter);
        }
        for (width, height) in [(1, 1), (2, 2), (10, 8), (100, 28)] {
            render(&mut overlay, &model, width, height);
        }
    }
}
