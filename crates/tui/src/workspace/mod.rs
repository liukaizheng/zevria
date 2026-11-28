//! Pane lifetime, global surfaces, restoration and engine/input routing.

use crate::app::overlays::{OverlayController, SurfaceSnapshot};
use zevria_session_api::{EnsembleEvent, JobEvent, ManagementEvent, SubtaskEvent, TurnEvent};

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{crossterm::event::Event, style::Style};
use tokio::sync::mpsc::UnboundedSender;
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskLaunchMetadata;
use zevria_foundation::subtask::SubtaskStatus;
use zevria_foundation::{SideChatDescriptor, SideChatId, SideChatStatus};
use zevria_model::models::ModelSelectionScope;
use zevria_presentation::side_chats::{
    RestoredSidePane, SideChatPane, extract_side_chat_references_from, format_quote_for_main,
    new_side_chat, side_chat_answers,
};
use zevria_presentation::subtasks::active_count;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionStreamBatch;
use zevria_session_api::{ControlCommand, SessionCommand, SideChatCommand, SideChatEvent};
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
use crate::input::{Action, InputEvent, PaneId, SurfaceKind, UserInput};
use crate::picker::SessionPicker;
use crate::question::QuestionDialog;
use crate::status::ExternalContextUsage;
use crate::subtasks::SubtaskRow;
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
pub(crate) struct ChildPane {
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
pub(crate) struct AgentPane {
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
    SideChat(SideChatId),
    SideChatExcerpt,
    Agent(EnsembleRunId, AgentRunId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisiblePane {
    SideChat(SideChatId),
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

struct NativeSidePane {
    descriptor: SideChatDescriptor,
    app: App,
}

/// The root pane plus live/historical child panes and their navigation state.
pub struct SessionViews {
    pub(crate) workspace: zevria_presentation::workspace::WorkspaceModel<
        App,
        ChildPane,
        AgentPane,
        OverlayController,
    >,
    side_threads: Vec<NativeSidePane>,
    side_completion: usize,
    dismissed_side_query: Option<String>,
    pending_side_request: Option<String>,
    side_command: Option<SessionCommand>,
    model_profiles: Arc<ConfiguredModelProfiles>,
    workspace_header: WorkspaceHeader,
    pub(crate) startup_workspace: PathBuf,
    pub(crate) file_service_id: crate::workspace_files::ServiceId,
    file_owner: Option<PaneId>,
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
}

impl SessionViews {
    fn surface_snapshot(&self, pane: &App) -> SurfaceSnapshot {
        self.workspace.controls.snapshot(
            pane.surface(),
            self.workspace.root.jobs_inspector().is_open(),
        )
    }

    pub(crate) fn overlay_owns(&self, kind: SurfaceKind) -> bool {
        self.workspace
            .controls
            .owns(kind, self.workspace.root.jobs_inspector().is_open())
    }

    /// Root follow-ups drain from any pane, but not while a modal captures input.
    pub(crate) fn dispatch_queued_root_work(&mut self) -> Option<UiAction> {
        if self.surface_snapshot(&self.workspace.root).owner.captures {
            return None;
        }
        let action = self
            .workspace
            .root
            .dispatch_next_queued()
            .or_else(|| self.workspace.root.dispatch_job_continuation());
        self.consume_view_action(action)
    }

    pub fn new(mut root: App, startup_workspace: PathBuf) -> Self {
        // Capture relative API inputs now, not against a later process cwd.
        // Canonicalization and all discovery still belong to the file worker.
        let startup_workspace =
            std::path::absolute(&startup_workspace).unwrap_or(startup_workspace);
        // A prior standalone render did not reserve the workspace header.
        root.invalidate_rendered_geometry();
        let model_profiles = root.shared_model_profiles();
        Self {
            workspace: zevria_presentation::workspace::WorkspaceModel::new(
                root,
                OverlayController::default(),
            ),
            side_threads: Vec::new(),
            side_completion: 0,
            dismissed_side_query: None,
            pending_side_request: None,
            side_command: None,
            model_profiles,
            workspace_header: WorkspaceHeader::new(startup_workspace.clone()),
            startup_workspace,
            file_service_id: crate::workspace_files::ServiceId::default(),
            file_owner: None,
            visible: None,
            last_entered: None,
            next_creation_ordinal: 0,
            restored_subtask_ordinals: HashMap::new(),
            restored_agent_ordinals: HashMap::new(),
            abandoned_workers: HashMap::new(),
            baseline_workers: HashMap::new(),
        }
    }

    /// Move restored history into the root after seeding child order, without
    /// cloning all restored message and native replay payloads in the frontend.
    pub fn new_restored(
        root: App,
        startup_workspace: PathBuf,
        input: crate::RestorationInput,
    ) -> Self {
        let mut views = Self::new(root, startup_workspace);
        views.seed_child_creation_order(&input.items);
        views.workspace.root.restore_session(input);
        views.model_profiles = views.workspace.root.shared_model_profiles();
        views
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
                    .find(|state| state.proposals.baseline.is_some() && !state.is_abandoned())
                {
                    self.baseline_workers
                        .insert(run_id, state.descriptor.id.clone());
                }
            }
        }
        for item in items {
            match item.ensemble_record() {
                Some(zevria_workflow::EnsembleRecord::Started { start }) => {
                    if let Some(worker_id) = self.workspace.root.ensemble_baseline(&start.run_id) {
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
                Some(zevria_workflow::EnsembleRecord::WorkerReview {
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
                Some(zevria_workflow::EnsembleRecord::WorkersConfirmed {
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
                _ => {}
            }
            if let TranscriptItem::ToolResults { metadata, .. } = item {
                for entry in metadata.iter().flat_map(|metadata| metadata.subtasks()) {
                    if let Some(subtask) = &entry.launch {
                        self.seed_subtask_ordinal(&subtask.id);
                        self.install_subtask_metadata(subtask, entry.status);
                        let index = self.ensure_child(&subtask.id);
                        self.workspace.children[index].historical = true;
                        self.workspace.children[index].refresh_pane_metadata();
                    }
                }
            }
        }
        self.refresh_subtasks();
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
                self.workspace.agents[index].historical = true;
                self.workspace.agents[index].app.freeze_worker();
                pane_index = Some(index);
            }
            let Some(index) = pane_index else {
                continue;
            };
            let pane = &mut self.workspace.agents[index];
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
        let pane = &mut self.workspace.agents[index];
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
            let pane = &mut self.workspace.children[index];
            pane.kind = metadata.kind;
            pane.title = metadata.title;
            pane.workspace = metadata.workspace;
            pane.app.set_inspect_model_role(pane.kind.model_role());
        }
        self.workspace.children[index].historical = true;
        self.workspace.children[index].app.restore(items);
        self.workspace.children[index].refresh_pane_metadata();
        self.refresh_subtasks();
    }

    fn install_subtask_metadata(
        &mut self,
        metadata: &SubtaskLaunchMetadata,
        status: SubtaskStatus,
    ) {
        let index = self.ensure_child(&metadata.id);
        let pane = &mut self.workspace.children[index];
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
        if let Some(index) = self
            .workspace
            .children
            .iter()
            .position(|child| &child.id == id)
        {
            return index;
        }
        let app = App::subtask_inspect("")
            .with_shared_model_profiles(Arc::clone(&self.model_profiles))
            .with_inspect_reasoning_from(&self.workspace.root);
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
        self.workspace.children.push(pane);
        self.workspace.children.len() - 1
    }

    fn ensure_agent(
        &mut self,
        ensemble_run_id: &EnsembleRunId,
        descriptor: &AgentRunDescriptor,
    ) -> usize {
        if let Some(index) = self
            .workspace
            .agents
            .iter()
            .position(|pane| pane.id == descriptor.id)
        {
            return index;
        }
        let app = App::acp_inspect("")
            .with_shared_model_profiles(Arc::clone(&self.model_profiles))
            .with_inspect_reasoning_from(&self.workspace.root);
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
                .or_else(|| self.workspace.root.ensemble_baseline(ensemble_run_id))
                == Some(&descriptor.id),
        };
        pane.refresh_pane_metadata();
        self.workspace.agents.push(pane);
        self.workspace.agents.len() - 1
    }

    /// Reconcile the visible input owner before requesting or applying results.
    /// Hidden, retired, frozen, and overlay-owned drafts cannot receive searches.
    pub(crate) fn file_search_request(&mut self) -> Option<crate::workspace_files::SearchRequest> {
        let owner = self.surface_snapshot(self.visible_app()).owner;
        let pane = (!owner.captures).then_some(owner.id.pane);
        if self.file_owner != pane {
            if let Some(previous) = self.file_owner {
                if self.workspace.root.surface().id.pane == previous {
                    self.workspace.root.suspend_file_completion();
                } else if let Some(agent) = self
                    .workspace
                    .agents
                    .iter_mut()
                    .find(|agent| agent.app.surface().id.pane == previous)
                {
                    agent.app.suspend_file_completion();
                } else if let Some(thread) = self
                    .side_threads
                    .iter_mut()
                    .find(|thread| thread.app.surface().id.pane == previous)
                {
                    thread.app.suspend_file_completion();
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

    pub(crate) fn take_side_command(&mut self) -> Option<SessionCommand> {
        self.side_command.take()
    }

    pub(crate) fn clipboard_origin(&self) -> Option<ClipboardOrigin> {
        match self.visible {
            None => Some(ClipboardOrigin::Root),
            Some(VisiblePane::Agent(id)) => self
                .workspace
                .agents
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| ClipboardOrigin::Agent(pane.ensemble_run_id.clone(), pane.id.clone())),
            Some(VisiblePane::Subtask(_)) => None,
            Some(VisiblePane::SideChat(id)) => Some(ClipboardOrigin::SideChat(id)),
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
            ClipboardOrigin::SideChatExcerpt => {
                match result {
                    crate::clipboard::ClipboardResult::Text(text) if !text.trim().is_empty() => {
                        self.side_command = Some(self.create_side(None, Some(text)));
                    }
                    crate::clipboard::ClipboardResult::Error(error) => self.show_root_error(error),
                    _ => self.show_root_error(
                        "Copy text in your terminal before asking in Side Chat.".into(),
                    ),
                }
                return;
            }
            ClipboardOrigin::SideChat(id) => self
                .side_threads
                .iter_mut()
                .find(|pane| pane.descriptor.id == id)
                .map(|pane| &mut pane.app),
            ClipboardOrigin::Root => Some(&mut self.workspace.root),
            ClipboardOrigin::Agent(run, id) => self
                .workspace
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
            Some(VisiblePane::SideChat(id)) => {
                &mut self
                    .side_threads
                    .iter_mut()
                    .find(|pane| pane.descriptor.id == id)
                    .expect("live side pane")
                    .app
            }
            Some(VisiblePane::Subtask(id)) => {
                &mut self
                    .workspace
                    .children
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            Some(VisiblePane::Agent(id)) => {
                &mut self
                    .workspace
                    .agents
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            None => &mut self.workspace.root,
        }
    }

    fn set_visible(&mut self, visible: Option<VisiblePane>) {
        if self.visible != visible {
            // Tail rewrites may already have removed the outgoing worker.
            let outgoing = match self.visible {
                Some(VisiblePane::SideChat(id)) => self
                    .side_threads
                    .iter_mut()
                    .find(|pane| pane.descriptor.id == id)
                    .map(|pane| &mut pane.app),
                Some(VisiblePane::Subtask(index)) => self
                    .workspace
                    .children
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == index)
                    .map(|pane| &mut pane.app),
                Some(VisiblePane::Agent(index)) => self
                    .workspace
                    .agents
                    .iter_mut()
                    .find(|pane| pane.app.surface().id.pane == index)
                    .map(|pane| &mut pane.app),
                None => Some(&mut self.workspace.root),
            };
            if let Some(app) = outgoing {
                app.clear_text_selection();
                app.close_composer_edit_group();
            }
            self.visible = visible;
            self.visible_app_mut().close_composer_edit_group();
            self.visible_app_mut().invalidate_rendered_geometry();
        }
    }

    fn visible_app(&self) -> &App {
        match self.visible {
            Some(VisiblePane::SideChat(id)) => {
                &self
                    .side_threads
                    .iter()
                    .find(|pane| pane.descriptor.id == id)
                    .expect("live side pane")
                    .app
            }
            Some(VisiblePane::Subtask(id)) => {
                &self
                    .workspace
                    .children
                    .iter()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            Some(VisiblePane::Agent(id)) => {
                &self
                    .workspace
                    .agents
                    .iter()
                    .find(|pane| pane.app.surface().id.pane == id)
                    .expect("live pane identity")
                    .app
            }
            None => &self.workspace.root,
        }
    }

    /// Observe only the visible pane; hidden native panes still observe lifecycle receipt.
    pub(crate) fn observe_clock(&mut self, now: Instant) {
        self.visible_app_mut().observe_clock(now);
        for pane in &mut self.side_threads {
            if self.visible != Some(VisiblePane::SideChat(pane.descriptor.id)) {
                pane.app.observe_clock(now);
            }
        }
    }

    pub(crate) fn clock_required(&self) -> bool {
        self.visible_app().is_busy()
            || self
                .side_threads
                .iter()
                .any(|pane| pane.descriptor.status == SideChatStatus::Running)
            || self.workspace.root.jobs_inspector().is_open()
            || (self.workspace.controls.subtasks.is_open()
                && self.workspace.controls.subtasks.has_active_rows())
    }

    fn subtask_rows(&self) -> Vec<SubtaskRow> {
        let mut children = self.workspace.children.iter().collect::<Vec<_>>();
        children.sort_unstable_by_key(|pane| pane.creation_ordinal);
        children
            .into_iter()
            .map(|pane| SubtaskRow {
                id: pane.id.clone(),
                kind: pane.kind,
                title: pane.title.clone(),
                workspace: pane.workspace.clone(),
                status: pane.status,
                historical: pane.historical,
                visible: self.visible == Some(VisiblePane::Subtask(pane.app.surface().id.pane)),
            })
            .collect()
    }

    pub(crate) fn show_jobs(&mut self) {
        self.workspace
            .controls
            .show_jobs(self.workspace.root.jobs_inspector_mut());
    }

    pub(crate) fn show_subtasks(&mut self) {
        self.workspace.root.jobs_inspector_mut().close();
        self.visible_app_mut().close_composer_edit_group();
        self.workspace.controls.show_subtasks(self.subtask_rows());
    }

    fn refresh_subtask_rows(&mut self) {
        if self.workspace.controls.subtasks.is_open() {
            self.workspace
                .controls
                .subtasks
                .replace_rows(self.subtask_rows());
        }
    }

    fn refresh_subtasks(&mut self) {
        self.workspace.root.set_active_subtasks(active_count(
            self.workspace.children.iter().map(|pane| pane.status),
        ));
        self.refresh_subtask_rows();
    }

    /// Open the modal session picker over whatever pane is visible.
    pub fn open_session_picker(&mut self, sessions: Vec<SessionSummary>) {
        self.workspace
            .controls
            .show_sessions(SessionPicker::new(sessions));
    }

    /// Surface a frontend-side failure as a root-pane notice.
    pub fn show_root_error(&mut self, error: String) {
        self.workspace.root.push_error(error);
    }

    /// Draw the currently visible pane and global workspace chrome, then any
    /// modal overlay above the protected header row.
    pub fn render(&mut self, frame: &mut ratatui::Frame) {
        self.refresh_subtask_rows();
        self.workspace
            .root
            .set_side_chat_count(self.side_threads.len());
        if self.overlay_owns(SurfaceKind::SideChats) {
            self.workspace.controls.update_side_chats(self.side_rows());
        }
        let accent = self.visible_app().status_accent().color();
        let snapshot = self.surface_snapshot(self.visible_app());
        let owner = snapshot.owner;
        let cursor = !owner.captures && owner.cursor;
        let layout = self.visible_app_mut().render_surface(frame, true, cursor);
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
        self.workspace.controls.render(
            &snapshot,
            self.workspace.root.jobs_inspector(),
            frame,
            layout.modal_body,
        );
        if !owner.captures {
            let matches = self.side_completions();
            crate::side_chats::render_completions(
                frame,
                layout.modal_body,
                layout.prompt,
                &matches,
                self.side_completion,
            );
        }
    }

    pub(crate) fn open_model_picker(&mut self, scope: ModelSelectionScope) {
        if self.workspace.controls.models.is_open() {
            return;
        }
        if zevria_presentation::workspace::model_picker_gate(
            self.visible.is_none(),
            false,
            self.workspace.controls.question().is_some(),
        )
        .is_ok()
            && !self.surface_snapshot(&self.workspace.root).owner.captures
            && let Some(mode) = self.workspace.root.begin_model_management()
        {
            self.workspace.controls.show_models(mode, scope);
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
            self.apply_root_event(SessionEvent::Manage(ManagementEvent::ModeResult {
                request_id,
                result: zevria_session_api::ModeSelectionResult::Rejected {
                    code: "engine_stopped".into(),
                    message: "The session engine stopped before accepting mode selection.".into(),
                },
            }));
        }
    }

    /// Route an engine event: tagged child traffic reduces into its hidden
    /// pane, everything else into the root pane.
    pub fn apply(&mut self, event: SessionEvent) {
        self.apply_event(event);
    }

    fn apply_event(&mut self, event: SessionEvent) {
        if let SessionEvent::SideChat(event) = event {
            self.apply_side(event);
            return;
        }
        if matches!(&event, SessionEvent::Plan(zevria_session_api::PlanEvent::StateChanged { state }) if matches!(state.as_ref(), zevria_workflow::PlanWorkflowState::Ready { .. }))
        {
            self.workspace.controls.close_side_chats();
        }
        if matches!(
            &event,
            SessionEvent::Job(JobEvent::Updated { .. } | JobEvent::ControlResult { .. })
        ) {
            let anchor = self
                .workspace
                .controls
                .jobs
                .selection_anchor(self.workspace.root.jobs_inspector());
            self.apply_root_event(event);
            self.workspace
                .controls
                .jobs
                .reconcile(self.workspace.root.jobs_inspector(), anchor);
            return;
        }
        if let SessionEvent::Manage(ManagementEvent::ModelsResult { request_id, result }) = &event {
            if self.workspace.controls.models.accept(request_id, result) {
                self.workspace.root.install_validated_model_result(result);
                if let Some(notice) = self.workspace.controls.models.take_settlement_notice() {
                    self.workspace.root.push_error(notice);
                }
                self.model_profiles = self.workspace.root.shared_model_profiles();
                if !self.workspace.controls.models.is_open() {
                    self.workspace.root.finish_model_management();
                }
            }
            return;
        }
        if let Some(entries) = self.workspace.controls.skills.event(&event) {
            self.workspace.root.replace_skill_entries(entries);
        }
        if self.workspace.controls.mcp.inspector.event(&event) {
            self.workspace
                .root
                .replace_mcp_prompts(&self.workspace.controls.mcp.inspector.prompts);
            return;
        }
        let refresh_workspace_header =
            matches!(&event, SessionEvent::Turn(TurnEvent::ToolResults { .. }));
        let terminal_turn = zevria_presentation::workspace::accepted_terminal_turn(&event, |id| {
            self.workspace.root.accepts_terminal(id)
        });
        if terminal_turn.is_some() {
            for pane in &mut self.workspace.agents {
                pane.app.freeze_worker();
            }
        }
        if terminal_turn.is_some_and(|turn_id| {
            self.workspace
                .controls
                .question()
                .is_some_and(|question| question.turn_id() == turn_id)
        }) {
            self.workspace.controls.set_question(None);
        }
        let child_count = self.workspace.children.len();
        let mut refresh_subtasks = matches!(
            &event,
            SessionEvent::Subtask(SubtaskEvent::Launched { .. })
                | SessionEvent::Subtask(SubtaskEvent::Status { .. })
        );
        if let SessionEvent::Turn(TurnEvent::ToolResults { metadata, .. }) = &event {
            for entry in metadata.iter().flat_map(|metadata| metadata.subtasks()) {
                if let Some(launch) = &entry.launch {
                    self.install_subtask_metadata(launch, entry.status);
                    refresh_subtasks = true;
                }
            }
        }
        match event {
            SessionEvent::Question(event) => {
                use zevria_presentation::workspace::{QuestionRoute, route_question};
                let active = self
                    .workspace
                    .controls
                    .question()
                    .map(|question| (question.turn_id(), question.request_id()));
                // Resolve correlation before capturing focus. A delayed
                // question cannot reopen after its turn or steal management.
                match route_question(event, active, |id| self.workspace.root.accepts_progress(id)) {
                    QuestionRoute::Open { turn_id, request } => self
                        .workspace
                        .controls
                        .set_question(Some(QuestionDialog::new(turn_id, request))),
                    QuestionRoute::Close => self.workspace.controls.set_question(None),
                    QuestionRoute::Ignore => {}
                }
            }
            SessionEvent::Subtask(SubtaskEvent::Launched {
                turn_id,
                call_id,
                entry_index,
                mut descriptor,
            }) => {
                let index = self.ensure_child(&descriptor.id);
                let pane = &mut self.workspace.children[index];
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
                self.apply_root_event(SessionEvent::Subtask(SubtaskEvent::Launched {
                    turn_id,
                    call_id,
                    entry_index,
                    descriptor,
                }));
            }
            SessionEvent::Subtask(SubtaskEvent::Session { id, event }) => {
                let index = self.ensure_child(&id);
                let effects = self.workspace.children[index].app.reduce(*event);
                debug_assert!(
                    effects.is_empty(),
                    "inspect panes cannot prune root workers"
                );
            }
            SessionEvent::Subtask(SubtaskEvent::Status {
                turn_id,
                id,
                status,
            }) => {
                let index = self.ensure_child(&id);
                if self.workspace.children[index]
                    .status
                    .is_none_or(|previous| {
                        !previous.is_terminal() && status != SubtaskStatus::Starting
                    })
                {
                    self.workspace.children[index].status = Some(status);
                }
                let status = self.workspace.children[index]
                    .status
                    .expect("child status installed");
                self.workspace.children[index].refresh_pane_metadata();
                self.apply_root_event(SessionEvent::Subtask(SubtaskEvent::Status {
                    turn_id,
                    id,
                    status,
                }));
            }
            SessionEvent::Ensemble(EnsembleEvent::Started {
                turn_id,
                start,
                resumed,
            }) => {
                self.apply_root_event(SessionEvent::Ensemble(EnsembleEvent::Started {
                    turn_id,
                    start: start.clone(),
                    resumed,
                }));
                for descriptor in &start.agents {
                    if self
                        .abandoned_workers
                        .contains_key(&(start.run_id.clone(), descriptor.id.clone()))
                        && !self.workspace.agents.iter().any(|pane| {
                            pane.id == descriptor.id && pane.ensemble_run_id == start.run_id
                        })
                    {
                        continue;
                    }
                    let index = self.ensure_agent(&start.run_id, descriptor);
                    let pane = &mut self.workspace.agents[index];
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
            SessionEvent::Ensemble(EnsembleEvent::WorkerControlResult { result }) => {
                if let Some(pane) = self.workspace.agents.iter_mut().find(|pane| {
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
            SessionEvent::Ensemble(EnsembleEvent::WorkerReviewUpdated { target, state }) => {
                let live = self.workspace.root.active_turn_id() == Some(target.turn_id);
                if let Some(pane) = self.workspace.agents.iter_mut().find(|pane| {
                    pane.id == target.worker_id
                        && pane.ensemble_run_id == target.run_id
                        && !pane.historical
                }) {
                    pane.status = state.status();
                    if state.is_abandoned() {
                        // Root authority also freezes panes when no worker audit
                        // mirror made it to disk. Keep preceding transcript text.
                        let change = pane
                            .transcript
                            .abandon(pane.app.conversation_projection_mut());
                        pane.app.apply_conversation_change(change);
                    }
                    if !state.is_abandoned()
                        && state.quiescent()
                        && let Some(snapshot) = &state.proposals.retained
                    {
                        pane.reconcile_plan(&snapshot.plan);
                    }
                    if !state.is_abandoned() {
                        let anchor = pane.app.presentation_selection_anchor();
                        pane.transcript
                            .apply_review_snapshot(pane.app.conversation_projection_mut(), &state);
                        pane.app.reconcile_fold_structure();
                        pane.app.restore_presentation_selection(anchor);
                    }
                    pane.app.bind_worker_review(target.clone(), state.clone());
                    if !live {
                        pane.app.freeze_worker();
                    }
                    pane.refresh_pane_metadata();
                }
                self.apply_root_event(SessionEvent::Ensemble(EnsembleEvent::WorkerReviewUpdated {
                    target,
                    state,
                }));
            }
            SessionEvent::Ensemble(EnsembleEvent::AgentRunUpdated {
                turn_id,
                ensemble_run_id,
                agent_run_id,
                event,
            }) => {
                let status = match &*event {
                    AgentRunEvent::Status { status, .. } => Some(*status),
                    _ => None,
                };
                let root_status = if let AgentRunEvent::Status { status, detail } = &*event {
                    Some(AgentRunEvent::Status {
                        status: *status,
                        detail: detail.clone(),
                    })
                } else {
                    None
                };
                if let Some(pane) =
                    self.workspace.agents.iter_mut().find(|pane| {
                        pane.id == agent_run_id && pane.ensemble_run_id == ensemble_run_id
                    })
                {
                    if pane.status != AgentRunStatus::Abandoned {
                        let refresh =
                            status.is_some() || matches!(&*event, AgentRunEvent::Usage { .. });
                        if let Some(status) = status {
                            pane.status = status;
                        }
                        pane.apply_transcript_event(*event);
                        if refresh {
                            pane.refresh_pane_metadata();
                        }
                    }
                }
                if let Some(event) = root_status {
                    self.apply_root_event(SessionEvent::Ensemble(EnsembleEvent::AgentRunUpdated {
                        turn_id,
                        ensemble_run_id,
                        agent_run_id,
                        event: Box::new(event),
                    }));
                }
            }
            SessionEvent::Ensemble(EnsembleEvent::AgentRunFinished {
                turn_id,
                ensemble_run_id,
                outcome,
            }) => {
                if let Some(index) = self.workspace.agents.iter().position(|pane| {
                    pane.id == outcome.descriptor.id && pane.ensemble_run_id == ensemble_run_id
                }) {
                    let reviewed = self
                        .workspace
                        .root
                        .reviewed_worker_status(&ensemble_run_id, &outcome.descriptor.id);
                    let pane = &mut self.workspace.agents[index];
                    if let Some(usage) = &outcome.usage {
                        pane.usage = Some(external_context_usage(usage));
                    }
                    if pane.status == AgentRunStatus::Abandoned || reviewed.is_some() {
                        // Review authority includes revocation and current
                        // generations. A late outcome must not freeze a newer
                        // draft/round or replay an old confirmation into it.
                        self.apply_root_event(SessionEvent::Ensemble(
                            EnsembleEvent::AgentRunFinished {
                                turn_id,
                                ensemble_run_id,
                                outcome,
                            },
                        ));
                        return;
                    }
                    pane.status = outcome.status;
                    pane.app.freeze_worker();
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
                self.apply_root_event(SessionEvent::Ensemble(EnsembleEvent::AgentRunFinished {
                    turn_id,
                    ensemble_run_id,
                    outcome,
                }));
            }
            event => self.apply_root_event(event),
        }
        if refresh_subtasks || self.workspace.children.len() != child_count {
            self.refresh_subtasks();
        }
        if refresh_workspace_header {
            self.workspace_header.refresh();
        }
    }

    fn apply_root_event(&mut self, event: SessionEvent) {
        let sync_agents = match &event {
            SessionEvent::Ensemble(
                EnsembleEvent::Started { .. }
                | EnsembleEvent::ReportsReady { .. }
                | EnsembleEvent::AgentRunFinished { .. }
                | EnsembleEvent::WorkerReviewUpdated { .. },
            ) => true,
            SessionEvent::Ensemble(EnsembleEvent::AgentRunUpdated { event, .. }) => {
                matches!(event.as_ref(), AgentRunEvent::Status { .. })
            }
            _ => false,
        };
        let effects = self.workspace.root.reduce(event);
        let sync_agents = sync_agents || !effects.is_empty();
        self.consume_root_effects(effects);
        if !sync_agents {
            return;
        }
        // A single root ID drives every badge, including panes without a live
        // review binding. Sidecar text never changes current selection.
        for pane in &mut self.workspace.agents {
            let selected = self.workspace.root.ensemble_baseline(&pane.ensemble_run_id);
            if let Some(id) = selected {
                self.baseline_workers
                    .insert(pane.ensemble_run_id.clone(), id.clone());
            } else {
                self.baseline_workers.remove(&pane.ensemble_run_id);
            }
            pane.baseline = selected == Some(&pane.id);
            if let Some(status) = self
                .workspace
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
        self.workspace
            .agents
            .retain(|pane| !run_ids.contains(&pane.ensemble_run_id));
        self.baseline_workers.retain(|id, _| !run_ids.contains(id));
        self.set_visible(self.visible.filter(|pane| self.contains_pane(*pane)));
        self.last_entered = self.last_entered.filter(|pane| self.contains_pane(*pane));
    }

    fn contains_pane(&self, pane: VisiblePane) -> bool {
        match pane {
            VisiblePane::SideChat(id) => self
                .side_threads
                .iter()
                .any(|pane| pane.descriptor.id == id),
            VisiblePane::Subtask(id) => self
                .workspace
                .children
                .iter()
                .any(|pane| pane.app.surface().id.pane == id),
            VisiblePane::Agent(id) => self
                .workspace
                .agents
                .iter()
                .any(|pane| pane.app.surface().id.pane == id),
        }
    }

    pub fn apply_streams(&mut self, batch: SessionStreamBatch) {
        for (id, stream) in batch.side_chats {
            if let Some(pane) = self
                .side_threads
                .iter_mut()
                .find(|pane| pane.descriptor.id == id)
            {
                pane.app.apply_stream(stream);
            }
        }
        let child_count = self.workspace.children.len();
        if let Some(stream) = batch.root {
            let effects = self.workspace.root.apply_stream(stream);
            self.consume_root_effects(effects);
        }
        for (id, stream) in batch.subtasks {
            let index = self.ensure_child(&id);
            let effects = self.workspace.children[index].app.apply_stream(stream);
            debug_assert!(
                effects.is_empty(),
                "inspect panes cannot prune root workers"
            );
        }
        if self.workspace.children.len() != child_count {
            self.refresh_subtasks();
        }
        for stream in batch.agent_runs {
            if let Some(index) = self.workspace.agents.iter().position(|pane| {
                pane.id == stream.agent_run_id && pane.ensemble_run_id == stream.ensemble_run_id
            }) {
                let pane = &mut self.workspace.agents[index];
                pane.apply_transcript_preview(std::sync::Arc::unwrap_or_clone(stream.event));
                pane.refresh_pane_metadata();
            }
        }
    }

    /// Resolve one owner before mutation. Capturing surfaces never delegate
    /// ignored keys; pane shortcuts are allowed only by that owner's policy.
    pub fn handle_event(&mut self, event: Event) -> Option<UiAction> {
        let action = self.handle_workspace_event(event);
        if self
            .workspace
            .root
            .side_reference_query()
            .map(|(query, _)| query)
            != self.dismissed_side_query.as_deref()
        {
            self.dismissed_side_query = None;
        }
        self.file_search_request();
        action
    }

    fn handle_workspace_event(&mut self, event: Event) -> Option<UiAction> {
        let input = match crate::input::normalize(event) {
            InputEvent::User(input) => input,
            InputEvent::Resize => {
                self.workspace.controls.invalidate_geometry();
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
        let owner = self.surface_snapshot(self.visible_app()).owner;
        if owner.captures {
            self.visible_app_mut().close_composer_edit_group();
            let action = if self.overlay_owns(owner.id.kind) {
                let busy = self.workspace.root.is_busy();
                let mode = self.workspace.root.mcp_mode();
                let action = self.workspace.controls.handle_input(
                    owner,
                    input,
                    self.workspace.root.jobs_inspector_mut(),
                    busy,
                    mode,
                );
                match action {
                    Some(UiAction::Session(SessionCommand::Turn(command)))
                        if matches!(
                            command,
                            zevria_session_api::TurnCommand::InvokeMcpPrompt { .. }
                        ) =>
                    {
                        self.workspace.root.begin_overlay_mcp_prompt(command)
                    }
                    other => other,
                }
            } else {
                self.visible_app_mut()
                    .handle_input_on(owner, input, Instant::now())
            };
            return self.consume_view_action(action);
        }
        if let UserInput::Key(key) = &input {
            let matches = self.side_completions();
            if !matches.is_empty() {
                match (key.code, key.modifiers) {
                    (KeyCode::Up, KeyModifiers::NONE) => {
                        self.side_completion = self.side_completion.saturating_sub(1);
                        return None;
                    }
                    (KeyCode::Down, KeyModifiers::NONE) => {
                        self.side_completion = (self.side_completion + 1).min(matches.len() - 1);
                        return None;
                    }
                    (KeyCode::Tab | KeyCode::Enter, KeyModifiers::NONE)
                    | (KeyCode::Char('i'), KeyModifiers::CONTROL) => {
                        let item = &matches[self.side_completion.min(matches.len() - 1)];
                        self.workspace.root.complete_side_reference(&item.insertion);
                        self.side_completion = 0;
                        return None;
                    }
                    (KeyCode::Esc, KeyModifiers::NONE) => {
                        self.dismissed_side_query = self
                            .workspace
                            .root
                            .side_reference_query()
                            .map(|(query, _)| query.to_owned());
                        return None;
                    }
                    _ => self.side_completion = 0,
                }
            }
            match owner.action(*key) {
                Some(Action::Help) if !self.visible_app().has_pending_text_find() => {
                    let eligibility = self.visible_app().hint_eligibility();
                    let context = self.visible_app().key_context(owner);
                    self.visible_app_mut().close_composer_edit_group();
                    self.workspace.controls.show_help(context, &eligibility);
                    return None;
                }
                Some(Action::SideChatOverlay) => {
                    self.show_side_chats();
                    return None;
                }
                Some(Action::ModelPicker) => {
                    self.open_model_picker(ModelSelectionScope::SessionOnly);
                    return None;
                }
                Some(Action::JobsOverlay) => {
                    self.show_jobs();
                    return None;
                }
                Some(Action::SubtasksOverlay) => {
                    self.show_subtasks();
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
        let action = self
            .visible_app_mut()
            .handle_input_on(owner, input, Instant::now());
        self.consume_view_action(action)
    }

    fn consume_view_action(&mut self, action: Option<UiAction>) -> Option<UiAction> {
        match action {
            Some(UiAction::OpenSideChat {
                prompt: None,
                excerpt: None,
            }) => {
                self.show_side_chats();
                None
            }
            Some(UiAction::NewSideChat) => Some(UiAction::Session(self.create_side(None, None))),
            Some(UiAction::OpenSidePane { id }) => {
                self.open_side(id);
                None
            }
            Some(UiAction::InsertSideReference { id }) => {
                let short = self
                    .side_threads
                    .iter()
                    .find(|pane| pane.descriptor.id == id)?
                    .descriptor
                    .short_id
                    .clone();
                self.insert_in_root(&format!(" @side:{short} "));
                None
            }
            Some(UiAction::InsertSideQuote) => {
                let Some(VisiblePane::SideChat(id)) = self.visible else {
                    return None;
                };
                let pane = self
                    .side_threads
                    .iter()
                    .find(|pane| pane.descriptor.id == id)?;
                let answer = pane.app.excerpt_for_side_chat().or_else(|| {
                    side_chat_answers(pane.app.side_model())
                        .pop()
                        .map(|(_, _, answer)| answer)
                })?;
                let quote = format_quote_for_main(&pane.descriptor.title, &answer);
                self.insert_in_root(&quote);
                None
            }
            Some(UiAction::Session(SessionCommand::Turn(
                zevria_session_api::TurnCommand::EditTranscript(mut edit),
            ))) => {
                if let zevria_session_api::TranscriptEditReplacement::Message {
                    text,
                    references,
                    ..
                } = &mut edit.replacement
                {
                    *references = extract_side_chat_references_from(
                        text,
                        self.side_threads
                            .iter()
                            .map(|pane| (&pane.descriptor, pane.app.side_model())),
                    );
                }
                Some(UiAction::Session(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::EditTranscript(edit),
                )))
            }
            Some(UiAction::OpenSideChat { prompt, excerpt }) => {
                Some(UiAction::Session(self.create_side(prompt, excerpt)))
            }
            Some(UiAction::Session(SessionCommand::Turn(
                zevria_session_api::TurnCommand::Submit {
                    text,
                    mode,
                    behavior,
                    ..
                },
            ))) => {
                let references = extract_side_chat_references_from(
                    &text,
                    self.side_threads
                        .iter()
                        .map(|pane| (&pane.descriptor, pane.app.side_model())),
                );
                Some(UiAction::Session(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        text,
                        mode,
                        behavior,
                        references,
                    },
                )))
            }
            Some(UiAction::OpenSubtask { id }) => {
                self.open_child(&id);
                None
            }
            Some(UiAction::OpenAgentRun { id }) => {
                self.open_agent(&id);
                None
            }
            Some(UiAction::JobControl { id, terminate }) => {
                let inspector = self.workspace.root.jobs_inspector_mut();
                let notice = if terminate {
                    inspector.terminate(id)
                } else {
                    inspector.move_to_background(id)
                };
                if let Some(notice) = notice {
                    self.show_root_error(notice);
                }
                None
            }
            action => action,
        }
    }

    pub fn restore_side_chat(
        &mut self,
        descriptor: SideChatDescriptor,
        quoted_excerpt: Option<String>,
        items: Vec<TranscriptItem>,
    ) {
        let side = SideChatPane::restore(RestoredSidePane {
            descriptor,
            quoted_excerpt,
            items,
        });
        self.add_side(side, false);
    }

    fn add_side(&mut self, side: SideChatPane, focus: bool) {
        let descriptor = side.descriptor.clone();
        let id = descriptor.id;
        let app = App::from_side_chat(side, focus)
            .with_shared_model_profiles(Arc::clone(&self.model_profiles));
        self.side_threads.push(NativeSidePane { descriptor, app });
        if focus {
            self.open_side(id);
        }
    }

    fn create_side(
        &mut self,
        prompt: Option<zevria_content::UserPrompt>,
        excerpt: Option<String>,
    ) -> SessionCommand {
        let command = new_side_chat(prompt, excerpt);
        if let SideChatCommand::Create { request_id, .. } = &command {
            self.pending_side_request = Some(request_id.clone());
        }
        SessionCommand::Control(ControlCommand::SideChat(command))
    }

    fn apply_side(&mut self, event: SideChatEvent) {
        let id = match event {
            SideChatEvent::Created {
                request_id,
                descriptor,
                quoted_excerpt,
                awaiting_initial_turn,
            } => {
                let focus = self.pending_side_request.as_deref() == Some(&request_id);
                if focus {
                    self.pending_side_request = None;
                }
                let mut side = SideChatPane::new(*descriptor, quoted_excerpt);
                side.awaiting_initial_turn = awaiting_initial_turn;
                if awaiting_initial_turn {
                    side.descriptor.status = SideChatStatus::Running;
                }
                let focus = focus
                    && self.surface_snapshot(self.visible_app()).owner.id.kind
                        != SurfaceKind::PlanReview
                    && !self.surface_snapshot(self.visible_app()).owner.captures;
                self.add_side(side, focus);
                return;
            }
            SideChatEvent::CreateFailed { request_id, error } => {
                if self.pending_side_request.as_deref() == Some(&request_id) {
                    self.pending_side_request = None;
                }
                self.show_root_error(format!("Side Chat could not open: {error}"));
                return;
            }
            SideChatEvent::Session { id, .. }
            | SideChatEvent::Status { id, .. }
            | SideChatEvent::Unloaded { id }
            | SideChatEvent::Failed { id, .. }
            | SideChatEvent::SubmitRejected { id, .. } => id,
        };
        if let Some(pane) = self
            .side_threads
            .iter_mut()
            .find(|pane| pane.descriptor.id == id)
        {
            match event {
                SideChatEvent::Session { event, .. } => {
                    if matches!(
                        *event,
                        SessionEvent::Turn(
                            TurnEvent::Started { .. }
                                | TurnEvent::Rejected { .. }
                                | TurnEvent::Failed { .. }
                                | TurnEvent::Cancelled { .. }
                        )
                    ) {
                        pane.app.side_chat_started_or_settled();
                    }
                    if matches!(*event, SessionEvent::Turn(TurnEvent::Started { .. })) {
                        pane.descriptor.turn_count += 1;
                        pane.descriptor.status = SideChatStatus::Running;
                    }
                    pane.app.reduce(*event);
                }
                SideChatEvent::Status { status, .. } => pane.descriptor.status = status,
                SideChatEvent::Unloaded { .. } => pane.descriptor.status = SideChatStatus::Idle,
                SideChatEvent::Failed { error, .. } => {
                    pane.descriptor.status = SideChatStatus::Failed;
                    pane.app.stop_side_with_error(error);
                }
                SideChatEvent::SubmitRejected { error, .. } => {
                    if pane.app.side_model().active_turn_id().is_none() {
                        pane.descriptor.status = SideChatStatus::Failed;
                        pane.app.stop_side_with_error(error);
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn side_completions(&self) -> Vec<zevria_presentation::side_chats::SideChatCompletionItem> {
        if self.visible.is_some() {
            return Vec::new();
        }
        let Some((query, _)) = self.workspace.root.side_reference_query() else {
            return Vec::new();
        };
        if self.dismissed_side_query.as_deref() == Some(query) {
            return Vec::new();
        }
        zevria_presentation::side_chats::side_chat_completions_from(
            self.side_threads.iter().rev().map(|pane| &pane.descriptor),
            query,
        )
    }

    fn open_side(&mut self, id: SideChatId) {
        let pane = VisiblePane::SideChat(id);
        self.last_entered = Some(pane);
        self.workspace.controls.close_side_chats();
        self.set_visible(Some(pane));
    }

    fn insert_in_root(&mut self, text: &str) {
        self.workspace.controls.close_side_chats();
        self.set_visible(None);
        self.workspace.root.insert_side_quote(text);
    }

    fn show_side_chats(&mut self) {
        let owner = self.surface_snapshot(self.visible_app()).owner;
        if owner.captures || owner.id.kind == SurfaceKind::PlanReview {
            return;
        }
        self.workspace.root.jobs_inspector_mut().close();
        self.visible_app_mut().close_composer_edit_group();
        self.workspace.controls.show_side_chats(self.side_rows());
    }

    fn side_rows(&self) -> Vec<SideChatDescriptor> {
        self.side_threads
            .iter()
            .rev()
            .map(|pane| pane.descriptor.clone())
            .collect()
    }

    fn open_child(&mut self, id: &SubtaskId) {
        if let Some(index) = self
            .workspace
            .children
            .iter()
            .position(|child| &child.id == id)
        {
            let pane = VisiblePane::Subtask(self.workspace.children[index].app.surface().id.pane);
            self.set_visible(Some(pane));
            self.last_entered = Some(pane);
        }
    }

    fn open_agent(&mut self, id: &AgentRunId) {
        if let Some(index) = self.workspace.agents.iter().position(|pane| &pane.id == id) {
            let pane = VisiblePane::Agent(self.workspace.agents[index].app.surface().id.pane);
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
                    .workspace
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
                    .workspace
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
                    (None, None) => self
                        .side_threads
                        .last()
                        .map(|pane| VisiblePane::SideChat(pane.descriptor.id)),
                }
            });
        if let Some(pane) = target {
            self.set_visible(Some(pane));
            self.last_entered = Some(pane);
        }
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &App {
        &self.workspace.root
    }

    #[cfg(test)]
    pub(crate) fn visible_child_id(&self) -> Option<&SubtaskId> {
        match self.visible {
            Some(VisiblePane::Subtask(id)) => self
                .workspace
                .children
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| &pane.id),
            Some(VisiblePane::Agent(_) | VisiblePane::SideChat(_)) | None => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn child(&self, id: &SubtaskId) -> Option<&App> {
        self.workspace
            .children
            .iter()
            .find(|child| &child.id == id)
            .map(|child| &child.app)
    }

    #[cfg(test)]
    pub(crate) fn visible_agent_id(&self) -> Option<&AgentRunId> {
        match self.visible {
            Some(VisiblePane::Agent(id)) => self
                .workspace
                .agents
                .iter()
                .find(|pane| pane.app.surface().id.pane == id)
                .map(|pane| &pane.id),
            Some(VisiblePane::Subtask(_) | VisiblePane::SideChat(_)) | None => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn agent(&self, id: &AgentRunId) -> Option<&App> {
        self.workspace
            .agents
            .iter()
            .find(|agent| &agent.id == id)
            .map(|agent| &agent.app)
    }
}

#[cfg(test)]
mod side_chat_tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;
    use zevria_foundation::{ModelProfileRef, ReasoningLevel, SessionMode, TurnId};

    fn descriptor(id: SideChatId, mode: SessionMode) -> SideChatDescriptor {
        SideChatDescriptor {
            id,
            short_id: id.short(),
            title: "fixture".into(),
            forked_mode: mode,
            forked_profile: ModelProfileRef::new("fixture", "profile"),
            forked_reasoning: ReasoningLevel::Medium,
            inherited_turn_count: 0,
            turn_count: 0,
            status: SideChatStatus::Idle,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn side_reference_completion_yields_to_plan_review_and_its_escape() {
        use zevria_session_api::PlanEvent;
        use zevria_workflow::{PlanArtifact, PlanId, PlanVersion, PlanWorkflowState};
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        views.add_side(
            SideChatPane::new(descriptor(SideChatId::random(), SessionMode::Build), None),
            false,
        );
        views.handle_event(key(KeyCode::Char('i')));
        views.handle_event(Event::Paste("Read @side:".into()));
        assert_eq!(views.side_completions().len(), 1);
        views.apply(SessionEvent::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version: PlanVersion {
                        id: PlanId::new(),
                        revision: 1,
                    },
                    title: "Plan review".into(),
                    markdown: "Check the parser".into(),
                    source_turn_id: TurnId::new(1),
                },
            }),
        }));
        assert_eq!(views.root().surface().id.kind, SurfaceKind::PlanReview);
        assert!(views.side_completions().is_empty());
        views.handle_event(key(KeyCode::Esc));
        assert_ne!(views.root().surface().id.kind, SurfaceKind::PlanReview);
        assert_eq!(views.root().input(), "Read @side:");
    }

    #[test]
    fn plan_side_selection_quotes_and_a_long_excerpt_cannot_hide_an_error() {
        use ratatui::{Terminal, backend::TestBackend};
        use rig_core::message::Message;
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        let id = SideChatId::random();
        views.restore_side_chat(
            descriptor(id, SessionMode::Plan),
            Some("A selected line from the main conversation. ".repeat(20)),
            vec![
                TranscriptItem::Message(Message::user("Question")),
                TranscriptItem::Message(Message::assistant("Side reply")),
            ],
        );
        views.open_side(id);
        assert_eq!(
            views.visible_app().side_model().next_mode(),
            SessionMode::Plan
        );
        views.apply(SessionEvent::SideChat(SideChatEvent::Failed {
            id,
            error: "Side reply failed".into(),
        }));
        let mut terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
        terminal.draw(|frame| views.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            (0..buffer.area.height).any(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.contains("Side reply failed")
            }),
            "the side error must be rendered before the excerpt"
        );
        views.handle_event(key(KeyCode::Char('v')));
        assert_eq!(
            views.visible_app().surface().id.kind,
            SurfaceKind::Selection
        );
        let mut wide = Terminal::new(TestBackend::new(100, 24)).unwrap();
        wide.draw(|frame| views.render(frame)).unwrap();
        let wide_buf = wide.backend().buffer();
        assert!(
            (0..wide_buf.area.height - 1).any(|y| {
                let row: String = (0..wide_buf.area.width)
                    .map(|x| wide_buf[(x, y)].symbol())
                    .collect();
                row.contains("m quote in main")
            }),
            "Side Chat Selection hints row should expose SideBlockNav bindings"
        );
        views.handle_event(key(KeyCode::Char('m')));
        assert!(views.visible.is_none());
        assert!(views.root().input().contains("Side reply"));
    }

    #[test]
    fn side_chat_list_uses_capturing_surface_and_normalized_keys() {
        use ratatui::crossterm::event::KeyEventKind;
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        views.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        )));
        let owner = views.surface_snapshot(views.visible_app()).owner;
        assert_eq!(owner.id.kind, SurfaceKind::SideChats);
        assert!(owner.captures);
        assert_eq!(views.handle_event(Event::Resize(50, 20)), None);
        assert_eq!(
            views.handle_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            ))),
            None
        );
        assert!(views.overlay_owns(SurfaceKind::SideChats));
        assert!(matches!(
            views.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE
            ))),
            Some(UiAction::Session(SessionCommand::Control(
                ControlCommand::SideChat(SideChatCommand::Create {
                    initial_prompt: None,
                    ..
                })
            )))
        ));
        assert!(!views.overlay_owns(SurfaceKind::SideChats));
    }

    #[test]
    fn side_chat_slash_routes_to_control_and_cancel_is_pane_local() {
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        views.apply(SessionEvent::Turn(TurnEvent::Started {
            turn_id: zevria_foundation::TurnId::new(8),
            mode: zevria_foundation::SessionMode::Build,
            message: rig_core::message::Message::user("Main turn stays active"),
            references: Vec::new(),
        }));
        views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        views.handle_event(Event::Paste("/btw why is this isolated?".into()));
        let Some(UiAction::Session(SessionCommand::Control(ControlCommand::SideChat(
            SideChatCommand::Create {
                request_id,
                initial_prompt,
                ..
            },
        )))) = views.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
        else {
            panic!("expected a non-turn Side Chat control")
        };
        assert_eq!(
            initial_prompt.unwrap().text_projection(),
            "why is this isolated?"
        );
        let id = SideChatId::random();
        views.apply(SessionEvent::SideChat(SideChatEvent::Created {
            request_id,
            descriptor: Box::new(descriptor(id, SessionMode::Build)),
            quoted_excerpt: None,
            awaiting_initial_turn: true,
        }));
        assert!(views.visible == Some(VisiblePane::SideChat(id)));
        assert_eq!(
            views.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(UiAction::Session(SessionCommand::Control(
                ControlCommand::SideChat(SideChatCommand::Cancel { id })
            )))
        );
        assert!(views.workspace.root.is_busy());
        views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(views.visible.is_none());
    }
}
