use super::*;
use SubtaskStatus::*;
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyModifiers},
};

fn row(name: &str, status: SubtaskStatus) -> SubtaskRow {
    SubtaskRow {
        id: SubtaskId::new(name),
        kind: SubtaskKind::Explore,
        title: name.into(),
        workspace: Some("crates/tui".into()),
        status: Some(status),
        historical: false,
        visible: false,
    }
}

fn press(overlay: &mut SubtasksOverlay, key: KeyCode) -> Option<SubtaskId> {
    overlay.handle_input(KeyEvent::new(key, KeyModifiers::NONE))
}

fn render(overlay: &mut SubtasksOverlay, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| overlay.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn rows_show_the_kind_status_workspace_and_visible_historical_pane() {
    let mut overlay = SubtasksOverlay::default();
    let mut historical = row("Read the logs", Completed);
    historical.visible = true;
    historical.historical = true;
    let mut live = row("Build a fixture", Running);
    live.kind = SubtaskKind::Build;
    overlay.show(vec![historical, live]);
    let screen = render(&mut overlay, 120, 28);
    assert!(
        screen.contains("Subtasks · 1 running · 2 total"),
        "{screen}"
    );
    let historical = screen
        .lines()
        .find(|line| line.contains("Read the logs"))
        .unwrap();
    assert!(
        historical.contains("▸") && historical.contains("Explore"),
        "{historical}"
    );
    assert!(
        historical.contains("completed · crates/tui · historical"),
        "{historical}"
    );
    let live = screen
        .lines()
        .find(|line| line.contains("Build a fixture"))
        .unwrap();
    assert!(
        live.contains("running · crates/tui") && !live.contains("▸"),
        "{live}"
    );
    let metadata_end = |line: &str| display_width(line.rsplit_once('│').unwrap().0.trim_end());
    assert_eq!(metadata_end(historical), metadata_end(live));
    assert_eq!(
        press(&mut overlay, KeyCode::Enter),
        Some(SubtaskId::new("Read the logs"))
    );
    assert!(!overlay.is_open());
}

#[test]
fn navigation_confirms_one_id_and_live_refresh_preserves_it() {
    let mut overlay = SubtasksOverlay::default();
    overlay.show(vec![
        row("old", Running),
        row("newest active", Starting),
        row("finished", Completed),
    ]);
    assert_eq!(overlay.nav.selected, 1);
    assert_eq!(press(&mut overlay, KeyCode::Up), None);
    assert_eq!(overlay.nav.selected, 0);
    overlay.replace_rows(vec![
        row("restored", Completed),
        row("old", Completed),
        row("newest active", Running),
        row("finished", Completed),
    ]);
    assert_eq!(overlay.nav.selected, 1);
    assert_eq!(press(&mut overlay, KeyCode::Char('j')), None);
    assert_eq!(
        press(&mut overlay, KeyCode::Enter),
        Some(SubtaskId::new("newest active"))
    );
    assert!(!overlay.is_open());
}

#[test]
fn empty_state_does_not_confirm_and_close_captures_escape() {
    let mut overlay = SubtasksOverlay::default();
    overlay.show(Vec::new());
    assert!(render(&mut overlay, 80, 20).contains("No subtasks in this session."));
    assert_eq!(press(&mut overlay, KeyCode::Enter), None);
    assert!(overlay.is_open());
    assert_eq!(press(&mut overlay, KeyCode::Esc), None);
    assert!(!overlay.is_open());
}

#[test]
fn narrow_rows_keep_the_title_and_preserve_the_historical_suffix_when_it_fits() {
    let mut row = row("調べる\nRestore the transcript", Completed);
    row.historical = true;
    row.workspace = Some("a/long/historical/workspace/path/for/crates/desktop".into());
    let line = subtask_line(&row, 68, 0).to_string();
    assert!(line.contains("調べる Restore"), "{line}");
    assert!(line.ends_with(" · historical"), "{line}");
    assert_eq!(display_width(&line), 68, "{line}");
    assert!(!line.contains('\n'));
}
