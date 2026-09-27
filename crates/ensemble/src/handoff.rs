//! Claude plan-handoff validation and artifact lifecycle.

use super::*;
use zevria_workflow::{NativePlanCapture, NativePlanSource};

#[path = "artifact_snapshot.rs"]
mod artifact_snapshot;
use artifact_snapshot::*;

#[cfg(windows)]
#[path = "windows_artifact_path.rs"]
mod windows_artifact_path;

#[derive(Clone)]
pub(super) struct ClaudePlanHandoff {
    pub(super) state: Arc<Mutex<ClaudePlanHandoffState>>,
    pub(super) completion: Arc<Mutex<tokio_util::sync::CancellationToken>>,
    pub(super) changed: watch::Sender<()>,
}

pub(super) struct ClaudePlanHandoffState {
    pub(super) artifact_directory: PathBuf,
    pub(super) workspace_artifact_directory: PathBuf,
    pub(super) workspace: PathBuf,
    pub(super) tools: HashMap<String, ClaudePlanTool>,
    pub(super) retired_tools: HashSet<String>,
    // Tool evidence survives proposal boundaries; only current-generation exits
    // participate in proposal eligibility. Retired identities are tombstones.
    pub(super) generation: u64,
    pub(super) tool_generations: HashMap<String, u64>,
    pub(super) artifact_evidence: HashMap<String, ArtifactEvidence>,
    pub(super) exit_hints: HashMap<String, PathBuf>,
    pub(super) exit_terminals: HashMap<String, ToolCallStatus>,
    pub(super) next_announcement: u64,
    pub(super) next_attempt: u64,
    pub(super) current_attempt: Option<u64>,
    pub(super) next_ticket: u64,
    pub(super) permissions: VecDeque<ClaudePlanPermissionKey>,
    pub(super) granted_tool: Option<String>,
    // A terminal update can arrive while the permission event is being synced.
    // Do not admit another request until that sync has also succeeded.
    pub(super) delivering_permission: Option<ClaudePlanPermissionKey>,
    pub(super) phase: ClaudePlanHandoffPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaudePlanArtifactOperation {
    Write,
    Edit,
    MultiEdit,
}

impl ClaudePlanArtifactOperation {
    pub(super) fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "Write" => Some(Self::Write),
            "Edit" => Some(Self::Edit),
            "MultiEdit" => Some(Self::MultiEdit),
            _ => None,
        }
    }

    pub(super) fn tool_name(self) -> &'static str {
        match self {
            Self::Write => "Write",
            Self::Edit => "Edit",
            Self::MultiEdit => "MultiEdit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaudePlanArtifactLifecycle {
    Active,
    Completed,
    Failed,
}

impl ClaudePlanArtifactLifecycle {
    pub(super) fn terminal_from_status(status: &ToolCallStatus) -> Option<Self> {
        match status {
            ToolCallStatus::Completed => Some(Self::Completed),
            ToolCallStatus::Failed => Some(Self::Failed),
            _ => None,
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaudePlanTool {
    ArtifactMutation {
        operation: ClaudePlanArtifactOperation,
        path: Option<PathBuf>,
        lifecycle: ClaudePlanArtifactLifecycle,
        announcement: u64,
    },
    PendingExitPlanMode,
    ExitPlanMode {
        plan: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaudePlanHandoffPhase {
    Active,
    Settling {
        generation: u64,
        tool_call_id: String,
        source: ClaudePlanCandidate,
        rejection_delivered: bool,
    },
    Capturing {
        tool_call_id: String,
    },
    Completed,
    Aborted {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudePlanPermissionKey {
    pub(super) sequence: u64,
    pub(super) attempt: u64,
    pub(super) session_id: String,
    pub(super) tool_call_id: String,
}

// Transport-bound guards never erase logical tool evidence or release a grant.
pub(super) struct ClaudePlanAttempt {
    pub(super) handoff: ClaudePlanHandoff,
    pub(super) id: u64,
    pub(super) lifetime: tokio_util::sync::CancellationToken,
}

impl Drop for ClaudePlanAttempt {
    fn drop(&mut self) {
        self.lifetime.cancel();
        let mut state = self
            .handoff
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        state.permissions.retain(|ticket| ticket.attempt != self.id);
        if state.current_attempt == Some(self.id) {
            state.current_attempt = None;
        }
        self.handoff.changed.send_replace(());
    }
}

pub(super) struct ClaudePlanPermissionTicket {
    pub(super) handoff: ClaudePlanHandoff,
    pub(super) key: ClaudePlanPermissionKey,
}

impl Drop for ClaudePlanPermissionTicket {
    fn drop(&mut self) {
        let mut state = self
            .handoff
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        state.permissions.retain(|ticket| ticket != &self.key);
        if state.delivering_permission.as_ref() == Some(&self.key) {
            // Abandoned/send-failed/sync-failed delivery is ambiguous. Even if a
            // terminal raced with it, no waiter in this attempt may approve work.
            state.current_attempt = None;
        }
        self.handoff.changed.send_replace(());
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaudePlanAdmission {
    Waiting,
    Cancelled,
    Granted,
}

pub(super) enum ClaudeHandoffPermission {
    Ordinary,
    ArtifactMutation,
    ExitPlanMode { source: ClaudePlanCandidate },
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaudePlanCandidate {
    Explicit(String),
    Artifact { path_hint: Option<PathBuf> },
}

#[derive(Debug, Clone, Default)]
pub(super) struct ArtifactEvidence {
    directory_identity: Option<String>,
    // Pin the first terminal observation, never refresh it on terminal replay.
    file_version: Option<zevria_foundation::contained_read::FileSnapshot>,
    // Only Write.rawInput.content (also in permission requests) establishes
    // whole-file scope. Edit/MultiEdit hunks and Write display diffs can be
    // partial or context-expanded, even with oldText: null. They remain path
    // and display evidence, never authoritative document contents.
    whole_file_content: Option<String>,
}

impl ArtifactEvidence {
    fn observe_write_content(
        &mut self,
        operation: ClaudePlanArtifactOperation,
        raw_input: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        if operation == ClaudePlanArtifactOperation::Write
            && let Some(content) = raw_input
                .and_then(|input| input.get("content"))
                .and_then(serde_json::Value::as_str)
        {
            if self
                .whole_file_content
                .as_deref()
                .is_some_and(|previous| previous != content)
            {
                return Err("Claude Write changed its complete-content evidence".into());
            }
            self.whole_file_content = Some(content.to_string());
        }
        Ok(())
    }
}

impl ClaudePlanHandoff {
    pub(super) async fn wait_for_capture(&self) {
        let signal = self
            .completion
            .lock()
            .expect("handoff signal poisoned")
            .clone();
        signal.cancelled().await;
    }

    #[cfg(test)]
    pub(super) fn begin_generation(&self) -> Result<(), AcpError> {
        let generation = self.state.lock().expect("handoff poisoned").generation + 1;
        self.begin_generation_at(generation)
    }

    pub(super) fn begin_generation_at(&self, generation: u64) -> Result<(), AcpError> {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        if state.granted_tool.is_some()
            || state.delivering_permission.is_some()
            || !state.permissions.is_empty()
        {
            return Err(acp_error(
                "cannot begin a new proposal while an artifact permission remains unresolved",
            ));
        }
        if matches!(
            state.phase,
            ClaudePlanHandoffPhase::Settling { .. } | ClaudePlanHandoffPhase::Capturing { .. }
        ) {
            return Err(acp_error("the previous native proposal has not settled"));
        }
        let retired = state.tools.keys().cloned().collect::<Vec<_>>();
        state.retired_tools.extend(retired);
        state.generation = generation;
        state.phase = ClaudePlanHandoffPhase::Active;
        *self.completion.lock().expect("handoff signal poisoned") =
            tokio_util::sync::CancellationToken::new();
        Ok(())
    }

    pub(super) fn new(agent: &EnsembleAgentConfig, workspace: &Path) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let normalized_workspace = zevria_foundation::windows_io::normalize_disk_path(workspace)?;
        #[cfg(windows)]
        let workspace = normalized_workspace.as_path();
        let artifact_directory = claude_plan_artifact_directory(agent, workspace)?;
        Ok(Self {
            state: Arc::new(Mutex::new(ClaudePlanHandoffState {
                artifact_directory,
                workspace_artifact_directory: workspace_claude_plan_artifact_directory(workspace),
                workspace: workspace.to_path_buf(),
                tools: HashMap::new(),
                retired_tools: HashSet::new(),
                generation: 1,
                tool_generations: HashMap::new(),
                artifact_evidence: HashMap::new(),
                exit_hints: HashMap::new(),
                exit_terminals: HashMap::new(),
                next_announcement: 0,
                next_attempt: 0,
                current_attempt: None,
                next_ticket: 0,
                permissions: VecDeque::new(),
                granted_tool: None,
                delivering_permission: None,
                phase: ClaudePlanHandoffPhase::Active,
            })),
            completion: Arc::new(Mutex::new(tokio_util::sync::CancellationToken::new())),
            changed: watch::channel(()).0,
        })
    }

    pub(super) fn start_attempt(
        &self,
        lifetime: tokio_util::sync::CancellationToken,
    ) -> ClaudePlanAttempt {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        state.next_attempt += 1;
        let id = state.next_attempt;
        state.current_attempt = Some(id);
        state.permissions.clear();
        state.delivering_permission = None;
        self.changed.send_replace(());
        ClaudePlanAttempt {
            handoff: self.clone(),
            id,
            lifetime,
        }
    }

    // Called in the receive callback, before spawning any waiter: FIFO means
    // valid request receive order, not announcement order or task poll order.
    pub(super) fn register_permission(
        &self,
        request: &RequestPermissionRequest,
        attempt: u64,
    ) -> Result<ClaudePlanPermissionTicket, String> {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        if state
            .retired_tools
            .contains(&request.tool_call.tool_call_id.to_string())
        {
            return Err(
                "stale artifact permission belongs to an earlier proposal generation".into(),
            );
        }
        if state.current_attempt != Some(attempt) {
            return Err(
                "Claude plan permission belongs to an inactive connection attempt".to_string(),
            );
        }
        if !matches!(state.phase, ClaudePlanHandoffPhase::Active) {
            return Err(
                "Claude attempted another plan artifact mutation after handoff began".to_string(),
            );
        }
        match state.permission(request) {
            ClaudeHandoffPermission::ArtifactMutation => {}
            ClaudeHandoffPermission::Invalid(error) => return Err(error),
            _ => return Err("Claude artifact queue received a non-artifact permission".to_string()),
        }
        if artifact_mutation_permission_option(&request.options).is_none() {
            return Err(
                "Claude plan artifact mutation offered no allow_once permission option".to_string(),
            );
        }
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        if state.granted_tool.as_ref() == Some(&tool_call_id)
            || state
                .permissions
                .iter()
                .any(|ticket| ticket.tool_call_id == tool_call_id)
            || state
                .delivering_permission
                .as_ref()
                .is_some_and(|ticket| ticket.tool_call_id == tool_call_id)
        {
            return Err(format!(
                "Claude plan artifact {tool_call_id:?} requested duplicate permission or already has an unresolved grant"
            ));
        }
        let key = ClaudePlanPermissionKey {
            sequence: state.next_ticket,
            attempt,
            session_id: request.session_id.to_string(),
            tool_call_id,
        };
        state.next_ticket += 1;
        state.permissions.push_back(key.clone());
        self.changed.send_replace(());
        Ok(ClaudePlanPermissionTicket {
            handoff: self.clone(),
            key,
        })
    }

    pub(super) fn inspect_update(&self, update: &AcpSessionUpdate) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        let result = match update {
            AcpSessionUpdate::ToolCall(call) => state.inspect_tool_call(call),
            AcpSessionUpdate::ToolCallUpdate(update) => state.inspect_tool_update(update),
            _ => Ok(()),
        };
        if result.is_err() {
            // Fence admission under the same lock before waking tasks. The
            // notification handler will record the violation and cancel the
            // worker, but a waiter must not race ahead of that handler.
            state.current_attempt = None;
        }
        self.changed.send_replace(());
        result
    }

    pub(super) fn permission(&self, request: &RequestPermissionRequest) -> ClaudeHandoffPermission {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        let permission = state.permission(request);
        if matches!(permission, ClaudeHandoffPermission::Invalid(_)) {
            state.current_attempt = None;
            self.changed.send_replace(());
        }
        permission
    }

    #[cfg(test)]
    pub(super) fn begin_capture(&self, tool_call_id: &str, plan: &str) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        state.begin_capture(tool_call_id, plan)
    }

    pub(super) fn begin_settlement(
        &self,
        tool_call_id: &str,
        source: ClaudePlanCandidate,
    ) -> Result<(), String> {
        let mut state = self.state.lock().expect("handoff poisoned");
        if state.retired_tools.contains(tool_call_id) {
            return Err("stale Claude handoff belongs to an earlier proposal generation".into());
        }
        if !matches!(state.phase, ClaudePlanHandoffPhase::Active) {
            return Err("Claude attempted more than one ExitPlanMode handoff".into());
        }
        match (&source, state.tools.get(tool_call_id)) {
            (
                ClaudePlanCandidate::Explicit(plan),
                Some(ClaudePlanTool::ExitPlanMode { plan: expected }),
            ) if plan == expected => {}
            (ClaudePlanCandidate::Artifact { .. }, Some(ClaudePlanTool::PendingExitPlanMode)) => {}
            _ => return Err("Claude ExitPlanMode handoff no longer matched its tool call".into()),
        }
        state.phase = ClaudePlanHandoffPhase::Settling {
            generation: state.generation,
            tool_call_id: tool_call_id.into(),
            source,
            rejection_delivered: false,
        };
        self.changed.send_replace(());
        Ok(())
    }

    pub(super) fn rejection_delivered(&self) -> Result<(), String> {
        let mut state = self.state.lock().expect("handoff poisoned");
        let ClaudePlanHandoffPhase::Settling {
            rejection_delivered,
            ..
        } = &mut state.phase
        else {
            return Err("native handoff rejection delivered after proposal ended".into());
        };
        *rejection_delivered = true;
        self.changed.send_replace(());
        Ok(())
    }

    pub(super) fn abort_capture(&self, error: String) {
        self.state.lock().expect("handoff poisoned").phase =
            ClaudePlanHandoffPhase::Aborted { error };
        self.changed.send_replace(());
        self.completion
            .lock()
            .expect("handoff signal poisoned")
            .cancel();
    }

    pub(super) fn is_settling(&self) -> bool {
        matches!(
            self.state.lock().expect("handoff poisoned").phase,
            ClaudePlanHandoffPhase::Settling { .. } | ClaudePlanHandoffPhase::Capturing { .. }
        )
    }

    pub(super) fn capture_error(&self) -> Option<String> {
        match &self.state.lock().expect("handoff poisoned").phase {
            ClaudePlanHandoffPhase::Aborted { error } => Some(error.clone()),
            _ => None,
        }
    }

    // No state lock is held during waiting, filesystem IO, or journal sync.
    pub(super) async fn resolve_capture(
        &self,
    ) -> Result<(AgentStructuredPlan, NativePlanCapture), String> {
        let mut changed = self.changed.subscribe();
        let (generation, tool_call_id, source, snapshot) = loop {
            {
                let state = self.state.lock().expect("handoff poisoned");
                let ClaudePlanHandoffPhase::Settling {
                    generation,
                    tool_call_id,
                    source,
                    rejection_delivered,
                } = &state.phase
                else {
                    return Err("native handoff is no longer settling".into());
                };
                if *rejection_delivered
                    && state.unresolved_artifacts().is_empty()
                    && state.delivering_permission.is_none()
                    && state.granted_tool.is_none()
                {
                    let snapshot = match source {
                        ClaudePlanCandidate::Explicit(_) => None,
                        ClaudePlanCandidate::Artifact { path_hint } => {
                            Some(state.artifact_snapshot(path_hint.as_deref())?)
                        }
                    };
                    break (*generation, tool_call_id.clone(), source.clone(), snapshot);
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| "native handoff notification stream closed".to_string())?;
        };
        let (markdown, provenance) = match (source, snapshot) {
            (ClaudePlanCandidate::Explicit(plan), _) => (plan, NativePlanSource::Explicit),
            (_, Some(snapshot)) => tokio::task::spawn_blocking(move || snapshot.read())
                .await
                .map_err(|error| format!("native artifact snapshot task failed: {error}"))??,
            _ => unreachable!("file candidate requires a snapshot"),
        };
        let mut state = self.state.lock().expect("handoff poisoned");
        if state.generation != generation
            || !matches!(&state.phase, ClaudePlanHandoffPhase::Settling { tool_call_id: current, .. } if current == &tool_call_id)
            || !state.unresolved_artifacts().is_empty()
            || state.current_attempt.is_none()
        {
            return Err(
                "native proposal changed or disconnected while resolving its snapshot".into(),
            );
        }
        // Raw plan data that arrived while the file read was in flight must agree.
        if let Some(ClaudePlanTool::ExitPlanMode { plan }) = state.tools.get(&tool_call_id)
            && normalize_handoff_plan(&markdown)? != *plan
        {
            return Err(
                "Claude ExitPlanMode changed its plan payload during snapshot resolution".into(),
            );
        }
        if let NativePlanSource::Artifact {
            artifact_tool_id, ..
        } = &provenance
            && state
                .artifact_evidence
                .get(artifact_tool_id)
                .and_then(|evidence| evidence.whole_file_content.as_ref())
                .is_some_and(|expected| expected != &markdown)
        {
            return Err(
                "artifact complete-content evidence changed during snapshot resolution".into(),
            );
        }
        // Remember the resolved identity separately from provider terminal status.
        // Late repeated rawInput.plan must agree even for a file-backed capture.
        state.tools.insert(
            tool_call_id.clone(),
            ClaudePlanTool::ExitPlanMode {
                plan: normalize_handoff_plan(&markdown)?,
            },
        );
        state.phase = ClaudePlanHandoffPhase::Capturing {
            tool_call_id: tool_call_id.clone(),
        };
        let plan = AgentStructuredPlan {
            plan_id: Some(CLAUDE_PLAN_HANDOFF_PLAN_ID.into()),
            markdown: Some(markdown),
            entries: Vec::new(),
        };
        let capture = NativePlanCapture {
            generation,
            exit_tool_id: tool_call_id,
            source: provenance,
        };
        capture.validate(&plan).map_err(|error| error.to_string())?;
        Ok((plan, capture))
    }

    pub(super) async fn persist_capture(
        &self,
        log: &RunLog,
        permission: AgentRunEvent,
    ) -> Result<(), String> {
        log.emit(permission)
            .await
            .map_err(|error| format!("native handoff permission persistence failed: {error}"))?;
        self.rejection_delivered()?;
        let (plan, capture) = self.resolve_capture().await?;
        let id = capture.exit_tool_id.clone();
        log.emit(AgentRunEvent::NativePlanCaptured { plan, capture })
            .await
            .map_err(|error| format!("native capture persistence failed: {error}"))?;
        self.finish_capture(&id)
    }

    pub(super) fn finish_capture(&self, tool_call_id: &str) -> Result<(), String> {
        {
            let mut state = self
                .state
                .lock()
                .expect("Claude plan handoff lock poisoned");
            match &state.phase {
                ClaudePlanHandoffPhase::Capturing {
                    tool_call_id: capturing,
                } if capturing == tool_call_id => {
                    state.phase = ClaudePlanHandoffPhase::Completed;
                }
                phase => {
                    return Err(format!(
                        "Claude plan handoff completed from unexpected state {phase:?}"
                    ));
                }
            }
        }
        self.completion
            .lock()
            .expect("handoff signal poisoned")
            .cancel();
        Ok(())
    }

    pub(super) fn unresolved_violation(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        if let ClaudePlanHandoffPhase::Aborted { error } = &state.phase {
            return Some(error.clone());
        }
        let unresolved = state.unresolved_artifacts();
        if !unresolved.is_empty() {
            let abandoned = state.abandoned_preparations();
            let diagnostic = if abandoned.is_empty() {
                String::new()
            } else {
                format!("; {abandoned}")
            };
            return Some(format!(
                "Claude plan artifacts remain unresolved: {unresolved}{diagnostic}"
            ));
        }
        if matches!(state.phase, ClaudePlanHandoffPhase::Completed) {
            return None;
        }
        if let Some((tool_call_id, _)) = state.tools.iter().find(|(id, tool)| {
            state.tool_generations.get(*id) == Some(&state.generation)
                && !state.retired_tools.contains(*id)
                && matches!(tool, ClaudePlanTool::PendingExitPlanMode)
        }) {
            return Some(format!(
                "Claude ExitPlanMode handoff {tool_call_id:?} never resolved to a nonempty plan"
            ));
        }
        matches!(
            state.phase,
            ClaudePlanHandoffPhase::Settling { .. } | ClaudePlanHandoffPhase::Capturing { .. }
        )
        .then(|| {
            "Claude ExitPlanMode handoff did not finish durably before the worker stopped"
                .to_string()
        })
    }

    pub(super) fn abandoned_preparations(&self) -> Option<String> {
        let diagnostic = self
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned")
            .abandoned_preparations();
        (!diagnostic.is_empty()).then_some(diagnostic)
    }

    pub(super) fn is_completed(&self) -> bool {
        matches!(
            self.state
                .lock()
                .expect("Claude plan handoff lock poisoned")
                .phase,
            ClaudePlanHandoffPhase::Completed
        )
    }
}

// Transport/task destruction aborts the proposal, never its artifact ledger.
// This guard also covers permission send/spawn failures and connection teardown.
pub(super) struct NativeCaptureGuard(pub(super) ClaudePlanHandoff);
impl Drop for NativeCaptureGuard {
    fn drop(&mut self) {
        if self.0.is_settling() {
            self.0.abort_capture(
                "native handoff disconnected or was cancelled before durable capture".into(),
            );
        }
    }
}
impl NativeCaptureGuard {
    pub(super) async fn settle(
        self,
        log: RunLog,
        permission: AgentRunEvent,
        cancellation: tokio_util::sync::CancellationToken,
        lifetime: tokio_util::sync::CancellationToken,
        peer: agent_client_protocol::RequestCancellation,
        grace: Duration,
    ) -> Result<(), AcpError> {
        let work = self.0.persist_capture(&log, permission);
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err("native handoff cancelled during settlement".into()),
            () = lifetime.cancelled() => Err("native handoff disconnected during settlement".into()),
            () = peer.cancelled() => Err("native handoff permission cancelled during settlement".into()),
            result = tokio::time::timeout(grace, work) => result.unwrap_or_else(|_| Err(format!("native handoff settlement timed out awaiting artifact terminal evidence or durable capture: {}", self.0.unresolved_violation().unwrap_or_default()))),
        };
        if let Err(error) = result {
            self.0.abort_capture(error.clone());
            // A failed journal must not be used to publish success. The log's
            // own failure latch remains authoritative if this diagnostic fails.
            log.emit(AgentRunEvent::Failure { error })
                .await
                .map_err(acp_error)?;
        }
        Ok(())
    }
}

impl ClaudePlanPermissionTicket {
    pub(super) fn try_admit(
        &self,
        request: &RequestPermissionRequest,
        cancelled: impl Fn() -> bool,
    ) -> Result<ClaudePlanAdmission, String> {
        let mut state = self
            .handoff
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        if cancelled()
            || state.current_attempt != Some(self.key.attempt)
            || !state.permissions.contains(&self.key)
        {
            return Ok(ClaudePlanAdmission::Cancelled);
        }
        if self.key.session_id != request.session_id.to_string()
            || self.key.tool_call_id != request.tool_call.tool_call_id.to_string()
        {
            return Err(
                "Claude plan permission ticket changed session or tool identity".to_string(),
            );
        }
        if state.permissions.front() != Some(&self.key)
            || state.granted_tool.is_some()
            || state.delivering_permission.is_some()
        {
            return Ok(ClaudePlanAdmission::Waiting);
        }
        // Recheck all request evidence and the filesystem after time in queue.
        match state.permission(request) {
            ClaudeHandoffPermission::ArtifactMutation => {}
            ClaudeHandoffPermission::Invalid(error) => return Err(error),
            _ => return Err("Claude plan permission no longer identifies an artifact".to_string()),
        }
        if !matches!(state.phase, ClaudePlanHandoffPhase::Active) {
            return Err(
                "Claude attempted another plan artifact mutation after handoff began".to_string(),
            );
        }
        state.prepare_artifact_mutation(request)?;
        if cancelled() {
            return Ok(ClaudePlanAdmission::Cancelled);
        }
        state.permissions.pop_front();
        state.granted_tool = Some(self.key.tool_call_id.clone());
        state.delivering_permission = Some(self.key.clone());
        Ok(ClaudePlanAdmission::Granted)
    }

    pub(super) fn delivered(&self) {
        let mut state = self
            .handoff
            .state
            .lock()
            .expect("Claude plan handoff lock poisoned");
        if state.delivering_permission.as_ref() == Some(&self.key) {
            state.delivering_permission = None;
        }
        self.handoff.changed.send_replace(());
    }
}

impl ClaudePlanHandoffState {
    fn artifact_snapshot(&self, path_hint: Option<&Path>) -> Result<ArtifactSnapshot, String> {
        let hint = path_hint
            .map(|path| {
                validate_claude_plan_artifact_path(
                    path,
                    &self.artifact_directory,
                    &self.workspace_artifact_directory,
                    &self.workspace,
                )
            })
            .transpose()?;
        // Select the latest observed mutation on each target, not the newest file
        // or newest successful write. A later failed mutation invalidates fallback.
        let mut latest = HashMap::new();
        for (id, tool) in &self.tools {
            if self.tool_generations.get(id) != Some(&self.generation)
                || self.retired_tools.contains(id)
            {
                continue;
            }
            if let ClaudePlanTool::ArtifactMutation {
                path: Some(path),
                lifecycle,
                announcement,
                ..
            } = tool
            {
                let entry = latest.entry(path).or_insert((id, lifecycle, announcement));
                if announcement > entry.2 {
                    *entry = (id, lifecycle, announcement);
                }
            }
        }
        let candidates = latest
            .iter()
            .filter(|(path, (_, status, _))| {
                **status == ClaudePlanArtifactLifecycle::Completed
                    && hint.as_ref().is_none_or(|hint| hint == **path)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err("Claude ExitPlanMode has no native plan payload and no eligible completed artifact in this generation".into());
        }
        if candidates.len() != 1 {
            return Err("Claude ExitPlanMode artifact source is ambiguous; multiple eligible paths and no disambiguating planFilePath".into());
        }
        let (path, (id, _, _)) = candidates[0];
        Ok(ArtifactSnapshot {
            path: (*path).clone(),
            artifact_tool_id: (*id).clone(),
            evidence: self.artifact_evidence.get(*id).cloned().unwrap_or_default(),
            artifact_directory: self.artifact_directory.clone(),
            workspace_artifact_directory: self.workspace_artifact_directory.clone(),
            workspace: self.workspace.clone(),
        })
    }

    pub(super) fn nonterminal_artifacts(
        &self,
    ) -> Vec<(u64, &str, ClaudePlanArtifactOperation, Option<&Path>)> {
        let mut tools = self
            .tools
            .iter()
            .filter_map(|(id, tool)| match tool {
                ClaudePlanTool::ArtifactMutation {
                    operation,
                    path,
                    lifecycle: ClaudePlanArtifactLifecycle::Active,
                    announcement,
                } => Some((*announcement, id.as_str(), *operation, path.as_deref())),
                _ => None,
            })
            .collect::<Vec<_>>();
        tools.sort_by_key(|(announcement, ..)| *announcement);
        tools
    }

    pub(super) fn unresolved_artifacts(&self) -> String {
        self.nonterminal_artifacts()
            .iter()
            .filter_map(|(_, id, operation, path)| {
                let path = (*path)?;
                let admission = if self.granted_tool.as_deref() == Some(*id) {
                    "granted; terminal result unknown"
                } else if self
                    .permissions
                    .iter()
                    .any(|ticket| ticket.tool_call_id == *id)
                {
                    "queued; not granted"
                } else {
                    "no host permission observed; native execution may still be active"
                };
                Some(format!(
                    "{} {id:?} targeting {} never reached a terminal status ({admission})",
                    operation.tool_name(),
                    path.display()
                ))
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub(super) fn abandoned_preparations(&self) -> String {
        self.nonterminal_artifacts()
            .iter()
            .filter(|(_, _, _, path)| path.is_none())
            .map(|(_, id, operation, _)| {
                format!(
                    "{} {id:?}: tool input never completed; abandoned preparation",
                    operation.tool_name()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub(super) fn inspect_tool_call(&mut self, call: &ToolCall) -> Result<(), String> {
        let tool_call_id = call.tool_call_id.to_string();
        if self.retired_tools.contains(&tool_call_id)
            && !matches!(
                call.status,
                ToolCallStatus::Completed | ToolCallStatus::Failed
            )
        {
            return Err(
                "stale Claude tool announcement belongs to an earlier proposal generation".into(),
            );
        }
        let tool_name = claude_tool_name(call.meta.as_ref());
        if let Some(operation) = tool_name.and_then(ClaudePlanArtifactOperation::from_tool_name) {
            return self.inspect_artifact_mutation(
                &tool_call_id,
                operation,
                Some(call.kind),
                Some(call.status),
                call.raw_input.as_ref(),
                &call.locations,
                &call.content,
                true,
            );
        }
        match tool_name {
            Some("ExitPlanMode") => self.inspect_exit_plan_mode(
                &tool_call_id,
                Some(call.kind),
                Some(call.status),
                call.raw_input.as_ref(),
                &call.content,
                true,
            ),
            _ if self.tools.contains_key(&tool_call_id) => Err(format!(
                "Claude plan handoff tool call {tool_call_id:?} changed or omitted its metadata identity"
            )),
            _ if mutating_tool_report(Some(call.kind), &call.content) => Err(
                "agent reported a mutating tool operation outside the configured Claude plan artifact handoff"
                    .to_string(),
            ),
            _ => Ok(()),
        }
    }

    pub(super) fn inspect_tool_update(&mut self, update: &ToolCallUpdate) -> Result<(), String> {
        let tool_call_id = update.tool_call_id.to_string();
        if self.retired_tools.contains(&tool_call_id)
            && !(matches!(
                update.fields.status,
                Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
            ) || (update.fields.status.is_none()
                && self.exit_terminals.contains_key(&tool_call_id)))
        {
            return Err(
                "stale Claude tool refinement belongs to an earlier proposal generation".into(),
            );
        }
        let known = self.tools.get(&tool_call_id).cloned();
        let tool_name = claude_tool_name(update.meta.as_ref());
        if let Some(operation) = tool_name.and_then(ClaudePlanArtifactOperation::from_tool_name) {
            return self.inspect_artifact_mutation(
                &tool_call_id,
                operation,
                update.fields.kind,
                update.fields.status,
                update.fields.raw_input.as_ref(),
                update.fields.locations.as_deref().unwrap_or_default(),
                update.fields.content.as_deref().unwrap_or_default(),
                false,
            );
        }
        match tool_name {
            Some("ExitPlanMode") => self.inspect_exit_plan_mode(
                &tool_call_id,
                update.fields.kind,
                update.fields.status,
                update.fields.raw_input.as_ref(),
                update.fields.content.as_deref().unwrap_or_default(),
                false,
            ),
            _ if known.is_some()
                || mutating_tool_report(
                    update.fields.kind,
                    update.fields.content.as_deref().unwrap_or_default(),
                ) =>
            {
                Err(
                    "Claude plan handoff tool update is missing matching _meta.claudeCode.toolName metadata"
                        .to_string(),
                )
            }
            _ => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn inspect_artifact_mutation(
        &mut self,
        tool_call_id: &str,
        operation: ClaudePlanArtifactOperation,
        kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        raw_input: Option<&serde_json::Value>,
        locations: &[agent_client_protocol::schema::v1::ToolCallLocation],
        content: &[ToolCallContent],
        initial: bool,
    ) -> Result<(), String> {
        let existing = self.tools.get(tool_call_id).cloned();
        let previously_terminal = matches!(&existing, Some(ClaudePlanTool::ArtifactMutation { lifecycle, .. }) if lifecycle.is_terminal());
        self.tool_generations
            .entry(tool_call_id.to_string())
            .or_insert(self.generation);
        match &existing {
            Some(ClaudePlanTool::ArtifactMutation {
                operation: previous,
                ..
            }) if *previous != operation => {
                return Err(format!(
                    "Claude plan artifact tool call {tool_call_id:?} changed operation from {} to {}",
                    previous.tool_name(),
                    operation.tool_name()
                ));
            }
            Some(ClaudePlanTool::PendingExitPlanMode | ClaudePlanTool::ExitPlanMode { .. }) => {
                return Err(format!(
                    "ACP tool call {tool_call_id:?} changed from ExitPlanMode to {}",
                    operation.tool_name()
                ));
            }
            _ => {}
        }

        if existing.is_none() && !matches!(self.phase, ClaudePlanHandoffPhase::Active) {
            return Err(
                "Claude attempted another plan artifact mutation after handoff began".to_string(),
            );
        }

        if kind.is_some_and(|kind| kind != ToolKind::Edit) {
            return Err(format!(
                "Claude {} handoff reported a non-edit ACP tool kind",
                operation.tool_name()
            ));
        }

        let target = self.artifact_mutation_target(raw_input, locations, content)?;
        if target.is_some()
            && matches!(
                existing,
                Some(ClaudePlanTool::ArtifactMutation { path: None, .. })
            )
            && !matches!(self.phase, ClaudePlanHandoffPhase::Active)
        {
            return Err(format!(
                "Claude plan artifact {tool_call_id:?} acquired its first target after handoff began"
            ));
        }
        let reported_terminal = status
            .as_ref()
            .and_then(ClaudePlanArtifactLifecycle::terminal_from_status);
        let reported_nonterminal = status.is_some() && reported_terminal.is_none();

        let (path, lifecycle, announcement) = match existing {
            Some(ClaudePlanTool::ArtifactMutation {
                path: previous_path,
                lifecycle: previous_lifecycle,
                announcement,
                ..
            }) => {
                let path = match (previous_path, target) {
                    (Some(previous), Some(target)) if previous != target => {
                        return Err(format!(
                            "Claude plan artifact {} {tool_call_id:?} changed targets from {} to {}",
                            operation.tool_name(),
                            previous.display(),
                            target.display()
                        ));
                    }
                    (Some(previous), _) => Some(previous),
                    (None, target) => target,
                };
                let lifecycle = if previous_lifecycle.is_terminal() {
                    match reported_terminal {
                        Some(reported) => {
                            if reported != previous_lifecycle {
                                return Err(format!(
                                    "Claude plan artifact {} {tool_call_id:?} changed terminal status from {previous_lifecycle:?} to {reported:?}",
                                    operation.tool_name()
                                ));
                            }
                            previous_lifecycle
                        }
                        None if reported_nonterminal => {
                            return Err(format!(
                                "Claude plan artifact {} {tool_call_id:?} regressed from terminal status {previous_lifecycle:?}",
                                operation.tool_name()
                            ));
                        }
                        None => previous_lifecycle,
                    }
                } else {
                    reported_terminal.unwrap_or(ClaudePlanArtifactLifecycle::Active)
                };
                (path, lifecycle, announcement)
            }
            None => {
                if target.is_none() && !initial {
                    return Err(format!(
                        "unknown {} update for ACP tool call {tool_call_id:?} without a plan artifact path",
                        operation.tool_name()
                    ));
                }
                let announcement = self.next_announcement;
                self.next_announcement += 1;
                (
                    target,
                    reported_terminal.unwrap_or(ClaudePlanArtifactLifecycle::Active),
                    announcement,
                )
            }
            Some(ClaudePlanTool::PendingExitPlanMode | ClaudePlanTool::ExitPlanMode { .. }) => {
                unreachable!("ExitPlanMode identity was rejected above")
            }
        };

        if path.is_none() && lifecycle.is_terminal() {
            return Err(format!(
                "Claude plan artifact {} {tool_call_id:?} became terminal without a validated path",
                operation.tool_name()
            ));
        }

        let evidence = self
            .artifact_evidence
            .entry(tool_call_id.to_string())
            .or_default();
        evidence.observe_write_content(operation, raw_input)?;
        if let Some(parent) = path.as_ref().and_then(|path| path.parent()) {
            let identity = zevria_foundation::contained_read::OpenedRoot::open_absolute(parent)
                .ok()
                .and_then(|root| root.identity().ok());
            if evidence.directory_identity.is_some() && identity != evidence.directory_identity {
                return Err("Claude artifact directory was replaced during mutation".into());
            }
            evidence.directory_identity = identity;
        }
        if lifecycle.is_terminal() {
            validate_claude_plan_artifact_path(
                path.as_ref().expect("terminal target validated above"),
                &self.artifact_directory,
                &self.workspace_artifact_directory,
                &self.workspace,
            )?;
            if !previously_terminal {
                evidence.file_version =
                    artifact_file_snapshot(path.as_ref().expect("validated terminal path"));
            }
            // Only matching terminal evidence releases ownership. Telemetry,
            // responder completion, and unrelated/replayed terminals do not.
            if self.granted_tool.as_deref() == Some(tool_call_id) {
                self.granted_tool = None;
            }
            self.permissions
                .retain(|ticket| ticket.tool_call_id != tool_call_id);
        }
        self.tools.insert(
            tool_call_id.to_string(),
            ClaudePlanTool::ArtifactMutation {
                operation,
                path,
                lifecycle,
                announcement,
            },
        );
        Ok(())
    }

    pub(super) fn inspect_exit_plan_mode(
        &mut self,
        tool_call_id: &str,
        kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        raw_input: Option<&serde_json::Value>,
        content: &[ToolCallContent],
        initial: bool,
    ) -> Result<(), String> {
        if kind.is_some_and(|kind| kind != ToolKind::SwitchMode) {
            return Err("Claude ExitPlanMode reported an unexpected ACP tool kind".to_string());
        }
        let existing = self.tools.get(tool_call_id).cloned();
        self.tool_generations
            .entry(tool_call_id.to_string())
            .or_insert(self.generation);
        if let Some(hint) = extract_handoff_path_hint(raw_input)? {
            if self
                .exit_hints
                .get(tool_call_id)
                .is_some_and(|previous| previous != &hint)
            {
                return Err("Claude ExitPlanMode changed its planFilePath".into());
            }
            self.exit_hints.insert(tool_call_id.to_string(), hint);
        }
        if let Some(ClaudePlanTool::ArtifactMutation { operation, .. }) = &existing {
            return Err(format!(
                "ACP tool call {tool_call_id:?} changed from {} to ExitPlanMode",
                operation.tool_name()
            ));
        }
        let terminal = self.exit_terminals.contains_key(tool_call_id)
            || matches!(
                status,
                Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
            );
        if existing.is_none() && !matches!(self.phase, ClaudePlanHandoffPhase::Active) {
            return Err("Claude attempted more than one ExitPlanMode handoff".into());
        }
        if let Some(previous) = self.exit_terminals.get(tool_call_id) {
            if status.is_some_and(|status| status != *previous) {
                return Err("Claude ExitPlanMode changed its provider terminal status".into());
            }
        } else if terminal {
            self.exit_terminals
                .insert(tool_call_id.to_string(), status.expect("terminal status"));
        }
        let plan = if terminal {
            // Zevria deliberately rejects ExitPlanMode after capturing its plan so
            // Claude stays in planning mode. The provider tool may therefore finish
            // as failed even though the native handoff completed successfully. ACP
            // terminal content is tool output; only a repeated rawInput.plan can
            // assert (and potentially violate) payload identity at this point.
            extract_handoff_raw_plan(raw_input)?
        } else {
            extract_handoff_plan(raw_input, content)?
        };
        let Some(plan) = plan else {
            return match existing {
                Some(ClaudePlanTool::ExitPlanMode { .. }) => Ok(()),
                // A rejected exit may fail before a file-backed capture settles.
                // Terminal output is not proposal data.
                Some(ClaudePlanTool::PendingExitPlanMode) => Ok(()),
                Some(ClaudePlanTool::ArtifactMutation { operation, .. }) => Err(format!(
                    "ACP tool call {tool_call_id:?} changed from {} to ExitPlanMode",
                    operation.tool_name()
                )),
                None if initial && !terminal => {
                    self.tools.insert(
                        tool_call_id.to_string(),
                        ClaudePlanTool::PendingExitPlanMode,
                    );
                    Ok(())
                }
                None if initial => {
                    Err("Claude ExitPlanMode handoff contained no nonempty plan".to_string())
                }
                None => Err(format!(
                    "unknown ExitPlanMode update for ACP tool call {tool_call_id:?}"
                )),
            };
        };
        match existing {
            Some(ClaudePlanTool::ExitPlanMode { plan: previous }) if previous != plan => {
                Err("Claude ExitPlanMode changed its plan payload during handoff".to_string())
            }
            Some(ClaudePlanTool::ArtifactMutation { operation, .. }) => Err(format!(
                "ACP tool call {tool_call_id:?} changed from {} to ExitPlanMode",
                operation.tool_name()
            )),
            Some(ClaudePlanTool::PendingExitPlanMode | ClaudePlanTool::ExitPlanMode { .. })
            | None => {
                self.tools.insert(
                    tool_call_id.to_string(),
                    ClaudePlanTool::ExitPlanMode { plan },
                );
                Ok(())
            }
        }
    }

    pub(super) fn permission(
        &mut self,
        request: &RequestPermissionRequest,
    ) -> ClaudeHandoffPermission {
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        if self.retired_tools.contains(&tool_call_id) {
            return ClaudeHandoffPermission::Invalid(
                "stale Claude permission belongs to an earlier proposal generation".into(),
            );
        }
        let request_tool_name = claude_tool_name(request.tool_call.meta.as_ref());
        match self.tools.get(&tool_call_id).cloned() {
            Some(ClaudePlanTool::ArtifactMutation {
                operation,
                path,
                lifecycle,
                ..
            }) => {
                if !matches!(self.phase, ClaudePlanHandoffPhase::Active) {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude attempted another plan artifact mutation after handoff began"
                            .to_string(),
                    );
                }
                if lifecycle.is_terminal() {
                    return ClaudeHandoffPermission::Invalid(format!(
                        "Claude plan artifact {} {tool_call_id:?} requested permission after reaching terminal status {lifecycle:?}",
                        operation.tool_name()
                    ));
                }
                if request_tool_name.is_some_and(|name| name != operation.tool_name()) {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude plan artifact permission metadata changed tool identity"
                            .to_string(),
                    );
                }
                if request.tool_call.fields.kind != Some(ToolKind::Edit) {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude plan artifact permission reported a non-edit tool kind".to_string(),
                    );
                }
                match self.artifact_mutation_target(
                    request.tool_call.fields.raw_input.as_ref(),
                    request
                        .tool_call
                        .fields
                        .locations
                        .as_deref()
                        .unwrap_or_default(),
                    request
                        .tool_call
                        .fields
                        .content
                        .as_deref()
                        .unwrap_or_default(),
                ) {
                    Ok(Some(request_path)) => {
                        if let Some(path) = path
                            && request_path != path
                        {
                            return ClaudeHandoffPermission::Invalid(format!(
                                "Claude plan artifact permission changed target from {} to {}",
                                path.display(),
                                request_path.display()
                            ));
                        }
                        let evidence = self
                            .artifact_evidence
                            .entry(tool_call_id.clone())
                            .or_default();
                        if let Err(error) = evidence.observe_write_content(
                            operation,
                            request.tool_call.fields.raw_input.as_ref(),
                        ) {
                            return ClaudeHandoffPermission::Invalid(error);
                        }
                        if evidence.directory_identity.is_none() {
                            evidence.directory_identity = request_path
                                .parent()
                                .and_then(|parent| {
                                    zevria_foundation::contained_read::OpenedRoot::open_absolute(
                                        parent,
                                    )
                                    .ok()
                                })
                                .and_then(|root| root.identity().ok());
                        }
                        if let Some(ClaudePlanTool::ArtifactMutation { path, .. }) =
                            self.tools.get_mut(&tool_call_id)
                        {
                            *path = Some(request_path);
                        }
                        ClaudeHandoffPermission::ArtifactMutation
                    }
                    Ok(None) => ClaudeHandoffPermission::Invalid(
                        "Claude plan artifact permission omitted the validated target".to_string(),
                    ),
                    Err(error) => ClaudeHandoffPermission::Invalid(error),
                }
            }
            Some(ClaudePlanTool::PendingExitPlanMode | ClaudePlanTool::ExitPlanMode { .. }) => {
                if request_tool_name.is_some_and(|name| name != "ExitPlanMode") {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude ExitPlanMode permission metadata changed tool identity".to_string(),
                    );
                }
                if request.tool_call.fields.kind != Some(ToolKind::SwitchMode) {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude ExitPlanMode permission reported an unexpected ACP tool kind"
                            .to_string(),
                    );
                }
                let request_plan = match extract_handoff_plan(
                    request.tool_call.fields.raw_input.as_ref(),
                    request
                        .tool_call
                        .fields
                        .content
                        .as_deref()
                        .unwrap_or_default(),
                ) {
                    Ok(plan) => plan,
                    Err(error) => return ClaudeHandoffPermission::Invalid(error),
                };
                let hint =
                    match extract_handoff_path_hint(request.tool_call.fields.raw_input.as_ref()) {
                        Ok(hint) => hint,
                        Err(error) => return ClaudeHandoffPermission::Invalid(error),
                    };
                if let Some(hint) = hint {
                    if self
                        .exit_hints
                        .get(&tool_call_id)
                        .is_some_and(|previous| previous != &hint)
                    {
                        return ClaudeHandoffPermission::Invalid(
                            "Claude ExitPlanMode permission changed planFilePath".into(),
                        );
                    }
                    self.exit_hints.insert(tool_call_id.clone(), hint);
                }
                let previous = match self.tools.get(&tool_call_id) {
                    Some(ClaudePlanTool::ExitPlanMode { plan }) => Some(plan.clone()),
                    _ => None,
                };
                if let (Some(previous), Some(request)) = (&previous, &request_plan)
                    && previous != request
                {
                    return ClaudeHandoffPermission::Invalid(
                        "Claude ExitPlanMode permission changed its plan payload".into(),
                    );
                }
                let source = match request_plan.or(previous) {
                    Some(plan) => {
                        self.tools.insert(
                            tool_call_id.clone(),
                            ClaudePlanTool::ExitPlanMode { plan: plan.clone() },
                        );
                        ClaudePlanCandidate::Explicit(plan)
                    }
                    None => ClaudePlanCandidate::Artifact {
                        path_hint: self.exit_hints.get(&tool_call_id).cloned(),
                    },
                };
                ClaudeHandoffPermission::ExitPlanMode { source }
            }
            None if matches!(
                request_tool_name,
                Some("Write" | "Edit" | "MultiEdit" | "ExitPlanMode")
            ) || (request.tool_call.fields.kind == Some(ToolKind::SwitchMode)
                && request
                    .tool_call
                    .fields
                    .raw_input
                    .as_ref()
                    .is_some_and(|input| {
                        input
                            .as_object()
                            .is_some_and(|input| input.contains_key("plan"))
                    })) =>
            {
                ClaudeHandoffPermission::Invalid(format!(
                    "Claude handoff permission referenced unknown ACP tool call {tool_call_id:?}"
                ))
            }
            None => ClaudeHandoffPermission::Ordinary,
        }
    }

    pub(super) fn prepare_artifact_mutation(
        &self,
        request: &RequestPermissionRequest,
    ) -> Result<(), String> {
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        let expected_path = match self.tools.get(&tool_call_id) {
            Some(ClaudePlanTool::ArtifactMutation {
                path: Some(path),
                lifecycle: ClaudePlanArtifactLifecycle::Active,
                ..
            }) => path.clone(),
            _ => {
                return Err(format!(
                    "Claude plan artifact mutation {tool_call_id:?} is no longer active and validated"
                ));
            }
        };
        let request_path = self
            .artifact_mutation_target(
                request.tool_call.fields.raw_input.as_ref(),
                request
                    .tool_call
                    .fields
                    .locations
                    .as_deref()
                    .unwrap_or_default(),
                request
                    .tool_call
                    .fields
                    .content
                    .as_deref()
                    .unwrap_or_default(),
            )?
            .ok_or_else(|| {
                "Claude plan artifact permission omitted the validated target".to_string()
            })?;
        if request_path != expected_path {
            return Err(format!(
                "Claude plan artifact permission changed target from {} to {}",
                expected_path.display(),
                request_path.display()
            ));
        }
        if request_path.parent() != Some(self.workspace_artifact_directory.as_path()) {
            return Ok(());
        }

        prepare_workspace_claude_plan_artifact_directory(
            &self.workspace_artifact_directory,
            &self.workspace,
        )?;
        let prepared_path = validate_claude_plan_artifact_path(
            &request_path,
            &self.artifact_directory,
            &self.workspace_artifact_directory,
            &self.workspace,
        )?;
        if prepared_path != expected_path {
            return Err(format!(
                "Claude plan artifact target changed while preparing its workspace directory from {} to {}",
                expected_path.display(),
                prepared_path.display()
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn begin_capture(&mut self, tool_call_id: &str, plan: &str) -> Result<(), String> {
        if self.retired_tools.contains(tool_call_id) {
            return Err("stale Claude handoff belongs to an earlier proposal generation".into());
        }
        if !matches!(self.phase, ClaudePlanHandoffPhase::Active) {
            return Err("Claude attempted more than one ExitPlanMode handoff".to_string());
        }
        let unresolved = self.unresolved_artifacts();
        if !unresolved.is_empty() {
            return Err(format!(
                "Claude ExitPlanMode cannot capture while plan artifacts remain unresolved: {unresolved}"
            ));
        }
        if self.tools.iter().any(|(id, tool)| {
            self.tool_generations.get(id) == Some(&self.generation)
                && matches!(tool, ClaudePlanTool::PendingExitPlanMode)
        }) {
            return Err(
                "Claude ExitPlanMode cannot capture while a handoff payload remains unresolved"
                    .to_string(),
            );
        }
        match self.tools.get(tool_call_id) {
            Some(ClaudePlanTool::ExitPlanMode { plan: expected }) if expected == plan => {
                self.phase = ClaudePlanHandoffPhase::Capturing {
                    tool_call_id: tool_call_id.to_string(),
                };
                Ok(())
            }
            _ => Err("Claude ExitPlanMode handoff no longer matched its tool call".to_string()),
        }
    }

    pub(super) fn artifact_mutation_target(
        &self,
        raw_input: Option<&serde_json::Value>,
        locations: &[agent_client_protocol::schema::v1::ToolCallLocation],
        content: &[ToolCallContent],
    ) -> Result<Option<PathBuf>, String> {
        let mut paths = Vec::new();
        if let Some(raw_input) = raw_input {
            let object = raw_input.as_object().ok_or_else(|| {
                "Claude plan artifact mutation raw input must be an object when present".to_string()
            })?;
            if let Some(file_path) = object.get("file_path") {
                let file_path = file_path.as_str().ok_or_else(|| {
                    "Claude plan artifact mutation raw input file_path must be a string".to_string()
                })?;
                paths.push(PathBuf::from(file_path));
            }
        }
        paths.extend(locations.iter().map(|location| location.path.clone()));
        for content in content {
            if let ToolCallContent::Diff(diff) = content {
                paths.push(diff.path.clone());
            }
        }
        if paths.is_empty() {
            return Ok(None);
        }
        let mut resolved = None;
        for path in paths {
            let path = validate_claude_plan_artifact_path(
                &path,
                &self.artifact_directory,
                &self.workspace_artifact_directory,
                &self.workspace,
            )?;
            if resolved.as_ref().is_some_and(|resolved| resolved != &path) {
                return Err("Claude plan artifact mutation reported conflicting paths".to_string());
            }
            resolved = Some(path);
        }
        Ok(resolved)
    }
}

pub(super) fn claude_plan_artifact_directory(
    agent: &EnsembleAgentConfig,
    workspace: &Path,
) -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let normalized_workspace = zevria_foundation::windows_io::normalize_disk_path(workspace)?;
    #[cfg(windows)]
    let workspace = normalized_workspace.as_path();
    let configured = match agent.env.get(CLAUDE_CONFIG_DIR_ENV) {
        Some(value) if value.is_empty() => {
            anyhow::bail!("ensemble agent CLAUDE_CONFIG_DIR override must not be empty")
        }
        Some(value) => Some(PathBuf::from(value)),
        None => std::env::var_os(CLAUDE_CONFIG_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    };
    let config_directory = match configured {
        Some(configured) => configured,
        None => platform_home_directory()
            .context("cannot resolve the platform home for Claude plan handoff")?
            .join(".claude"),
    };
    let requested = config_directory.join("plans");
    #[cfg(windows)]
    {
        windows_artifact_path::artifact_directory(&requested, workspace)
            .context("unsupported Windows Claude artifact directory")
    }
    #[cfg(not(windows))]
    {
        let workspace_artifact_directory = workspace_claude_plan_artifact_directory(workspace);
        if requested == workspace_artifact_directory {
            validate_workspace_claude_plan_artifact_directory(
                &workspace_artifact_directory,
                workspace,
            )
            .map_err(anyhow::Error::msg)?;
            return Ok(workspace_artifact_directory);
        }
        if requested.is_absolute() && requested.starts_with(workspace) {
            anyhow::bail!(
                "Claude plan artifact directory {} is inside the ensemble workspace but is not the permitted workspace-local .claude/plans directory",
                requested.display()
            );
        }
        let artifact_directory = std::fs::canonicalize(&requested).with_context(|| {
            format!(
                "failed to resolve the Claude plan artifact directory at {}",
                requested.display()
            )
        })?;
        if artifact_directory == workspace_artifact_directory {
            validate_workspace_claude_plan_artifact_directory(
                &workspace_artifact_directory,
                workspace,
            )
            .map_err(anyhow::Error::msg)?;
            return Ok(workspace_artifact_directory);
        }
        if artifact_directory.starts_with(workspace) {
            anyhow::bail!(
                "Claude plan artifact directory {} is inside the ensemble workspace but is not the permitted workspace-local .claude/plans directory",
                artifact_directory.display()
            );
        }
        Ok(artifact_directory)
    }
}

pub(super) fn workspace_claude_plan_artifact_directory(workspace: &Path) -> PathBuf {
    workspace.join(".claude").join("plans")
}

pub(super) fn validate_workspace_claude_plan_artifact_directory(
    directory: &Path,
    workspace: &Path,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows_artifact_path::validate_workspace_directory(directory, workspace)
            .map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        let claude_directory = workspace.join(".claude");
        if directory != claude_directory.join("plans") {
            return Err(format!(
                "Claude workspace plan artifact directory {} is not <workspace>/.claude/plans",
                directory.display()
            ));
        }

        let mut plan_directory_exists = false;
        for (component, label) in [
            (claude_directory.as_path(), "workspace .claude directory"),
            (directory, "workspace .claude/plans directory"),
        ] {
            match std::fs::symlink_metadata(component) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "Claude {label} {} must not be a symlink",
                        component.display()
                    ));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(format!(
                        "Claude {label} {} must be a directory",
                        component.display()
                    ));
                }
                Ok(_) => {
                    if component == directory {
                        plan_directory_exists = true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect Claude {label} {}: {error}",
                        component.display()
                    ));
                }
            }
        }

        if plan_directory_exists {
            let canonical_directory = std::fs::canonicalize(directory).map_err(|error| {
                format!(
                    "failed to resolve Claude workspace plan artifact directory {}: {error}",
                    directory.display()
                )
            })?;
            if canonical_directory != directory {
                return Err(format!(
                    "Claude workspace plan artifact directory {} resolves outside the exact workspace-local .claude/plans path",
                    directory.display()
                ));
            }
        }
        Ok(())
    }
}

pub(super) fn prepare_workspace_claude_plan_artifact_directory(
    directory: &Path,
    workspace: &Path,
) -> Result<(), String> {
    validate_workspace_claude_plan_artifact_directory(directory, workspace)?;
    std::fs::create_dir_all(directory).map_err(|error| {
        format!(
            "failed to create Claude workspace plan artifact directory {}: {error}",
            directory.display()
        )
    })?;
    validate_workspace_claude_plan_artifact_directory(directory, workspace)
}

pub(super) fn platform_home_directory() -> Option<PathBuf> {
    zevria_foundation::runtime_paths::home_dir().ok()
}

pub(super) fn validate_claude_plan_artifact_path(
    path: &Path,
    artifact_directory: &Path,
    workspace_artifact_directory: &Path,
    workspace: &Path,
) -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        windows_artifact_path::validate_artifact_path(
            path,
            artifact_directory,
            workspace_artifact_directory,
            workspace,
        )
        .map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        if !path.is_absolute() {
            return Err("Claude plan artifact path must be absolute".to_string());
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            return Err("Claude plan artifact must be a Markdown file".to_string());
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| "Claude plan artifact path has no file name".to_string())?;
        let parent = path
            .parent()
            .ok_or_else(|| "Claude plan artifact path has no parent directory".to_string())?;

        if parent == workspace_artifact_directory {
            validate_workspace_claude_plan_artifact_directory(
                workspace_artifact_directory,
                workspace,
            )?;
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("Claude plan artifact target must not be a symlink".to_string());
                }
                Ok(metadata) if !metadata.is_file() => {
                    return Err("Claude plan artifact target must be a regular file".to_string());
                }
                Ok(_) => {
                    let canonical_target = std::fs::canonicalize(path).map_err(|error| {
                        format!(
                            "failed to resolve existing Claude plan artifact {}: {error}",
                            path.display()
                        )
                    })?;
                    if canonical_target.parent() != Some(workspace_artifact_directory) {
                        return Err(
                            "Claude workspace plan artifact target resolves outside .claude/plans"
                                .to_string(),
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect Claude plan artifact {}: {error}",
                        path.display()
                    ));
                }
            }
            return Ok(workspace_artifact_directory.join(file_name));
        }
        if path.starts_with(workspace) {
            return Err(
                "Claude plan artifact workspace path must be a direct child of .claude/plans"
                    .to_string(),
            );
        }

        let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
            format!(
                "failed to resolve Claude plan artifact parent {}: {error}",
                parent.display()
            )
        })?;
        if canonical_parent != artifact_directory {
            return Err(format!(
                "Claude Write target {} is not a direct child of either permitted plan artifact directory",
                path.display()
            ));
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Claude plan artifact target must not be a symlink".to_string());
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err("Claude plan artifact target must be a regular file".to_string());
            }
            Ok(_) => {
                let canonical_target = std::fs::canonicalize(path).map_err(|error| {
                    format!(
                        "failed to resolve existing Claude plan artifact {}: {error}",
                        path.display()
                    )
                })?;
                if canonical_target.starts_with(workspace) {
                    return Err(
                        "Claude plan artifact target resolves inside the workspace".to_string()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect Claude plan artifact {}: {error}",
                    path.display()
                ));
            }
        }
        Ok(artifact_directory.join(file_name))
    }
}

pub(super) fn claude_tool_name(meta: Option<&Meta>) -> Option<&str> {
    meta?
        .get("claudeCode")?
        .as_object()?
        .get("toolName")?
        .as_str()
}

pub(super) fn mutating_tool_report(kind: Option<ToolKind>, content: &[ToolCallContent]) -> bool {
    content
        .iter()
        .any(|content| matches!(content, ToolCallContent::Diff(_)))
        || matches!(
            kind,
            Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move)
        )
}

pub(super) fn extract_handoff_plan(
    raw_input: Option<&serde_json::Value>,
    content: &[ToolCallContent],
) -> Result<Option<String>, String> {
    let raw_plan = extract_handoff_raw_plan(raw_input)?;
    let content_plan = extract_handoff_content_plan(content)?;
    match (raw_plan, content_plan) {
        (Some(raw), Some(content)) if raw != content => {
            Err("Claude ExitPlanMode raw input and rendered plan content disagree".to_string())
        }
        (Some(plan), _) | (_, Some(plan)) => Ok(Some(plan)),
        (None, None) => Ok(None),
    }
}

pub(super) fn extract_handoff_raw_plan(
    raw_input: Option<&serde_json::Value>,
) -> Result<Option<String>, String> {
    let raw_plan = match raw_input {
        Some(raw_input) => {
            let object = raw_input
                .as_object()
                .ok_or_else(|| "Claude ExitPlanMode raw input must be an object".to_string())?;
            object
                .get("plan")
                .map(|plan| {
                    plan.as_str()
                        .ok_or_else(|| {
                            "Claude ExitPlanMode plan payload must be a string".to_string()
                        })
                        .and_then(normalize_handoff_plan)
                })
                .transpose()?
        }
        None => None,
    };
    Ok(raw_plan)
}

pub(super) fn extract_handoff_content_plan(
    content: &[ToolCallContent],
) -> Result<Option<String>, String> {
    let mut content_plan = String::new();
    for item in content {
        match item {
            ToolCallContent::Content(item) => match &item.content {
                ContentBlock::Text(text) => content_plan.push_str(&text.text),
                _ => {
                    return Err(
                        "Claude ExitPlanMode plan content must contain only text".to_string()
                    );
                }
            },
            _ => {
                return Err("Claude ExitPlanMode plan content must contain only text".to_string());
            }
        }
    }
    (!content_plan.is_empty())
        .then(|| normalize_handoff_plan(&content_plan))
        .transpose()
}

pub(super) fn normalize_handoff_plan(plan: &str) -> Result<String, String> {
    let plan = plan.replace("\r\n", "\n").replace('\r', "\n");
    let plan = plan.trim();
    if plan.is_empty() {
        return Err("Claude ExitPlanMode handoff contained an empty plan".to_string());
    }
    Ok(plan.to_string())
}
