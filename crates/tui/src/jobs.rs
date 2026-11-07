//! Global, frontend-only shell job controls. Inspection never consumes model output.
use crate::{
    hints::{Eligibility, hint_line},
    input::{Action, ChordState, KeyContext},
};
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
use zevria_foundation::{CommandJobId, CommandJobMode, CommandJobSnapshot, CommandJobView};
use zevria_session_api::{
    CommandControl, CommandControlAction as Request, CommandControlResult as Reply, ControlCommand,
    SessionCommand, SessionEvent,
};
use zevria_tui_widgets::overlay::{ListNav, modal};
#[derive(Clone)]
enum Purpose {
    Inspect(Option<CommandJobId>, u64),
    Control(u64),
}
#[derive(Default)]
pub(crate) struct JobsOverlay {
    open: bool,
    jobs: Vec<CommandJobSnapshot>,
    inspect: Option<CommandJobId>,
    detail: Option<CommandJobView>,
    nav: ListNav,
    keys: ChordState,
    pending: HashMap<String, Purpose>,
    next_id: u64,
    generation: u64,
    last_refresh: Option<Instant>,
    refresh_again: bool,
    message: String,
    pub commands: VecDeque<SessionCommand>,
}
impl JobsOverlay {
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn show(&mut self) {
        self.open = true;
        self.inspect = None;
        self.detail = None;
        self.message.clear();
        self.nav.request_reveal();
        self.invalidate_geometry();
        self.refresh();
    }
    pub fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }
    fn selected(&self) -> Option<&CommandJobSnapshot> {
        if let Some(id) = &self.inspect {
            self.jobs.iter().find(|j| &j.id == id)
        } else {
            self.jobs.get(self.nav.selected)
        }
    }
    pub fn context(&self) -> KeyContext {
        if self.inspect.is_some() {
            KeyContext::JobsInspect
        } else {
            KeyContext::JobsList
        }
    }
    pub fn hint_eligibility(&self) -> Eligibility {
        let mut hints = Eligibility::default();
        hints
            .labels
            .extend([(Action::Confirm, "inspect"), (Action::Back, "list")]);
        let job = self.selected();
        if job.is_none() {
            hints.disabled.push(Action::Confirm);
        }
        if job.is_none_or(|j| j.status.is_terminal()) {
            hints.disabled.push(Action::Terminate);
        }
        if job.is_none_or(|j| j.status.is_terminal() || j.mode != CommandJobMode::Foreground) {
            hints.disabled.push(Action::MoveToBackground);
        }
        hints
    }
    fn send(&mut self, action: Request, purpose: Purpose) {
        if self.pending.len() >= 32 {
            self.message = "Too many pending job controls; wait for their results".into();
            return;
        }
        self.next_id += 1;
        let request_id = format!("tui.jobs.{}", self.next_id);
        self.pending.insert(request_id.clone(), purpose);
        self.commands
            .push_back(SessionCommand::Control(ControlCommand::Command(
                CommandControl { request_id, action },
            )));
    }
    fn refresh(&mut self) {
        if !self.open {
            return;
        }
        if self
            .pending
            .values()
            .any(|p| matches!(p, Purpose::Inspect(..)))
        {
            self.refresh_again = true;
            return;
        }
        self.last_refresh = Some(Instant::now());
        self.send(
            Request::Inspect {
                id: self.inspect.clone(),
            },
            Purpose::Inspect(self.inspect.clone(), self.generation),
        );
    }
    pub fn tick(&mut self, now: Instant) {
        if self.open
            && self
                .last_refresh
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            self.refresh();
        }
    }
    fn update(&mut self, job: CommandJobSnapshot) {
        let selected = self.jobs.get(self.nav.selected).map(|j| j.id.clone());
        if let Some(old) = self.jobs.iter_mut().find(|j| j.id == job.id) {
            if old.status.is_terminal() && !job.status.is_terminal() {
                return;
            }
            *old = job;
        } else {
            self.jobs.push(job);
        }
        self.jobs.sort_by_key(|j| j.started_at);
        let mut finished = self
            .jobs
            .iter()
            .filter(|j| j.mode == CommandJobMode::Background && j.status.is_terminal())
            .count();
        self.jobs.retain(|j| {
            if j.mode == CommandJobMode::Foreground && j.status.is_terminal() {
                return self.inspect.as_ref() == Some(&j.id);
            }
            if j.status.is_terminal() && finished > 8 {
                finished -= 1;
                return false;
            }
            true
        });
        self.nav.selected = selected
            .and_then(|id| self.jobs.iter().position(|j| j.id == id))
            .unwrap_or(self.nav.selected.min(self.jobs.len().saturating_sub(1)));
        self.invalidate_geometry();
    }
    pub fn event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::CommandJobUpdated { job } => {
                self.generation = self.generation.wrapping_add(1);
                self.update(job.clone());
                self.refresh();
            }
            SessionEvent::CommandControlResult { request_id, result } => {
                let Some(purpose) = self.pending.remove(request_id) else {
                    return;
                };
                match (purpose.clone(), result) {
                    (Purpose::Inspect(id, generation), Reply::Inspected(views))
                        if generation == self.generation =>
                    {
                        let selected = self.jobs.get(self.nav.selected).map(|j| j.id.clone());
                        if id.is_none() {
                            self.jobs
                                .retain(|j| views.iter().any(|v| v.snapshot.id == j.id));
                        }
                        for view in views {
                            self.update(view.snapshot.clone());
                        }
                        self.nav.selected = selected
                            .and_then(|id| self.jobs.iter().position(|j| j.id == id))
                            .unwrap_or(self.nav.selected.min(self.jobs.len().saturating_sub(1)));
                        if id == self.inspect && self.open {
                            self.detail = id
                                .and_then(|id| views.iter().find(|v| v.snapshot.id == id).cloned());
                        }
                    }
                    (Purpose::Inspect(..), Reply::Inspected(_)) => {
                        self.refresh_again = true;
                    }
                    (Purpose::Control(generation), Reply::Accepted(job)) => {
                        if generation == self.generation {
                            self.generation = self.generation.wrapping_add(1);
                            self.update(job.clone());
                        }
                        self.message = "Control accepted".into();
                        self.refresh_again = true;
                    }
                    (_, Reply::Rejected { message }) => {
                        self.message = message.clone();
                    }
                    _ => {}
                }
                if matches!(purpose, Purpose::Inspect(..))
                    && std::mem::take(&mut self.refresh_again)
                {
                    self.refresh();
                } else if matches!(purpose, Purpose::Control(_)) {
                    self.refresh();
                }
            }
            _ => {}
        }
    }
    pub fn handle_input(&mut self, key: KeyEvent) {
        let Some(action) = self.keys.resolve(self.context(), key) else {
            return;
        };
        if self.hint_eligibility().disabled.contains(&action) {
            return;
        }
        match action {
            Action::Cancel | Action::Close => self.open = false,
            Action::Back => {
                self.inspect = None;
                self.detail = None;
                self.jobs
                    .retain(|j| j.mode == CommandJobMode::Background || !j.status.is_terminal());
                self.nav.selected = self.nav.selected.min(self.jobs.len().saturating_sub(1));
                self.nav.viewport.jump_start();
                self.nav.request_reveal();
                self.refresh();
            }
            Action::Confirm if self.inspect.is_none() => {
                self.inspect = self.selected().map(|j| j.id.clone());
                self.detail = None;
                self.nav.viewport.jump_start();
                self.refresh();
            }
            Action::Terminate | Action::MoveToBackground => {
                if let Some(job) = self.selected() {
                    let id = job.id.clone();
                    self.send(
                        if action == Action::Terminate {
                            Request::Terminate { id }
                        } else {
                            Request::MoveToBackground { id }
                        },
                        Purpose::Control(self.generation),
                    );
                }
            }
            _ => {
                if let Some(action) = action.list_action() {
                    if self.inspect.is_some() {
                        self.nav.pan(action);
                    } else {
                        self.nav.handle(action, self.jobs.len());
                    }
                }
            }
        }
    }
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        let mut entries = Vec::new();
        if let Some(id) = &self.inspect {
            lines.push(Line::from(format!("Inspect {id}")));
            if let Some(detail) = &self.detail {
                lines.push(Line::from(format!(
                    "{} · {}",
                    detail.snapshot.command, detail.snapshot.status
                )));
                lines.push(Line::from("stdout (tail):"));
                lines.extend(
                    clean(&detail.stdout_tail)
                        .lines()
                        .map(|s| Line::from(s.to_owned())),
                );
                lines.push(Line::from("stderr (tail):"));
                lines.extend(
                    clean(&detail.stderr_tail)
                        .lines()
                        .map(|s| Line::from(s.to_owned())),
                );
            } else {
                lines.push(Line::from("Loading output tails…"));
            }
        } else {
            for job in &self.jobs {
                entries.push(lines.len());
                let elapsed = job
                    .finished_at
                    .unwrap_or_else(std::time::SystemTime::now)
                    .duration_since(job.started_at)
                    .unwrap_or_default()
                    .as_secs();
                lines.push(Line::from(format!(
                    "{} · {} · {} · {}s · {}",
                    job.id,
                    if job.mode == CommandJobMode::Foreground {
                        "fg"
                    } else {
                        "bg"
                    },
                    job.status,
                    elapsed,
                    job.command.split_whitespace().collect::<Vec<_>>().join(" ")
                )));
            }
            if self.jobs.is_empty() {
                lines.push(Line::from("No command jobs in this session."));
            }
        }
        if !self.message.is_empty() {
            lines.push(Line::from(clean(&self.message)));
        }
        let footer = hint_line(
            self.context(),
            &self.hint_eligibility(),
            usize::from(area.width.saturating_sub(2)),
        );
        let inner = modal(frame, area, " Jobs ", footer);
        let mut physical = 0;
        let rows: Vec<_> = lines
            .iter()
            .map(|line| {
                let start = physical;
                physical += Paragraph::new(line.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width.max(1));
                crate::viewport::RowRange::new(start, physical)
            })
            .collect();
        self.nav.reconcile_rows(
            entries.into_iter().map(|i| rows[i]).collect(),
            physical,
            usize::from(inner.height),
        );
        frame.render_widget(
            Paragraph::new(lines)
                .style(crate::chrome::overlay_style())
                .wrap(Wrap { trim: false })
                .scroll((crate::viewport::rows_to_u16(self.nav.viewport.top()), 0)),
            inner,
        );
        if self.inspect.is_none() && !self.jobs.is_empty() {
            self.nav.paint_selected(frame, inner);
        }
        crate::viewport::render_scrollbar(frame, area, &self.nav.viewport);
    }
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }
    #[test]
    fn stale_inspection_cannot_resurrect_a_finished_foreground_job_or_reopen_overlay() {
        let mut overlay = JobsOverlay::default();
        overlay.show();
        let SessionCommand::Control(ControlCommand::Command(first)) =
            overlay.commands.pop_front().unwrap()
        else {
            panic!()
        };
        let mut job = CommandJobSnapshot {
            id: CommandJobId::new("job-stale"),
            call_id: None,
            command: "sleep 20".into(),
            mode: CommandJobMode::Foreground,
            status: zevria_foundation::CommandJobStatus::Running,
            started_at: std::time::SystemTime::now(),
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        let stale = job.clone();
        overlay.event(&SessionEvent::CommandJobUpdated { job: job.clone() });
        job.status = zevria_foundation::CommandJobStatus::Exited {
            code: Some(0),
            success: true,
        };
        overlay.event(&SessionEvent::CommandJobUpdated { job });
        overlay.event(&SessionEvent::CommandControlResult {
            request_id: first.request_id,
            result: Reply::Inspected(vec![CommandJobView {
                snapshot: stale,
                stdout_tail: "stale".into(),
                stderr_tail: String::new(),
            }]),
        });
        assert!(overlay.jobs.is_empty());
        assert!(
            overlay
                .hint_eligibility()
                .disabled
                .contains(&Action::Terminate)
        );
        assert_eq!(overlay.commands.len(), 1);
        overlay.handle_input(key('q'));
        let SessionCommand::Control(ControlCommand::Command(next)) =
            overlay.commands.pop_front().unwrap()
        else {
            panic!()
        };
        overlay.event(&SessionEvent::CommandControlResult {
            request_id: next.request_id,
            result: Reply::Inspected(Vec::new()),
        });
        assert!(!overlay.is_open());
    }
    #[test]
    fn jobs_list_and_inspection_render_in_tiny_and_normal_terminals() {
        let mut overlay = JobsOverlay::default();
        overlay.show();
        for inspect in [false, true] {
            overlay.inspect = inspect.then(|| CommandJobId::new("job-view"));
            for (width, height) in [(1, 1), (2, 2), (10, 8), (100, 28)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal
                    .draw(|frame| overlay.render(frame, frame.area()))
                    .unwrap();
            }
        }
    }
    #[test]
    fn correlated_controls_one_inspect_and_cancel_only_dismisses() {
        let mut overlay = JobsOverlay::default();
        overlay.show();
        overlay.tick(Instant::now() + Duration::from_secs(2));
        assert_eq!(overlay.commands.len(), 1);
        let SessionCommand::Control(ControlCommand::Command(control)) =
            overlay.commands.pop_front().unwrap()
        else {
            panic!()
        };
        let job = CommandJobSnapshot {
            id: CommandJobId::new("job-test"),
            call_id: None,
            command: "sleep 600".into(),
            mode: CommandJobMode::Foreground,
            status: zevria_foundation::CommandJobStatus::Running,
            started_at: std::time::SystemTime::now(),
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        overlay.event(&SessionEvent::CommandControlResult {
            request_id: control.request_id,
            result: Reply::Inspected(vec![CommandJobView {
                snapshot: job,
                stdout_tail: String::new(),
                stderr_tail: String::new(),
            }]),
        });
        overlay.handle_input(key('m'));
        overlay.handle_input(key('x'));
        assert!(overlay.commands.iter().any(|c| matches!(
            c,
            SessionCommand::Control(ControlCommand::Command(CommandControl {
                action: Request::MoveToBackground { .. },
                ..
            }))
        )));
        assert!(overlay.commands.iter().any(|c| matches!(
            c,
            SessionCommand::Control(ControlCommand::Command(CommandControl {
                action: Request::Terminate { .. },
                ..
            }))
        )));
        let before = overlay.commands.len();
        overlay.handle_input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!overlay.is_open());
        assert_eq!(overlay.commands.len(), before);
    }
}
