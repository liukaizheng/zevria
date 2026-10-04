//! Derived render state, never authoritative session state.
use super::*;

const fn role_label(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Build => "Build",
        ModelRole::Plan => "Plan",
        ModelRole::Review => "Review",
        ModelRole::Explore => "Explore",
        ModelRole::Builder => "Builder",
    }
}

/// Read-only pane-local durations evaluated once against the observed UI clock.
/// This projection is decoration only; it never changes a semantic block revision.
#[derive(Default)]
pub(crate) struct HeaderTimings {
    elapsed: std::collections::HashMap<crate::presentation::NativeHeader, Duration>,
}

impl HeaderTimings {
    pub(crate) fn observe(conversation: &ConversationState, now: Instant) -> Self {
        Self {
            elapsed: conversation.header_timings(now).collect(),
        }
    }

    pub(crate) fn elapsed(&self, header: crate::presentation::NativeHeader) -> Option<Duration> {
        self.elapsed.get(&header).copied()
    }
}

/// Derived conversation tail consumed directly by rendering.
pub(crate) enum ConversationTail<'a> {
    None,
    Compacting {
        elapsed: Duration,
    },
    Waiting {
        elapsed: Duration,
    },
    Streaming {
        message: &'a Message,
        elapsed: Duration,
    },
    Retrying {
        notice: &'a RetryNotice,
        countdown: RetryCountdown,
        elapsed: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderFocus {
    Normal,
    Insert,
}

/// Action-only composer-title precedence consumed directly by rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComposerChrome {
    Inspect,
    Selecting {
        can_recall: bool,
        scope: SelectionScope,
    },
    Busy,
    ModePending,
    WorkerBusy,
    Worker {
        focus: RenderFocus,
        command: bool,
        can_confirm: bool,
    },
    PersistenceDegraded,
    FreshPlanRetry,
    Command,
    Recalling {
        mode: SessionMode,
    },
    Idle {
        focus: RenderFocus,
        mode: SessionMode,
        retained_plan: bool,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct PlanDialogView<'a> {
    pub(crate) artifact: &'a zevria_workflow::PlanArtifact,
    pub(crate) state: PlanDialogState,
    pub(crate) decision_pending: bool,
}

/// Disjoint render borrows assembled inside the façade so sibling modules do
/// not need direct access to private substate fields.
pub(crate) struct RenderParts<'a> {
    pub(crate) history: &'a [HistoryEntry],
    pub(crate) turn_starts: Vec<super::conversation::TurnStartTarget>,
    pub(crate) folds: &'a FoldState,
    pub(crate) selected: Option<ActiveSelection>,
    pub(crate) selection_reveal: bool,
    pub(crate) diagnostics_visible: bool,
    pub(crate) appearance: crate::presentation::TranscriptAppearance,
    pub(crate) input: &'a str,
    pub(crate) input_cursor: usize,
    pub(crate) image_ranges: Vec<std::ops::Range<usize>>,
    pub(crate) paste_pending: bool,
    pub(crate) menu: &'a CompletionMenu,
    pub(crate) completion: Option<CompletionView<'a>>,
    pub(crate) command_menu_active: bool,
    pub(crate) composer_locked: bool,
    #[cfg(test)]
    pub(crate) editor_active: bool,
    pub(crate) draft_recovery: crate::input::DraftRecoveryHint,
    pub(crate) mode: SessionMode,
    pub(crate) retained_plan: bool,
    pub(crate) chrome: ComposerChrome,
    pub(crate) plan_dialog: Option<PlanDialogView<'a>>,
    pub(crate) tail: ConversationTail<'a>,
    pub(crate) network_status: Option<(String, bool)>,
    pub(crate) assistant_header: Option<crate::presentation::NativeHeader>,
    pub(crate) header_timings: HeaderTimings,
    pub(crate) pending_header: bool,
    pub(crate) status: StatusBarView,
    pub(crate) view: &'a mut ViewState,
}

impl App {
    pub(crate) fn status_accent(&self) -> StatusAccent {
        if !self.pane.is_root() {
            StatusAccent::Inspect
        } else {
            match self.session.display_role(self.pane.model_role_override()) {
                ModelRole::Build | ModelRole::Builder => StatusAccent::Build,
                ModelRole::Plan => StatusAccent::Plan,
                ModelRole::Review => StatusAccent::Review,
                ModelRole::Explore => StatusAccent::Explore,
            }
        }
    }

    pub(crate) fn render_parts(&mut self) -> RenderParts<'_> {
        let generated_controls =
            crate::hints::hint_line(self.surface().context(), &self.hint_eligibility(), 100)
                .to_string();
        let accent = self.status_accent();
        let work_pending = self.work_pending();
        let composer_locked = !self.can_edit_draft();
        #[cfg(test)]
        let editor_active = self.composer_editable();
        let draft_recovery = if !self.drafts.has_recovery() {
            crate::input::DraftRecoveryHint::None
        } else if self.interaction.is_normal() && !composer_locked && self.composer.is_empty() {
            crate::input::DraftRecoveryHint::Available
        } else {
            crate::input::DraftRecoveryHint::Saved
        };
        let worker_can_confirm = self.pane.is_worker()
            && self.interaction.is_normal()
            && self.composer.is_empty()
            && !self.composer.is_paste_pending()
            && self
                .worker
                .bound
                .as_ref()
                .and_then(|(_, state)| state.eligible_snapshot())
                .is_some_and(|plan| {
                    self.worker.eligible(&WorkerControlAction::Confirm {
                        expected_revision: plan.revision.clone(),
                    })
                });
        let command_menu_active = self.command_menu_active();
        let file_menu_active =
            command_menu_active && self.composer.completion_kind() == Some(CompletionKind::File);
        let can_recall = self.can_recall_selected();
        let Self {
            session,
            workflow,
            conversation,
            folds,
            composer,
            interaction,
            edit,
            pane,
            model_profiles,
            view,
            ..
        } = self;
        let selected = interaction.active_selection();
        let diagnostics_visible = pane.diagnostics_visible();
        let mode = session.next_mode();
        let retained_plan = workflow.submitted_artifact().is_some();
        let plan_dialog = workflow.dialog().and_then(|state| {
            workflow.dialog_artifact().map(|artifact| PlanDialogView {
                artifact,
                state,
                decision_pending: session.is_busy(),
            })
        });
        let pending_manual = session.operation_kind() == Some(OperationKind::ManualCompaction)
            && matches!(session.activity(), SessionActivity::Pending(_));
        let elapsed = session.elapsed().unwrap_or_default();
        let assistant_header = (pane.appearance()
            == crate::presentation::TranscriptAppearance::Native)
            .then(|| session.call_header())
            .flatten();
        let pending_header =
            assistant_header.is_some_and(|header| !conversation.represents_header(header));
        let tail = if session.is_compacting() || pending_manual {
            ConversationTail::Compacting { elapsed }
        } else {
            match session.activity() {
                SessionActivity::Idle => ConversationTail::None,
                SessionActivity::Pending(pending)
                    if pending.kind.is_plan_decision()
                        || pending.kind == OperationKind::ModeManagement =>
                {
                    ConversationTail::None
                }
                SessionActivity::Pending(_) => ConversationTail::Waiting { elapsed },
                SessionActivity::Active(active) => match &active.phase {
                    ActivePhase::Compacting { .. } => ConversationTail::Compacting { elapsed },
                    ActivePhase::AwaitingStart | ActivePhase::Running(TurnTail::Waiting) => {
                        ConversationTail::Waiting { elapsed }
                    }
                    ActivePhase::Running(TurnTail::Streaming(message)) => {
                        ConversationTail::Streaming { message, elapsed }
                    }
                    ActivePhase::Running(TurnTail::Retrying(notice)) => {
                        ConversationTail::Retrying {
                            notice,
                            countdown: notice.countdown(session.observed_at()),
                            elapsed,
                        }
                    }
                },
            }
        };
        let chrome = if !pane.can_compose() {
            ComposerChrome::Inspect
        } else if let Some(active) = selected {
            ComposerChrome::Selecting {
                can_recall,
                scope: active.scope,
            }
        } else if pane.is_worker() {
            if work_pending {
                ComposerChrome::WorkerBusy
            } else {
                ComposerChrome::Worker {
                    focus: if interaction.is_insert() {
                        RenderFocus::Insert
                    } else {
                        RenderFocus::Normal
                    },
                    command: command_menu_active && !file_menu_active,
                    can_confirm: worker_can_confirm,
                }
            }
        } else if session.operation_kind() == Some(OperationKind::ModeManagement) {
            ComposerChrome::ModePending
        } else if work_pending {
            ComposerChrome::Busy
        } else if session.persistence_error().is_some() {
            ComposerChrome::PersistenceDegraded
        } else if workflow.fresh_retry_version().is_some() {
            ComposerChrome::FreshPlanRetry
        } else if command_menu_active && !file_menu_active {
            ComposerChrome::Command
        } else if edit.is_recalling() {
            ComposerChrome::Recalling { mode }
        } else {
            ComposerChrome::Idle {
                focus: if interaction.is_insert() {
                    RenderFocus::Insert
                } else {
                    RenderFocus::Normal
                },
                mode,
                retained_plan,
            }
        };

        let role = session.display_role(pane.model_role_override());
        let role_name = role_label(role);
        let activity_primary = if session.is_compacting() || pending_manual {
            Some(format!("{role_name} · Compacting"))
        } else {
            match session.activity() {
                SessionActivity::Idle => None,
                SessionActivity::Pending(pending)
                    if pending.kind == OperationKind::ModeManagement =>
                {
                    Some("Selecting mode".to_string())
                }
                SessionActivity::Pending(_) => Some(format!("{role_name} · Waiting")),
                SessionActivity::Active(active) => match &active.phase {
                    ActivePhase::Compacting { .. } => Some(format!("{role_name} · Compacting")),
                    ActivePhase::AwaitingStart | ActivePhase::Running(TurnTail::Waiting) => {
                        Some(format!("{role_name} · Waiting"))
                    }
                    ActivePhase::Running(TurnTail::Streaming(_)) => {
                        Some(format!("{role_name} · Streaming"))
                    }
                    ActivePhase::Running(TurnTail::Retrying(notice)) => Some(format!(
                        "{role_name} · Reconnecting {}/{}",
                        notice.attempt, notice.max_attempts
                    )),
                },
            }
        };
        let ordinary_primary = if let Some(dialog) = plan_dialog {
            if dialog.state.recovering() {
                "Submitted plan".to_string()
            } else {
                "Plan ready".to_string()
            }
        } else if let Some(title) = pane.title() {
            title.to_string()
        } else if selected.is_some() {
            format!("{role_name} · Select")
        } else if command_menu_active {
            format!(
                "{role_name} · {}",
                if file_menu_active { "Files" } else { "Command" }
            )
        } else if edit.is_recalling() {
            format!("{role_name} · Recall")
        } else if let Some(activity) = activity_primary {
            activity
        } else {
            format!(
                "{role_name} · {}",
                if interaction.is_insert() {
                    "Insert"
                } else {
                    "Normal"
                }
            )
        };
        let (primary, tone, detail) = if session.persistence_error().is_some() {
            (
                "Persistence degraded".to_string(),
                StatusTone::Warning,
                Some(ordinary_primary),
            )
        } else if workflow.fresh_retry_version().is_some() {
            (
                "Fresh Plan handoff pending".to_string(),
                StatusTone::Warning,
                Some(ordinary_primary),
            )
        } else {
            (ordinary_primary, StatusTone::Normal, None)
        };
        let controls = pane.title().map(|_| {
            if !pane.can_compose() {
                format!("inspect only · {generated_controls}")
            } else {
                generated_controls.clone()
            }
        });
        let compact_controls = controls.clone();
        let telemetry_role = if pane.is_root() {
            Some(role)
        } else {
            pane.model_role_override()
        };
        let (profile, context, response) = telemetry_role.map_or((None, None, None), |role| {
            let telemetry = session.telemetry(role);
            (
                telemetry
                    .authoritative_profile()
                    .or_else(|| model_profiles.get(role))
                    .cloned(),
                telemetry.context().cloned(),
                telemetry.response().map(|response| response.usage()),
            )
        });
        let status = StatusBarView {
            primary,
            accent,
            tone,
            detail,
            profile,
            reasoning: telemetry_role.and_then(|role| session.reasoning(role)),
            context,
            response,
            external_context: pane.external_context(),
            controls,
            compact_controls,
        };
        RenderParts {
            history: conversation.history(),
            turn_starts: conversation.turn_starts(diagnostics_visible),
            folds,
            selected,
            selection_reveal: interaction.selection_reveal(),
            diagnostics_visible,
            appearance: pane.appearance(),
            input: composer.text(),
            input_cursor: composer.cursor(),
            image_ranges: composer.image_ranges(),
            paste_pending: composer.is_paste_pending(),
            menu: composer.menu(),
            completion: composer.completion_view(),
            command_menu_active,
            composer_locked,
            #[cfg(test)]
            editor_active,
            draft_recovery,
            mode,
            retained_plan,
            chrome,
            plan_dialog,
            tail,
            network_status: session.network_message(),
            assistant_header,
            header_timings: HeaderTimings::observe(conversation, session.observed_at()),
            pending_header,
            status,
            view,
        }
    }
}
