//! Metadata-only skill management and completion refresh, independent of UI state.
use crate::outbox::{CommandOutbox, RequestIdAllocator};
use std::collections::HashMap;
use zevria_instructions::{
    SkillName,
    skill::{
        SkillManagementRequest as Request, SkillManagementResult as Result, SkillManagementView,
        SkillMeta,
    },
};
use zevria_session_api::{
    ManagementCommand, ManagementEvent, SessionCommand, SessionEvent, TurnEvent,
};

#[derive(Clone, Copy)]
enum Purpose {
    Completion,
    Browse(u64),
    Mutation,
}

#[derive(Default)]
enum Browser {
    #[default]
    Closed,
    List {
        query: String,
    },
    Inspect {
        name: SkillName,
    },
}

pub enum SkillChange {
    Completions(Vec<SkillMeta>),
    Page { selected: usize },
}

#[derive(Default)]
pub struct SkillInspector {
    browser: Browser,
    page: Option<SkillManagementView>,
    message: String,
    pending: HashMap<String, Purpose>,
    pub commands: CommandOutbox,
    refresh_again: bool,
    browser_generation: u64,
}

impl SkillInspector {
    pub fn is_open(&self) -> bool {
        !matches!(self.browser, Browser::Closed)
    }

    pub fn inspecting(&self) -> bool {
        matches!(self.browser, Browser::Inspect { .. })
    }

    pub fn page(&self) -> Option<&SkillManagementView> {
        self.page.as_ref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn loading(&self) -> bool {
        self.pending.values().any(|purpose| matches!(purpose, Purpose::Browse(generation) if *generation == self.browser_generation))
    }

    pub fn show(&mut self, query: String) {
        self.browser = Browser::List { query };
        self.browse();
    }

    pub fn close(&mut self) {
        self.browser = Browser::Closed;
        self.browser_generation = self.browser_generation.wrapping_add(1);
    }

    pub fn inspect(&mut self, index: usize) {
        if !self.loading()
            && let Some(entry) = self.page().and_then(|page| page.entries.get(index))
        {
            self.browser = Browser::Inspect {
                name: entry.name.clone(),
            };
            self.browse();
        }
    }

    pub fn reload(&mut self) {
        if !self.loading()
            && let Some(page) = self.page()
        {
            self.send(
                Request::Reload {
                    expected_revision: page.revision.clone(),
                },
                Purpose::Mutation,
            );
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if !self.loading()
            && let Some(page) = self.page()
            && let Some(entry) = page.entries.get(index)
        {
            self.send(
                Request::SetEnabled {
                    expected_revision: page.revision.clone(),
                    name: entry.name.clone(),
                    enabled: !entry.enabled,
                },
                Purpose::Mutation,
            );
        }
    }

    fn send(&mut self, request: Request, purpose: Purpose) {
        if self.pending.len() >= 32 {
            self.message = "Too many pending skill requests; wait for their results".into();
            return;
        }
        let request_id = RequestIdAllocator::allocate("skills-");
        self.pending.insert(request_id.clone(), purpose);
        self.commands
            .push_back(SessionCommand::Manage(ManagementCommand::Skills {
                request_id,
                request,
            }));
    }

    pub fn refresh(&mut self) {
        if self
            .pending
            .values()
            .any(|purpose| matches!(purpose, Purpose::Completion))
        {
            self.refresh_again = true;
            return;
        }
        self.send(
            Request::List {
                query: String::new(),
            },
            Purpose::Completion,
        );
    }

    fn browse(&mut self) {
        let request = match &self.browser {
            Browser::Closed => return,
            Browser::List { query } => Request::List {
                query: query.clone(),
            },
            Browser::Inspect { name } => Request::Inspect { name: name.clone() },
        };
        self.message = "Loading installed metadata…".into();
        self.browser_generation = self.browser_generation.wrapping_add(1);
        self.send(request, Purpose::Browse(self.browser_generation));
    }

    pub fn event(&mut self, event: &SessionEvent, selected: usize) -> Option<SkillChange> {
        match event {
            SessionEvent::Manage(ManagementEvent::SkillsChanged { .. }) => {
                self.refresh();
                self.browse();
            }
            SessionEvent::Turn(
                TurnEvent::ModeChanged { .. }
                | TurnEvent::Completed { .. }
                | TurnEvent::Recovered { .. }
                | TurnEvent::Failed { .. }
                | TurnEvent::Rejected { .. }
                | TurnEvent::Cancelled { .. },
            )
            | SessionEvent::Manage(ManagementEvent::ModeResult { .. }) => self.refresh(),
            SessionEvent::Manage(ManagementEvent::SkillsResult { request_id, result }) => {
                let purpose = self.pending.remove(request_id)?;
                match (purpose, result.as_ref()) {
                    (Purpose::Browse(generation), _) if generation != self.browser_generation => {}
                    (Purpose::Completion, outcome) => {
                        if std::mem::take(&mut self.refresh_again) {
                            self.refresh();
                        } else if let Result::View { view } = outcome {
                            return Some(SkillChange::Completions(view.completions.clone()));
                        }
                    }
                    (Purpose::Browse(_), Result::View { view }) => {
                        let selected = self
                            .page()
                            .and_then(|page| page.entries.get(selected))
                            .and_then(|old| {
                                view.entries
                                    .iter()
                                    .position(|entry| {
                                        entry.name == old.name
                                            && entry.scope == old.scope
                                            && entry.manifest == old.manifest
                                    })
                                    .or_else(|| {
                                        view.entries.iter().position(|entry| entry.name == old.name)
                                    })
                            })
                            .unwrap_or(selected.min(view.entries.len().saturating_sub(1)));
                        self.page = Some(view.clone());
                        self.message.clear();
                        return Some(SkillChange::Page { selected });
                    }
                    (Purpose::Mutation, Result::Changed { unchanged, .. }) => {
                        self.message = if *unchanged {
                            "Catalog unchanged"
                        } else {
                            "Saved and installed; other sessions require their own reload"
                        }
                        .into();
                        self.refresh();
                        self.browse();
                    }
                    (_, Result::Error { code, message }) => {
                        self.message = format!("{code}: {message}")
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }
}
