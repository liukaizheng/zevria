//! Submission, transcript edit, and Plan operation admission.
use super::*;

impl App {
    pub(super) fn submit(&mut self) -> Option<UiAction> {
        if self.pane.is_worker() {
            return self.submit_worker();
        }
        if self.session.is_busy()
            || !self.pane.is_root()
            || self.composer.is_blank()
            || self.composer.is_paste_pending()
        {
            return None;
        }
        let classified = match self.composer.classify() {
            Ok(classified) => classified,
            Err(error) => {
                self.push_error(error.to_string());
                return None;
            }
        };
        if !matches!(classified, ClassifiedInput::Builtin(_)) && !self.can_submit_work() {
            return None;
        }
        if self.edit.is_recalling() {
            let replacement = match classified {
                ClassifiedInput::Message(text) => TranscriptEditReplacement::Message {
                    text,
                    mode: self.session.next_mode(),
                    behavior: zevria_foundation::RequestBehavior::Standard,
                },
                ClassifiedInput::Orchestrate(text) => TranscriptEditReplacement::Message {
                    text,
                    mode: self.session.next_mode(),
                    behavior: zevria_foundation::RequestBehavior::Orchestrate,
                },
                ClassifiedInput::Skill { name, args } => TranscriptEditReplacement::Skill {
                    name,
                    args,
                    mode: self.session.next_mode(),
                },
                ClassifiedInput::Ensemble { workflow, prompt } => {
                    TranscriptEditReplacement::Ensemble { workflow, prompt }
                }
                ClassifiedInput::Builtin(_) => {
                    self.push_error(
                        "Built-in commands cannot replace transcript items; cancel the recall to run the command."
                            .to_string(),
                    );
                    return None;
                }
            };
            return self.begin_transcript_edit(replacement);
        }
        self.submit_classified_fresh(classified)
    }

    pub(super) fn submit_classified_fresh(
        &mut self,
        classified: ClassifiedInput,
    ) -> Option<UiAction> {
        let behavior = if matches!(&classified, ClassifiedInput::Orchestrate(_)) {
            zevria_foundation::RequestBehavior::Orchestrate
        } else {
            zevria_foundation::RequestBehavior::Standard
        };
        match classified {
            ClassifiedInput::Message(text) | ClassifiedInput::Orchestrate(text) => {
                let mode = self.session.next_mode();
                if !self.begin_operation(OperationKind::Submit, mode, role_for_mode(mode)) {
                    return None;
                }
                self.stage_draft();
                Some(UiAction::Submit {
                    text,
                    mode,
                    behavior,
                })
            }
            ClassifiedInput::Builtin(SlashCommand::Build) => {
                self.begin_mode_selection(SessionMode::Build, true)
            }
            ClassifiedInput::Builtin(SlashCommand::Plan) => {
                self.begin_mode_selection(SessionMode::Plan, true)
            }
            ClassifiedInput::Builtin(SlashCommand::Compact) => {
                let mode = self.session.next_mode();
                if !self.begin_operation(OperationKind::ManualCompaction, mode, role_for_mode(mode))
                {
                    return None;
                }
                self.composer.clear();
                Some(UiAction::Compact { mode })
            }
            ClassifiedInput::Builtin(SlashCommand::Implement) => {
                self.implement_submitted_plan(PlanDecision::ImplementCurrent)
            }
            ClassifiedInput::Builtin(SlashCommand::ImplementFresh) => {
                self.implement_submitted_plan(PlanDecision::ImplementFresh)
            }
            ClassifiedInput::Ensemble { workflow, prompt } => {
                let mode = ensemble_mode(workflow);
                if !self.begin_operation(OperationKind::Ensemble, mode, ensemble_role(workflow)) {
                    return None;
                }
                self.stage_draft();
                Some(UiAction::RunEnsemble { workflow, prompt })
            }
            ClassifiedInput::Builtin(command) => {
                self.composer.clear();
                Some(UiAction::RunCommand(command))
            }
            ClassifiedInput::Skill { name, args } => {
                let mode = self.session.next_mode();
                if !self.begin_operation(OperationKind::Skill, mode, role_for_mode(mode)) {
                    return None;
                }
                self.stage_draft();
                Some(UiAction::InvokeSkill { name, args, mode })
            }
        }
    }

    pub(super) fn stage_draft(&mut self) {
        self.drafts.stage(&mut self.composer);
        self.close_composer_edit_group();
        self.interaction.enter_normal();
    }

    pub(super) fn restore_rejected_draft(&mut self) {
        self.drafts.reject(&mut self.composer);
    }

    pub(crate) fn clipboard_completed(
        &mut self,
        generation: u64,
        cursor: usize,
        result: crate::clipboard::ClipboardResult,
    ) {
        if self.composer.cursor() != cursor || !self.composer.finish_paste(generation) {
            return;
        }
        self.view.invalidate_rendered_geometry();
        match result {
            crate::clipboard::ClipboardResult::Image(image) => {
                if let Err(error) = self.composer.attach_image(image) {
                    self.push_error(error.to_string());
                }
            }
            crate::clipboard::ClipboardResult::Text(text) => self.composer.insert_text(&text),
            crate::clipboard::ClipboardResult::Empty => {
                self.push_error("Clipboard has no usable image or text.".into())
            }
            crate::clipboard::ClipboardResult::Error(error) => self.push_error(error),
        }
    }

    pub(super) fn begin_transcript_edit(
        &mut self,
        replacement: TranscriptEditReplacement,
    ) -> Option<UiAction> {
        let mode = replacement.mode();
        let role = replacement_role(&replacement);
        if !self.begin_operation(OperationKind::TranscriptEdit, mode, role) {
            return None;
        }
        let target = self
            .edit
            .stage_submission()
            .expect("validated recall must stage exactly once");
        self.stage_draft();
        Some(UiAction::EditTranscript(TranscriptEdit {
            target,
            replacement,
        }))
    }

    pub(super) fn implement_submitted_plan(&mut self, decision: PlanDecision) -> Option<UiAction> {
        let Some(expected) = self
            .workflow
            .submitted_artifact()
            .map(|artifact| artifact.version)
        else {
            self.push_error("No submitted plan is available to implement.".to_string());
            return None;
        };
        let action = self.begin_plan_decision(expected, decision)?;
        self.composer.clear();
        Some(action)
    }

    pub(super) fn begin_plan_decision(
        &mut self,
        expected: PlanVersion,
        decision: PlanDecision,
    ) -> Option<UiAction> {
        if !self.pane.is_root() {
            return None;
        }
        let kind = OperationKind::plan_decision(expected, decision);
        let role = match decision {
            PlanDecision::Revise => ModelRole::Plan,
            PlanDecision::ImplementCurrent | PlanDecision::ImplementFresh => ModelRole::Build,
        };
        if !self.begin_operation(kind, SessionMode::Build, role) {
            return None;
        }
        if matches!(
            decision,
            PlanDecision::ImplementCurrent | PlanDecision::ImplementFresh
        ) {
            self.composer.clear();
        }
        self.interaction.clear_selection();
        self.interaction.clear_chords();
        Some(UiAction::ResolvePlan { expected, decision })
    }

    pub(super) fn apply_plan_snapshot(&mut self, state: zevria_workflow::PlanWorkflowState) {
        if let zevria_workflow::PlanWorkflowState::Ready { artifact }
        | zevria_workflow::PlanWorkflowState::Published { artifact } = &state
        {
            self.conversation.push_plan_artifact(artifact.clone());
        }
        let ready = matches!(state, zevria_workflow::PlanWorkflowState::Ready { .. });
        let changed = self.workflow.apply_snapshot(state);
        self.session.settle_plan_state(self.workflow.snapshot());
        if ready && changed {
            self.composer.close_edit_group();
            self.session.set_next_mode(SessionMode::Plan);
            self.interaction.clear_selection();
            self.interaction.clear_chords();
        }
    }

    pub(super) fn leave_insert_if_composer_locked(&mut self) {
        if !self.composer_editable() {
            self.composer.close_edit_group();
        }
        if !self.pane.can_compose()
            && self.session.operation_kind() != Some(OperationKind::ModeManagement)
            && self.interaction.is_insert()
        {
            self.interaction.enter_normal();
            self.interaction.clear_chords();
        }
    }

    pub(super) fn begin_operation(
        &mut self,
        kind: OperationKind,
        mode: SessionMode,
        role: ModelRole,
    ) -> bool {
        if !self.pane.is_root() || !self.session.begin_operation(kind, mode, role) {
            return false;
        }
        self.view.jump_bottom();
        self.interaction.clear_chords();
        self.leave_insert_if_composer_locked();
        true
    }
}
