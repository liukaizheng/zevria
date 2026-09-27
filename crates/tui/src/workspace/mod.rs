//! Pane lifetime, global surfaces, restoration and engine/input routing.

use crate::app::overlays::OverlayController;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{crossterm::event::Event, style::Style};
use tokio::sync::mpsc::UnboundedSender;
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskLaunchMetadata;
use zevria_foundation::subtask::SubtaskStatus;
use zevria_model::models::ModelSelectionScope;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionStreamBatch;
use zevria_transcript::AgentRunTranscriptRecord;
use zevria_transcript::transcript::SessionSummary;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentRunEvent;
use zevria_workflow::AgentRunId;
use zevria_workflow::AgentRunStatus;
use zevria_workflow::AgentUsage;
use zevria_workflow::EnsembleRunId;

use crate::agent_transcript::AgentTranscriptReducer;
use crate::app::{App, AppEffect, ConfiguredModelProfiles, UiAction};
use crate::chrome::paint_rule;
use crate::input::{Action, InputEvent, PaneId, UserInput};
use crate::picker::SessionPicker;
use crate::question::QuestionDialog;
use crate::status::ExternalContextUsage;
use crate::theme::theme;
use crate::workspace_header::WorkspaceHeader;

#[cfg(test)]
#[path = "../baseline_runtime_tests.rs"]
mod baseline_tests;
#[cfg(test)]
#[path = "../mode_runtime_tests.rs"]
mod mode_tests;
#[cfg(test)]
#[path = "../model_reasoning_runtime_tests.rs"]
mod model_reasoning_tests;
#[cfg(test)]
#[path = "../model_runtime_tests.rs"]
mod model_tests;
#[cfg(test)]
#[path = "../worker_runtime_tests.rs"]
mod worker_tests;

#[cfg(test)]
use crate::runtime::drain_update_burst;

/// One child subsession pane.
struct ChildPane {
    id: SubtaskId,
    creation_ordinal: u64,
    kind: SubtaskKind,
    title: String,
    workspace: Option<String>,
    app: App,
    /// Restored from a transcript with no live worker behind it.
    historical: bool,
    status: Option<SubtaskStatus>,
}

/// One live or restored provider-neutral ACP worker pane.
struct AgentPane {
    id: AgentRunId,
    creation_ordinal: u64,
    ensemble_run_id: EnsembleRunId,
    label: String,
    safe_mode: String,
    app: App,
    transcript: AgentTranscriptReducer,
    historical: bool,
    status: AgentRunStatus,
    usage: Option<ExternalContextUsage>,
    /// Root selection, independent of live review bindings and audit messages.
    baseline: bool,
}

fn external_context_usage(usage: &AgentUsage) -> ExternalContextUsage {
    ExternalContextUsage {
        used: usage.used,
        size: usage.size,
    }
}

impl AgentPane {
    fn apply_transcript_event(&mut self, event: AgentRunEvent) {
        if let AgentRunEvent::Usage { usage } = &event {
            self.usage = Some(external_context_usage(usage));
        }
        let anchor = self.app.presentation_selection_anchor();
        let change = self
            .transcript
            .apply_event(self.app.conversation_projection_mut(), event);
        let effects = self.app.apply_conversation_change(change);
        self.app.restore_presentation_selection(anchor);
        debug_assert!(
            effects.is_empty(),
            "inspect panes cannot prune root workers"
        );
    }

    fn apply_transcript_preview(&mut self, event: AgentRunEvent) {
        if let AgentRunEvent::Usage { usage } = &event {
            self.usage = Some(external_context_usage(usage));
        }
        let anchor = self.app.presentation_selection_anchor();
        let change = self
            .transcript
            .apply_preview(self.app.conversation_projection_mut(), event);
        let effects = self.app.apply_conversation_change(change);
        self.app.restore_presentation_selection(anchor);
        debug_assert!(
            effects.is_empty(),
            "inspect panes cannot prune root workers"
        );
    }

    fn reconcile_report(&mut self, report: &str) {
        let change = self
            .transcript
            .reconcile_report(self.app.conversation_projection_mut(), report);
        let effects = self.app.apply_conversation_change(change);
        debug_assert!(
            effects.is_empty(),
            "inspect panes cannot prune root workers"
        );
    }

    fn reconcile_plan(&mut self, plan: &zevria_workflow::AgentStructuredPlan) {
        let change = self
            .transcript
            .reconcile_plan(self.app.conversation_projection_mut(), plan);
        let effects = self.app.apply_conversation_change(change);
        debug_assert!(
            effects.is_empty(),
            "inspect panes cannot prune root workers"
        );
    }

    fn reconcile_outcome_evidence(
        &mut self,
        report: &str,
        plan: Option<&zevria_workflow::AgentStructuredPlan>,
        confirmation: Option<&zevria_workflow::ConfirmedWorkerPlan>,
    ) {
        if let Some(plan) = plan {
            self.reconcile_plan(plan);
        }
        if !self.transcript.report_duplicates_current_plan(report) {
            self.reconcile_report(report);
        }
        if let Some(confirmation) = confirmation {
            self.apply_transcript_event(AgentRunEvent::Review {
                event: Box::new(zevria_workflow::WorkerReviewEvent::Confirmed {
                    receipt: confirmation.receipt.clone(),
                }),
            });
            if let Some(receipt) = &confirmation.baseline {
                self.apply_transcript_event(AgentRunEvent::Review {
                    event: Box::new(zevria_workflow::WorkerReviewEvent::BaselineMarked {
                        receipt: receipt.clone(),
                    }),
                });
            }
            self.apply_transcript_event(AgentRunEvent::Review {
                event: Box::new(zevria_workflow::WorkerReviewEvent::Sealed),
            });
        }
    }

    fn refresh_pane_metadata(&mut self) {
        let mut label = format!(
            "ACP · {} · {} · {}",
            self.label, self.safe_mode, self.status
        );
        if self.historical {
            label.push_str(" · historical");
        }
        if self.baseline {
            label.push_str(" · baseline");
        }
        self.app.update_pane_metadata(label, self.usage);
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ClipboardOrigin {
    Root,
    Agent(EnsembleRunId, AgentRunId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisiblePane {
    Subtask(PaneId),
    Agent(PaneId),
}

impl ChildPane {
    fn refresh_pane_metadata(&mut self) {
        let mut label = format!("{} · {}", self.kind, self.title);
        if let Some(workspace) = &self.workspace {
            label.push_str(&format!(" · {workspace}"));
        }
        if let Some(status) = self.status {
            label.push_str(&format!(" · {status}"));
        }
        if self.historical {
            label.push_str(" · historical");
        }
        self.app.update_pane_metadata(label, None);
    }
}

/// The root pane plus live/historical child panes and their navigation state.
pub struct SessionViews {
    pub(crate) root: App,
    model_profiles: Arc<ConfiguredModelProfiles>,
    workspace_header: WorkspaceHeader,
    pub(crate) startup_workspace: PathBuf,
    pub(crate) file_service_id: crate::workspace_files::ServiceId,
    file_owner: Option<PaneId>,
    children: Vec<ChildPane>,
    agents: Vec<AgentPane>,
    /// `None` shows the root; `Some` selects an inspect-only child pane.
    visible: Option<VisiblePane>,
    /// The most recently entered child, re-opened by `Ctrl-I`.
    last_entered: Option<VisiblePane>,
    /// One clock shared by both child kinds, so recency is source-neutral.
    next_creation_ordinal: u64,
    /// Root-transcript launch order used when child files restore in a
    /// filesystem-dependent order.
    restored_subtask_ordinals: HashMap<SubtaskId, u64>,
    restored_agent_ordinals: HashMap<AgentRunId, u64>,
    abandoned_workers: HashMap<(EnsembleRunId, AgentRunId), zevria_workflow::WorkerControlId>,
    baseline_workers: HashMap<EnsembleRunId, AgentRunId>,
    pub(crate) overlays: OverlayController,
}

impl SessionViews {
    pub fn new(mut root: App, startup_workspace: PathBuf) -> Self {
        // Capture relative API inputs now, not against a later process cwd.
        // Canonicalization and all discovery still belong to the file worker.
        let startup_workspace =
            std::path::absolute(&startup_workspace).unwrap_or(startup_workspace);
        // A prior standalone render did not reserve the workspace header.
        root.invalidate_rendered_geometry();
        let model_profiles = root.shared_model_profiles();
        Self {
            root,
            model_profiles,
            workspace_header: WorkspaceHeader::new(startup_workspace.clone()),
            startup_workspace,
            file_service_id: crate::workspace_files::ServiceId::default(),
            file_owner: None,
            children: Vec::new(),
            agents: Vec::new(),
            visible: None,
            last_entered: None,
            next_creation_ordinal: 0,
            restored_subtask_ordinals: HashMap::new(),
            restored_agent_ordinals: HashMap::new(),
            abandoned_workers: HashMap::new(),
            baseline_workers: HashMap::new(),
            overlays: OverlayController::default(),
        }
    }

    /// Seed the shared child creation clock from durable root records before
    /// loading any child transcript files. Subtask launch metadata and ACP
    /// ensemble starts are the authoritative cross-source ordering available
    /// during restoration.
    pub fn seed_child_creation_order(&mut self, items: &[TranscriptItem]) {
        self.baseline_workers.clear();
        if let Ok(runs) = zevria_transcript::project_worker_reviews(items) {
            for (run_id, states) in runs {
                if let Some(state) = states
                    .iter()
                    .find(|state| state.baseline.is_some() && !state.abandoned)
                {
                    self.baseline_workers
                        .insert(run_id, state.descriptor.id.clone());
                }
            }
        }
        for item in items {
            match item {
                TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::Started { start }) => {
                    if let Some(worker_id) = self.root.ensemble_baseline(&start.run_id) {
                        self.baseline_workers
                            .insert(start.run_id.clone(), worker_id.clone());
                    }
                    for descriptor in &start.agents {
                        if !self.restored_agent_ordinals.contains_key(&descriptor.id) {
                            let ordinal = self.take_creation_ordinal();
                            self.restored_agent_ordinals
                                .insert(descriptor.id.clone(), ordinal);
                        }
                    }
                }
                TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::WorkerReview {
                    run_id,
                    worker_id,
                    event,
                    ..
                }) => {
                    if let zevria_workflow::WorkerReviewEvent::Abandoned { request_id } =
                        event.as_ref()
                    {
                        self.abandoned_workers
                            .insert((run_id.clone(), worker_id.clone()), request_id.clone());
                    }
                }
                TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::WorkersConfirmed {
                    run_id,
                    final_confirmation,
                    ..
                }) => {
                    if matches!(
                        final_confirmation.control.action,
                        zevria_workflow::WorkerControlAction::Abandon
                    ) {
                        self.abandoned_workers.insert(
                            (
                                run_id.clone(),
                                final_confirmation.control.target.worker_id.clone(),
                            ),
                            final_confirmation.control.request_id.clone(),
                        );
                    }
                }
                TranscriptItem::ToolResults { metadata, .. } => {
                    for entry in metadata.iter().flat_map(|metadata| metadata.subtasks()) {
                        if let Some(subtask) = &entry.launch {
                            self.seed_subtask_ordinal(&subtask.id);
                            self.install_subtask_metadata(subtask, entry.status);
                            let index = self.ensure_child(&subtask.id);
                            self.children[index].historical = true;
                            self.children[index].refresh_pane_metadata();
                        }
                    }
                }
                TranscriptItem::SessionModels(_)
                | TranscriptItem::SessionMode(_)
                | TranscriptItem::Directive(_)
                | TranscriptItem::RequestDirective(_)
                | TranscriptItem::RequestPrompt { .. }
                | TranscriptItem::Message(_)
                | TranscriptItem::ProviderMessage(_)
                | TranscriptItem::AssistantMessage { .. }
                | TranscriptItem::SkillInvocation(_)
                | TranscriptItem::Plan(_)
                | TranscriptItem::Ensemble(_)
                | TranscriptItem::Compaction(_)
                | TranscriptItem::WebSearchAttempt(_)
                | TranscriptItem::Error { .. } => {}
            }
        }
    }

    fn seed_subtask_ordinal(&mut self, id: &SubtaskId) {
        if !self.restored_subtask_ordinals.contains_key(id) {
            let ordinal = self.take_creation_ordinal();
            self.restored_subtask_ordinals.insert(id.clone(), ordinal);
        }
    }

    fn take_creation_ordinal(&mut self) -> u64 {
        let ordinal = self.next_creation_ordinal;
        self.next_creation_ordinal = self.next_creation_ordinal.wrapping_add(1);
        ordinal
    }

    /// Rebuild one ACP pane from its durable normalized JSONL records.
    pub fn restore_agent_run(&mut self, records: Vec<AgentRunTranscriptRecord>) {
        self.restore_agent_run_stream(records.into_iter().map(Ok::<_, anyhow::Error>))
            .expect("in-memory agent-run records cannot fail");
    }

    /// Stream one historical worker transcript into its inspect-only pane so
    /// session startup never retains both the raw file and decoded records.
    pub fn restore_agent_run_stream(
        &mut self,
        records: impl IntoIterator<Item = anyhow::Result<AgentRunTranscriptRecord>>,
    ) -> anyhow::Result<()> {
        let mut pane_index = None;
        for record in records {
            let record = record?;
            if let AgentRunTranscriptRecord::Header { header } = &record
                && pane_index.is_none()
            {
                let index = self.ensure_agent(&header.ensemble_run_id, &header.descriptor);
                self.agents[index].historical = true;
                self.agents[index].app.freeze_worker();
                pane_index = Some(index);
            }
            let Some(index) = pane_index else {
                continue;
            };
            let pane = &mut self.agents[index];
            match record {
                AgentRunTranscriptRecord::Header { .. } => {}
                AgentRunTranscriptRecord::Event { event } => {
                    if let AgentRunEvent::Status { status, .. } = &event {
                        pane.status = *status;
                    }
                    pane.apply_transcript_event(event);
                }
                AgentRunTranscriptRecord::Outcome { outcome } => {
                    pane.status = outcome.status;
                    pane.app.freeze_worker();
                    if let Some(usage) = &outcome.usage {
                        pane.usage = Some(external_context_usage(usage));
                    }
                    pane.reconcile_outcome_evidence(
                        &outcome.report,
                        outcome.plan.as_ref(),
                        outcome.confirmation.as_deref(),
                    );
                    if let Some(error) = outcome.failure
                        && !pane.app.contains_presented_error(&error)
                    {
                        pane.apply_transcript_event(AgentRunEvent::Failure { error });
                    }
                }
            }
        }
        let Some(index) = pane_index else {
            return Ok(());
        };
        let pane = &mut self.agents[index];
        if let Some(request_id) = self
            .abandoned_workers
            .get(&(pane.ensemble_run_id.clone(), pane.id.clone()))
        {
            pane.apply_transcript_event(AgentRunEvent::Review {
                event: Box::new(zevria_workflow::WorkerReviewEvent::Abandoned {
                    request_id: request_id.clone(),
                }),
            });
            pane.status = AgentRunStatus::Abandoned;
            pane.app.freeze_worker();
        } else if !pane.status.is_terminal() {
            pane.status = AgentRunStatus::Interrupted;
        }
        pane.refresh_pane_metadata();
        Ok(())
    }

    /// Rebuild one child pane from its persisted transcript. Historical
    /// children are inspectable but marked historical and never restarted.
    pub fn restore_child(
        &mut self,
        id: SubtaskId,
        metadata: Option<SubtaskLaunchMetadata>,
        items: Vec<TranscriptItem>,
    ) {
        let index = self.ensure_child(&id);
        if let Some(metadata) = metadata {
            let pane = &mut self.children[index];
            pane.kind = metadata.kind;
            pane.title = metadata.title;
            pane.workspace = metadata.workspace;
            pane.app.set_inspect_model_role(pane.kind.model_role());
        }
        self.children[index].historical = true;
        self.children[index].app.restore(items);
        self.children[index].refresh_pane_metadata();
    }

    fn install_subtask_metadata(
        &mut self,
        metadata: &SubtaskLaunchMetadata,
        status: SubtaskStatus,
    ) {
        let index = self.ensure_child(&metadata.id);
        let pane = &mut self.children[index];
        pane.title = metadata.title.clone();
        pane.kind = metadata.kind;
        pane.workspace = metadata.workspace.clone();
        if !pane.status.is_some_and(SubtaskStatus::is_terminal) {
            pane.status = Some(status);
        }
        pane.app.set_inspect_model_role(pane.kind.model_role());
        pane.refresh_pane_metadata();
    }

    fn ensure_child(&mut self, id: &SubtaskId) -> usize {
        if let Some(index) = self.children.iter().position(|child| &child.id == id) {
            return index;
        }
        let app = App::subtask_inspect("")
            .with_shared_model_profiles(Arc::clone(&self.model_profiles))
            .with_inspect_reasoning_from(&self.root);
        let creation_ordinal = self
            .restored_subtask_ordinals
            .get(id)
            .copied()
            .unwrap_or_else(|| self.take_creation_ordinal());
        let mut pane = ChildPane {
            id: id.clone(),
            creation_ordinal,
            kind: SubtaskKind::Explore,
            title: id.as_str().chars().take(8).collect(),
            workspace: None,
            app,
            historical: false,
            status: None,
        };
        pane.refresh_pane_metadata();
        self.children.push(pane);
        self.children.len() - 1
    }

    fn ensure_agent(
        &mut self,
        ensemble_run_id: &EnsembleRunId,
        descriptor: &AgentRunDescriptor,
    ) -> usize {
        if let Some(index) = self.agents.iter().position(|pane| pane.id == descriptor.id) {
            return index;
        }
        let app = App::acp_inspect("")
            .with_shared_model_profiles(Arc::clone(&self.model_profiles))
            .with_inspect_reasoning_from(&self.root);
        let creation_ordinal = self
            .restored_agent_ordinals
            .get(&descriptor.id)
            .copied()
            .unwrap_or_else(|| self.take_creation_ordinal());
        let mut pane = AgentPane {
            id: descriptor.id.clone(),
            creation_ordinal,
            ensemble_run_id: ensemble_run_id.clone(),
            label: descriptor.label.clone(),
            safe_mode: descriptor.safe_mode.clone(),
            app,
            transcript: AgentTranscriptReducer::default(),
            historical: false,
            status: AgentRunStatus::Queued,
            usage: None,
            baseline: self
                .baseline_workers
                .get(ensemble_run_id)
                .or_else(|| self.root.ensemble_baseline(ensemble_run_id))
                == Some(&descriptor.id),
        };
        pane.refresh_pane_metadata();
        self.agents.push(pane);
        self.agents.len() - 1
    }

    /// Reconcile the visible input owner before requesting or applying results.
    /// Hidden, retired, frozen, and overlay-owned drafts cannot receive searches.
    pub(crate) fn file_search_request(&mut self) -> Option<crate::workspace_files::SearchRequest> {
        let owner = self.overlays.snapshot(self.visible_app().surface()).owner;
        let pane = (!owner.captures).then_some(owner.id.pane);
        if self.file_owner != pane {
            if let Some(previous) = self.file_owner {
                if self.root.surface().id.pane == previous {
                    self.root.suspend_file_completion();
                } else if let Some(agent) = self
                    .agents
                    .iter_mut()
                    .find(|agent| agent.app.surface().id.pane == previous)
                {
                    agent.app.suspend_file_completion();
                }
            }
            self.file_owner = pane;
        }
        let completion = self
            .visible_app_mut()
            .reconcile_file_completion(pane.is_some())?;
        Some(crate::workspace_files::SearchRequest {
            service: self.file_service_id,
            pane: owner.id.pane,
            completion,
        })
    }

    pub(crate) fn file_search_completed(
        &mut self,
        result: crate::workspace_files::SearchResult,
    ) -> bool {
        if self.file_search_request().as_ref() != Some(&result.request) {
            return false;
        }
        self.visible_app_mut().install_file_results(
            &result.request.completion,
            result.paths,
            result.status,
        )
    }

    pub(crate) fn clipboard_origin(&self) -> Option<ClipboardOrigin> {
        match self.visible {
            None => Some(ClipboardOrigin::Root),
            Some(VisiblePane::Agent(id)) => self
                .agents
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| ClipboardOrigin::Agent(pane.ensemble_run_id.clone(), pane.id.clone())),
            Some(VisiblePane::Subtask(_)) => None,
        }
    }

    pub(crate) fn clipboard_completed(
        &mut self,
        origin: ClipboardOrigin,
        generation: u64,
        cursor: usize,
        result: crate::clipboard::ClipboardResult,
    ) {
        let app = match origin {
            ClipboardOrigin::Root => Some(&mut self.root),
            ClipboardOrigin::Agent(run, id) => self
                .agents
                .iter_mut()
                .find(|pane| pane.ensemble_run_id == run && pane.id == id && !pane.historical)
                .map(|pane| &mut pane.app),
        };
        if let Some(app) = app {
            app.clipboard_completed(generation, cursor, result);
        }
    }

    fn visible_app_mut(&mut self) -> &mut App {
        match self.visible {
            Some(VisiblePane::Subtask(id)) => {
                &mut self
                    .children
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            Some(VisiblePane::Agent(id)) => {
                &mut self
                    .agents
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            None => &mut self.root,
        }
    }

    fn set_visible(&mut self, visible: Option<VisiblePane>) {
        if self.visible != visible {
            // Tail rewrites may already have removed the outgoing worker.
            let outgoing = match self.visible {
                Some(VisiblePane::Subtask(index)) => self
                    .children
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == index)
                    .map(|pane| &mut pane.app),
                Some(VisiblePane::Agent(index)) => self
                    .agents
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == index)
                    .map(|pane| &mut pane.app),
                None => Some(&mut self.root),
            };
            if let Some(app) = outgoing {
                app.close_composer_edit_group();
            }
            self.visible = visible;
            self.visible_app_mut().close_composer_edit_group();
            self.visible_app_mut().invalidate_rendered_geometry();
        }
    }

    fn visible_app(&self) -> &App {
        match self.visible {
            Some(VisiblePane::Subtask(id)) => {
                &self
                    .children
                    .iter()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            Some(VisiblePane::Agent(id)) => {
                &self
                    .agents
                    .iter()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            None => &self.root,
        }
    }

    /// Observe only the visible pane; hidden native panes still observe lifecycle receipt.
    pub(crate) fn observe_clock(&mut self, now: Instant) {
        self.visible_app_mut().observe_clock(now);
    }

    pub(crate) fn clock_required(&self) -> bool {
        self.visible_app().is_busy()
    }

    /// Open the modal session picker over whatever pane is visible.
    pub fn open_session_picker(&mut self, sessions: Vec<SessionSummary>) {
        self.overlays.show_sessions(SessionPicker::new(sessions));
    }

    /// Surface a frontend-side failure as a root-pane notice.
    pub fn show_root_error(&mut self, error: String) {
        self.root.push_error(error);
    }

    /// Draw the currently visible pane and global workspace chrome, then any
    /// modal overlay above the protected header row.
    pub fn render(&mut self, frame: &mut ratatui::Frame) {
        let accent = self.visible_app().status_accent().color();
        let snapshot = self.overlays.snapshot(self.visible_app().surface());
        let owner = snapshot.owner;
        let layout =
            self.visible_app_mut()
                .render_surface(frame, true, !owner.captures && owner.cursor);
        if layout.workspace_header_enabled {
            self.workspace_header
                .render(frame.buffer_mut(), layout.workspace_header, accent);
            paint_rule(
                frame.buffer_mut(),
                layout.workspace_header_rule,
                Style::new()
                    .fg(theme().surfaces.border)
                    .bg(theme().surfaces.canvas),
            );
        }
        self.overlays.render(&snapshot, frame, layout.modal_body);
    }

    pub(crate) fn open_model_picker(&mut self, scope: ModelSelectionScope) {
        if self.visible.is_none()
            && let Some(mode) = self.root.begin_model_management()
        {
            self.overlays.show_models(mode, scope);
        } else {
            self.show_root_error("Model selection requires an idle interactive root session with no pending Plan approval.".into());
        }
    }

    pub(crate) fn send_mode_selection(
        &mut self,
        commands: &UnboundedSender<SessionCommand>,
        request_id: String,
        mode: zevria_foundation::SessionMode,
    ) {
        if commands
            .send(SessionCommand::Manage(
                zevria_session_api::ManagementCommand::SetMode {
                    request_id: request_id.clone(),
                    mode,
                },
            ))
            .is_err()
        {
            self.apply_root_event(SessionEvent::ModeResult {
                request_id,
                result: zevria_session_api::ModeSelectionResult::Rejected {
                    code: "engine_stopped".into(),
                    message: "The session engine stopped before accepting mode selection.".into(),
                },
            });
        }
    }

    /// Route an engine event: tagged child traffic reduces into its hidden
    /// pane, everything else into the root pane.
    pub fn apply(&mut self, event: SessionEvent) {
        self.apply_event(event);
        self.file_search_request();
    }

    fn apply_event(&mut self, event: SessionEvent) {
        if let SessionEvent::ModelsResult { request_id, result } = &event {
            if self.overlays.models.accept(request_id, result) {
                if let zevria_model::models::ModelManagementResult::Changed {
                    role,
                    context,
                    snapshot,
                    reasoning_level,
                    unchanged,
                    ..
                } = result
                {
                    self.root.install_reasoning(*role, *reasoning_level);
                    if !unchanged
                        && (self.model_profiles.get(*role) != Some(&context.profile)
                            || snapshot.is_some())
                    {
                        self.root.install_model(*role, context, snapshot.clone());
                        self.model_profiles = self.root.shared_model_profiles();
                    }
                }
                if !self.overlays.models.is_open() {
                    self.root.finish_model_management();
                }
            }
            return;
        }
        if let Some(entries) = self.overlays.skills.event(&event) {
            self.root.replace_skill_entries(entries);
        }
        let refresh_workspace_header = matches!(&event, SessionEvent::ToolResults { .. });
        let terminal_turn = match &event {
            SessionEvent::TurnCompleted { turn_id, .. }
            | SessionEvent::TurnRecovered { turn_id, .. }
            | SessionEvent::TurnFailed { turn_id, .. }
            | SessionEvent::TurnRejected { turn_id, .. }
            | SessionEvent::TurnCancelled { turn_id } => Some(*turn_id),
            _ => None,
        }
        .filter(|turn_id| self.root.accepts_terminal(*turn_id));
        if terminal_turn.is_some() {
            for pane in &mut self.agents {
                pane.app.freeze_worker();
            }
        }
        if terminal_turn.is_some_and(|turn_id| {
            self.overlays
                .question()
                .is_some_and(|question| question.turn_id() == turn_id)
        }) {
            self.overlays.set_question(None);
        }
        if let SessionEvent::ToolResults { metadata, .. } = &event {
            for entry in metadata.iter().flat_map(|metadata| metadata.subtasks()) {
                if let Some(launch) = &entry.launch {
                    self.install_subtask_metadata(launch, entry.status);
                }
            }
        }
        match event {
            SessionEvent::QuestionAsked { turn_id, request } => {
                // Resolve correlation before capturing focus. A delayed
                // question cannot reopen after its turn or steal management.
                if self.root.accepts_progress(turn_id) {
                    self.overlays
                        .set_question(Some(QuestionDialog::new(turn_id, request)));
                }
            }
            SessionEvent::QuestionClosed {
                turn_id,
                request_id,
            } => {
                if self.overlays.question().is_some_and(|question| {
                    question.turn_id() == turn_id && question.request_id() == &request_id
                }) {
                    self.overlays.set_question(None);
                }
            }
            SessionEvent::SubtaskLaunched {
                turn_id,
                call_id,
                entry_index,
                mut descriptor,
            } => {
                let index = self.ensure_child(&descriptor.id);
                let pane = &mut self.children[index];
                if pane.status.is_none() {
                    pane.status = Some(SubtaskStatus::Starting);
                }
                descriptor.status = pane
                    .status
                    .expect("a launched child pane always has lifecycle status");
                pane.kind = descriptor.kind;
                pane.title = descriptor.title.clone();
                pane.workspace = descriptor.workspace.clone();
                pane.app.set_inspect_model_role(pane.kind.model_role());
                pane.refresh_pane_metadata();
                self.apply_root_event(SessionEvent::SubtaskLaunched {
                    turn_id,
                    call_id,
                    entry_index,
                    descriptor,
                });
            }
            SessionEvent::SubtaskSession { id, event } => {
                let index = self.ensure_child(&id);
                let effects = self.children[index].app.reduce(*event);
                debug_assert!(
                    effects.is_empty(),
                    "inspect panes cannot prune root workers"
                );
            }
            SessionEvent::SubtaskStatus {
                turn_id,
                id,
                status,
            } => {
                let index = self.ensure_child(&id);
                if self.children[index].status.is_none_or(|previous| {
                    !previous.is_terminal() && status != SubtaskStatus::Starting
                }) {
                    self.children[index].status = Some(status);
                }
                let status = self.children[index].status.expect("child status installed");
                self.children[index].refresh_pane_metadata();
                self.apply_root_event(SessionEvent::SubtaskStatus {
                    turn_id,
                    id,
                    status,
                });
            }
            SessionEvent::EnsembleStarted {
                turn_id,
                start,
                resumed,
            } => {
                self.apply_root_event(SessionEvent::EnsembleStarted {
                    turn_id,
                    start: start.clone(),
                    resumed,
                });
                for descriptor in &start.agents {
                    if self
                        .abandoned_workers
                        .contains_key(&(start.run_id.clone(), descriptor.id.clone()))
                        && !self.agents.iter().any(|pane| {
                            pane.id == descriptor.id && pane.ensemble_run_id == start.run_id
                        })
                    {
                        continue;
                    }
                    let index = self.ensure_agent(&start.run_id, descriptor);
                    let pane = &mut self.agents[index];
                    pane.historical = false;
                    pane.status = if self
                        .abandoned_workers
                        .contains_key(&(start.run_id.clone(), descriptor.id.clone()))
                    {
                        AgentRunStatus::Abandoned
                    } else if resumed {
                        AgentRunStatus::Resuming
                    } else {
                        AgentRunStatus::Queued
                    };
                    pane.refresh_pane_metadata();
                }
            }
            SessionEvent::WorkerControlResult { result } => {
                if let Some(pane) = self.agents.iter_mut().find(|pane| {
                    pane.id == result.control.target.worker_id
                        && pane.ensemble_run_id == result.control.target.run_id
                }) {
                    pane.app.worker_control_result(&result);
                    if result.accepted {
                        use zevria_workflow::WorkerControlAction as Action;
                        use zevria_workflow::WorkerReviewEvent as Review;
                        let event = match &result.control.action {
                            Action::Unconfirm { .. } => Some(Review::Withdrawn {
                                request_id: result.control.request_id.clone(),
                            }),
                            Action::Unbaseline { .. } => Some(Review::BaselineCleared {
                                request_id: result.control.request_id.clone(),
                            }),
                            Action::Confirm { .. } | Action::Baseline { .. } => {
                                result.control.sealing_event().ok()
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            pane.apply_transcript_event(AgentRunEvent::Review {
                                event: Box::new(event),
                            });
                        }
                    }
                }
            }
            SessionEvent::WorkerReviewUpdated { target, state } => {
                let live = self.root.active_turn_id() == Some(target.turn_id);
                if let Some(pane) = self.agents.iter_mut().find(|pane| {
                    pane.id == target.worker_id
                        && pane.ensemble_run_id == target.run_id
                        && !pane.historical
                }) {
                    pane.status = state.status();
                    if state.abandoned {
                        // Root authority also freezes panes when no worker audit
                        // mirror made it to disk. Keep preceding transcript text.
                        let change = pane
                            .transcript
                            .abandon(pane.app.conversation_projection_mut());
                        pane.app.apply_conversation_change(change);
                    }
                    if !state.abandoned
                        && state.quiescent()
                        && let Some(snapshot) = &state.retained
                    {
                        pane.reconcile_plan(&snapshot.plan);
                    }
                    if !state.abandoned {
                        let anchor = pane.app.presentation_selection_anchor();
                        pane.transcript
                            .apply_review_snapshot(pane.app.conversation_projection_mut(), &state);
                        pane.app.restore_presentation_selection(anchor);
                    }
                    pane.app.bind_worker_review(target.clone(), state.clone());
                    if !live {
                        pane.app.freeze_worker();
                    }
                    pane.refresh_pane_metadata();
                }
                self.apply_root_event(SessionEvent::WorkerReviewUpdated { target, state });
            }
            SessionEvent::AgentRunUpdated {
                turn_id,
                ensemble_run_id,
                agent_run_id,
                event,
            } => {
                if let Some(index) = self.agents.iter().position(|pane| {
                    pane.id == agent_run_id && pane.ensemble_run_id == ensemble_run_id
                }) {
                    let pane = &mut self.agents[index];
                    if pane.status != AgentRunStatus::Abandoned {
                        if let AgentRunEvent::Status { status, .. } = &event {
                            pane.status = *status;
                        }
                        pane.apply_transcript_event(event.clone());
                    }
                    pane.refresh_pane_metadata();
                }
                self.apply_root_event(SessionEvent::AgentRunUpdated {
                    turn_id,
                    ensemble_run_id,
                    agent_run_id,
                    event,
                });
            }
            SessionEvent::AgentRunFinished {
                turn_id,
                ensemble_run_id,
                outcome,
            } => {
                if let Some(index) = self.agents.iter().position(|pane| {
                    pane.id == outcome.descriptor.id && pane.ensemble_run_id == ensemble_run_id
                }) {
                    let reviewed = self
                        .root
                        .reviewed_worker_status(&ensemble_run_id, &outcome.descriptor.id);
                    let pane = &mut self.agents[index];
                    if let Some(usage) = &outcome.usage {
                        pane.usage = Some(external_context_usage(usage));
                    }
                    if pane.status == AgentRunStatus::Abandoned || reviewed.is_some() {
                        // Review authority includes revocation and current
                        // generations. A late outcome must not freeze a newer
                        // draft/round or replay an old confirmation into it.
                        self.apply_root_event(SessionEvent::AgentRunFinished {
                            turn_id,
                            ensemble_run_id,
                            outcome,
                        });
                        return;
                    }
                    pane.status = outcome.status;
                    pane.app.freeze_worker();
                    if let Some(usage) = &outcome.usage {
                        pane.usage = Some(external_context_usage(usage));
                    }
                    pane.reconcile_outcome_evidence(
                        &outcome.report,
                        outcome.plan.as_ref(),
                        outcome.confirmation.as_deref(),
                    );
                    if let Some(error) = &outcome.failure
                        && !pane.app.contains_presented_error(error)
                    {
                        pane.apply_transcript_event(AgentRunEvent::Failure {
                            error: error.clone(),
                        });
                    }
                    pane.refresh_pane_metadata();
                }
                self.apply_root_event(SessionEvent::AgentRunFinished {
                    turn_id,
                    ensemble_run_id,
                    outcome,
                });
            }
            event => self.apply_root_event(event),
        }
        if refresh_workspace_header {
            self.workspace_header.refresh();
        }
    }

    fn apply_root_event(&mut self, event: SessionEvent) {
        let effects = self.root.reduce(event);
        self.consume_root_effects(effects);
        // A single root ID drives every badge, including panes without a live
        // review binding. Sidecar text never changes current selection.
        for pane in &mut self.agents {
            let selected = self.root.ensemble_baseline(&pane.ensemble_run_id);
            if let Some(id) = selected {
                self.baseline_workers
                    .insert(pane.ensemble_run_id.clone(), id.clone());
            } else {
                self.baseline_workers.remove(&pane.ensemble_run_id);
            }
            pane.baseline = selected == Some(&pane.id);
            if let Some(status) = self
                .root
                .reviewed_worker_status(&pane.ensemble_run_id, &pane.id)
            {
                pane.status = status;
            }
            pane.refresh_pane_metadata();
        }
    }

    fn consume_root_effects(&mut self, effects: Vec<AppEffect>) {
        for effect in effects {
            match effect {
                AppEffect::PruneEnsembleRuns(run_ids) => {
                    self.prune_discarded_agent_panes(run_ids);
                }
            }
        }
    }

    /// Remove only worker panes whose durable root starts were discarded by
    /// an accepted tail rewrite. Historical JSONL files remain untouched.
    fn prune_discarded_agent_panes(&mut self, run_ids: Vec<EnsembleRunId>) {
        if run_ids.is_empty() {
            return;
        }
        let run_ids: HashSet<_> = run_ids.into_iter().collect();
        self.agents
            .retain(|pane| !run_ids.contains(&pane.ensemble_run_id));
        self.set_visible(self.visible.filter(|pane| self.contains_pane(*pane)));
        self.last_entered = self.last_entered.filter(|pane| self.contains_pane(*pane));
    }

    fn contains_pane(&self, pane: VisiblePane) -> bool {
        match pane {
            VisiblePane::Subtask(id) => self
                .children
                .iter()
                .any(|pane| pane.app.surface().id.pane == id),
            VisiblePane::Agent(id) => self
                .agents
                .iter()
                .any(|pane| pane.app.surface().id.pane == id),
        }
    }

    pub fn apply_streams(&mut self, batch: &SessionStreamBatch) {
        if let Some(stream) = &batch.root {
            self.apply_root_event(if stream.message.is_some() || stream.attempt.is_some() {
                SessionEvent::AssistantStreamUpdated {
                    turn_id: stream.turn_id,
                    snapshot: zevria_content::AssistantStreamSnapshot {
                        message: stream.message.clone(),
                        attempt: stream.attempt.clone(),
                    },
                }
            } else {
                SessionEvent::StreamCleared {
                    turn_id: stream.turn_id,
                }
            });
        }
        for (id, stream) in &batch.subtasks {
            let index = self.ensure_child(id);
            let effects = self.children[index].app.reduce(
                if stream.message.is_some() || stream.attempt.is_some() {
                    SessionEvent::AssistantStreamUpdated {
                        turn_id: stream.turn_id,
                        snapshot: zevria_content::AssistantStreamSnapshot {
                            message: stream.message.clone(),
                            attempt: stream.attempt.clone(),
                        },
                    }
                } else {
                    SessionEvent::StreamCleared {
                        turn_id: stream.turn_id,
                    }
                },
            );
            debug_assert!(
                effects.is_empty(),
                "inspect panes cannot prune root workers"
            );
        }
        for stream in &batch.agent_runs {
            if let Some(index) = self.agents.iter().position(|pane| {
                pane.id == stream.agent_run_id && pane.ensemble_run_id == stream.ensemble_run_id
            }) {
                let pane = &mut self.agents[index];
                pane.apply_transcript_preview(stream.event.clone());
                pane.refresh_pane_metadata();
            }
        }
    }

    /// Resolve one owner before mutation. Capturing surfaces never delegate
    /// ignored keys; pane shortcuts are allowed only by that owner's policy.
    pub fn handle_event(&mut self, event: Event) -> Option<UiAction> {
        let action = self.handle_workspace_event(event);
        self.file_search_request();
        action
    }

    fn handle_workspace_event(&mut self, event: Event) -> Option<UiAction> {
        let input = match crate::input::normalize(event) {
            InputEvent::User(input) => input,
            InputEvent::Resize => {
                self.overlays.invalidate_geometry();
                self.visible_app_mut().invalidate_rendered_geometry();
                return None;
            }
            InputEvent::Focus { gained } => {
                self.visible_app_mut().close_composer_edit_group();
                if gained {
                    self.workspace_header.refresh();
                }
                return None;
            }
            InputEvent::Ignored => return None,
        };
        let owner = self.overlays.snapshot(self.visible_app().surface()).owner;
        if owner.captures {
            self.visible_app_mut().close_composer_edit_group();
            if self.overlays.owns(owner.id.kind) {
                return self.overlays.handle_input(owner, input);
            }
            return self
                .visible_app_mut()
                .handle_input_on(owner, input, Instant::now());
        }
        if let UserInput::Key(key) = &input {
            match owner.action(*key) {
                Some(Action::Help) => {
                    let eligibility = self.visible_app().hint_eligibility();
                    self.visible_app_mut().close_composer_edit_group();
                    self.overlays.show_help(owner.context(), &eligibility);
                    return None;
                }
                Some(Action::ReturnRoot) => {
                    self.set_visible(None);
                    return None;
                }
                Some(Action::LatestPane) => {
                    self.open_latest_child();
                    return None;
                }
                _ => {}
            }
        }
        match self
            .visible_app_mut()
            .handle_input_on(owner, input, Instant::now())
        {
            Some(UiAction::OpenSubtask { id }) => {
                self.open_child(&id);
                None
            }
            Some(UiAction::OpenAgentRun { id }) => {
                self.open_agent(&id);
                None
            }
            action => action,
        }
    }

    fn open_child(&mut self, id: &SubtaskId) {
        if let Some(index) = self.children.iter().position(|child| &child.id == id) {
            let pane = VisiblePane::Subtask(self.children[index].app.surface().id.pane);
            self.set_visible(Some(pane));
            self.last_entered = Some(pane);
        }
    }

    fn open_agent(&mut self, id: &AgentRunId) {
        if let Some(index) = self.agents.iter().position(|pane| &pane.id == id) {
            let pane = VisiblePane::Agent(self.agents[index].app.surface().id.pane);
            self.set_visible(Some(pane));
            self.last_entered = Some(pane);
        }
    }

    fn open_latest_child(&mut self) {
        let target = self
            .last_entered
            .filter(|pane| self.contains_pane(*pane))
            .or_else(|| {
                let child = self
                    .children
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, pane)| pane.creation_ordinal)
                    .map(|(_, pane)| {
                        (
                            pane.creation_ordinal,
                            VisiblePane::Subtask(pane.app.surface().id.pane),
                        )
                    });
                let agent = self
                    .agents
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, pane)| pane.creation_ordinal)
                    .map(|(_, pane)| {
                        (
                            pane.creation_ordinal,
                            VisiblePane::Agent(pane.app.surface().id.pane),
                        )
                    });
                match (child, agent) {
                    (Some(child), Some(agent)) => {
                        Some(if child.0 > agent.0 { child.1 } else { agent.1 })
                    }
                    (Some((_, pane)), None) | (None, Some((_, pane))) => Some(pane),
                    (None, None) => None,
                }
            });
        if let Some(pane) = target {
            self.set_visible(Some(pane));
            self.last_entered = Some(pane);
        }
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &App {
        &self.root
    }

    #[cfg(test)]
    pub(crate) fn visible_child_id(&self) -> Option<&SubtaskId> {
        match self.visible {
            Some(VisiblePane::Subtask(id)) => self
                .children
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| &pane.id),
            Some(VisiblePane::Agent(_)) | None => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn child(&self, id: &SubtaskId) -> Option<&App> {
        self.children
            .iter()
            .find(|child| &child.id == id)
            .map(|child| &child.app)
    }

    #[cfg(test)]
    pub(crate) fn visible_agent_id(&self) -> Option<&AgentRunId> {
        match self.visible {
            Some(VisiblePane::Agent(id)) => self
                .agents
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| &pane.id),
            Some(VisiblePane::Subtask(_)) | None => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn agent(&self, id: &AgentRunId) -> Option<&App> {
        self.agents
            .iter()
            .find(|agent| &agent.id == id)
            .map(|agent| &agent.app)
    }
}
