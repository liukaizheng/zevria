//! Submission, transcript edit, and Plan operation admission.
use super::*;
use zevria_session_api::TurnCommand;

impl App {
    pub(super) fn submit(&mut self) -> Option<UiAction> {
        if self.pane.is_worker() {
            return self.submit_worker();
        }
        if !self.pane.is_root() || self.composer.is_paste_pending() {
            return None;
        }
        if self.composer.is_blank() {
            return self.send_queued_now();
        }
        // Frontend-only control is admitted before busy/queue gates.
        if matches!(
            self.composer.classify(),
            Ok(ClassifiedInput::Builtin(SlashCommand::Jobs))
        ) {
            return self.submit_nonqueued(ClassifiedInput::Builtin(SlashCommand::Jobs));
        }
        let queueable = matches!(
            self.capabilities().queue_work,
            Ok(()) | Err(crate::input::DisabledReason::QueueFull)
        );
        if self.model.session.is_busy() && !queueable {
            return None;
        }
        let classified = match self.composer.classify() {
            Ok(classified) => classified,
            Err(error) => {
                self.push_error(error.to_string());
                return None;
            }
        };
        if queueable {
            let work = match classified {
                ClassifiedInput::Message(text) => QueuedWork::Message {
                    text,
                    behavior: zevria_foundation::RequestBehavior::Standard,
                },
                ClassifiedInput::Orchestrate(text) => QueuedWork::Message {
                    text,
                    behavior: zevria_foundation::RequestBehavior::Orchestrate,
                },
                ClassifiedInput::Skill { name, args } => QueuedWork::Skill { name, args },
                other => return self.submit_nonqueued(other),
            };
            return self.enqueue_composer(work);
        }
        self.submit_nonqueued(classified)
    }

    fn submit_nonqueued(&mut self, classified: ClassifiedInput) -> Option<UiAction> {
        if self.model.session.is_busy()
            && !matches!(classified, ClassifiedInput::Builtin(SlashCommand::Jobs))
        {
            return None;
        }
        if !matches!(classified, ClassifiedInput::Builtin(_)) && !self.can_submit_work() {
            return None;
        }
        if self.edit.is_recalling() {
            let replacement = match classified {
                ClassifiedInput::Message(text) => TranscriptEditReplacement::Message {
                    text,
                    mode: self.model.session.next_mode(),
                    behavior: zevria_foundation::RequestBehavior::Standard,
                },
                ClassifiedInput::Orchestrate(text) => TranscriptEditReplacement::Message {
                    text,
                    mode: self.model.session.next_mode(),
                    behavior: zevria_foundation::RequestBehavior::Orchestrate,
                },
                ClassifiedInput::Skill { name, args } => TranscriptEditReplacement::Skill {
                    name,
                    args,
                    mode: self.model.session.next_mode(),
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
                let TurnCommand::Submit {
                    text,
                    mode,
                    behavior,
                } = self.model.submit(text, behavior)?
                else {
                    unreachable!("submit admission")
                };
                self.after_admission();
                self.stage_draft();
                self.queue.resume();
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
                let TurnCommand::Compact { mode } = self.model.compact()? else {
                    unreachable!("compaction admission")
                };
                self.after_admission();
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
                let TurnCommand::RunEnsemble { workflow, prompt } =
                    self.model.run_ensemble(workflow, prompt)?
                else {
                    unreachable!("ensemble admission")
                };
                self.after_admission();
                self.stage_draft();
                self.queue.resume();
                Some(UiAction::RunEnsemble { workflow, prompt })
            }
            ClassifiedInput::Builtin(command) => {
                if self.queue.len() > 0
                    && matches!(command, SlashCommand::New | SlashCommand::Resume)
                {
                    self.push_error(
                        "Clear or send queued follow-ups before replacing the session.".into(),
                    );
                    return None;
                }
                self.composer.clear();
                Some(UiAction::RunCommand(command))
            }
            ClassifiedInput::Skill { name, args } => {
                let TurnCommand::InvokeSkill { name, args, mode } =
                    self.model.invoke_skill(name, args)?
                else {
                    unreachable!("skill admission")
                };
                self.after_admission();
                self.stage_draft();
                self.queue.resume();
                Some(UiAction::InvokeSkill { name, args, mode })
            }
        }
    }

    fn enqueue_composer(&mut self, work: QueuedWork) -> Option<UiAction> {
        let item = QueuedFollowUp::new(
            self.composer.snapshot(),
            work,
            self.model.session.next_mode(),
        );
        if self.queue.push(item).is_err() {
            self.push_error(format!("Follow-up queue is full (maximum {MAX_QUEUED_FOLLOW_UPS}). Edit or clear queued items first."));
            return None;
        }
        self.composer.clear();
        self.close_composer_edit_group();
        self.interaction.enter_normal();
        self.view.jump_bottom();
        self.view.invalidate_rendered_geometry();
        None
    }

    fn queue_admits_dispatch(&self) -> bool {
        self.pane.is_root()
            && self.queue.len() > 0
            && !self.model.session.is_busy()
            && !matches!(
                self.model.workflow.snapshot(),
                zevria_workflow::PlanWorkflowState::Ready { .. }
            )
            && self.model.workflow.dialog().is_none()
            && self.edit.is_none()
            && !self.drafts.has_pending()
    }

    /// Automatic dispatch waits until every event from the preceding work is reduced.
    pub(crate) fn dispatch_next_queued(&mut self) -> Option<UiAction> {
        if !self.queue_admits_dispatch()
            || self.queue.is_paused()
            || !self.model.session.is_quiescent()
        {
            return None;
        }
        if self.queue.front()?.mode != self.model.session.next_mode() {
            self.queue.pause(QueuePause::ModeChanged);
            self.view.invalidate_rendered_geometry();
            return None;
        }
        self.dispatch_queue_head()
    }

    fn dispatch_queue_head(&mut self) -> Option<UiAction> {
        let item = self.queue.pop_front()?;
        let command = match item.work.clone() {
            QueuedWork::Message { text, behavior } => {
                self.model.submit_in_mode(text, behavior, item.mode)
            }
            QueuedWork::Skill { name, args } => {
                self.model.invoke_skill_in_mode(name, args, item.mode)
            }
        };
        let Some(command) = command else {
            self.queue.push_front(item);
            return None;
        };
        self.drafts
            .stage_detached(item.draft, self.composer.generation());
        self.view.invalidate_rendered_geometry();
        Some(match command {
            TurnCommand::Submit {
                text,
                mode,
                behavior,
            } => UiAction::Submit {
                text,
                mode,
                behavior,
            },
            TurnCommand::InvokeSkill { name, args, mode } => {
                UiAction::InvokeSkill { name, args, mode }
            }
            _ => unreachable!("queued admission only submits prompts and skills"),
        })
    }

    pub(super) fn can_send_queued(&self) -> bool {
        self.composer.is_blank() && self.queue_admits_dispatch()
    }

    pub(super) fn send_queued_now(&mut self) -> Option<UiAction> {
        if !self.can_send_queued() {
            return None;
        }
        self.queue.resume();
        let action = self.dispatch_queue_head()?;
        self.interaction.enter_normal();
        self.interaction.clear_chords();
        self.view.jump_bottom();
        Some(action)
    }

    pub(super) fn can_edit_queued(&self) -> bool {
        self.pane.is_root()
            && self.queue.len() > 0
            && self.can_edit_draft()
            && self.composer.is_empty()
            && self.edit.is_none()
    }

    pub(super) fn edit_newest_queued(&mut self) {
        if !self.can_edit_queued() {
            return;
        }
        let item = self.queue.pop_back().expect("nonempty queue");
        self.composer.restore(item.draft);
        self.interaction.enter_insert();
        self.interaction.clear_chords();
        self.view.set_composer_scroll(0);
        self.view.invalidate_rendered_geometry();
    }

    pub(super) fn can_clear_queue(&self) -> bool {
        let owner = self.surface().id.kind;
        self.pane.is_root()
            && !self.model.session.is_busy()
            && self.queue.len() > 0
            && matches!(
                owner,
                SurfaceKind::Transcript
                    | SurfaceKind::Selection
                    | SurfaceKind::Composer
                    | SurfaceKind::Completion
            )
            && !(matches!(owner, SurfaceKind::Composer | SurfaceKind::Completion)
                && !self.composer.is_empty())
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
        let (target, target_index) = self.edit.recalled_target()?;
        let TurnCommand::EditTranscript(edit) = self.model.edit_transcript(
            TranscriptEdit {
                target,
                replacement,
            },
            target_index,
        )?
        else {
            unreachable!("edit admission")
        };
        self.after_admission();
        self.edit
            .stage_submission()
            .expect("validated recall must stage exactly once");
        self.stage_draft();
        Some(UiAction::EditTranscript(edit))
    }

    pub(super) fn implement_submitted_plan(&mut self, decision: PlanDecision) -> Option<UiAction> {
        let Some(expected) = self
            .model
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
        if decision == PlanDecision::ImplementFresh && self.queue.len() > 0 {
            self.push_error(
                "Clear or send queued follow-ups before implementing in a fresh session.".into(),
            );
            return None;
        }
        let TurnCommand::ResolvePlan { expected, decision } =
            self.model.resolve_plan(expected, decision)?
        else {
            unreachable!("plan admission")
        };
        self.after_admission();
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
        if self.model.apply_plan_snapshot(state) {
            self.composer.close_edit_group();
            self.interaction.clear_selection();
            self.interaction.clear_chords();
        }
    }

    pub(super) fn leave_insert_if_composer_locked(&mut self) {
        if !self.composer_editable() {
            self.composer.close_edit_group();
        }
        if !self.pane.can_compose()
            && self.model.session.operation_kind() != Some(OperationKind::ModeManagement)
            && self.interaction.is_insert()
        {
            self.interaction.enter_normal();
            self.interaction.clear_chords();
        }
    }

    fn after_admission(&mut self) {
        self.view.jump_bottom();
        self.interaction.clear_chords();
        self.leave_insert_if_composer_locked();
    }
}
