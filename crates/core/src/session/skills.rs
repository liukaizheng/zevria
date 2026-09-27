//! Skills orchestration.

use super::*;

/// The one engine-owned admission path for direct and tool applications.
/// The caller supplies a captured view and the same directive preparation used
/// for dispatch. Only direct historical edits may supply a recorded snapshot.
pub(super) struct SkillApplicationPreparation<'a> {
    pub context: &'a zevria_instructions::skill::SkillContext,
    pub instructions: &'a super::directives::InstructionPreparation,
    pub state: &'a zevria_instructions::DirectiveState,
    pub fixed_tokens: u64,
    pub input_token_limit: u64,
}

pub(super) struct PreparedSkillApplication {
    pub application: SkillApplication,
    pub prospective: ActiveSkills,
    pub updates: super::directives::PreparedInstructionUpdates,
}

impl SkillApplicationPreparation<'_> {
    pub fn prepare(
        &self,
        name: &SkillName,
        origin: zevria_instructions::skill::SkillInvocationOrigin,
        historical: Option<zevria_instructions::skill::SkillSnapshot>,
    ) -> anyhow::Result<PreparedSkillApplication> {
        self.context.ensure_enabled(name)?;
        let snapshot = if !self.context.pins.contains(name) {
            match historical {
                Some(snapshot) => {
                    anyhow::ensure!(
                        origin == zevria_instructions::skill::SkillInvocationOrigin::Explicit
                            && snapshot.name() == name,
                        "invalid historical edit snapshot"
                    );
                    snapshot
                }
                None => self.context.resolve(name, origin)?,
            }
        } else {
            self.context.resolve(name, origin)?
        };
        let (application, prospective) = self.context.pins.prepare(snapshot)?;
        let updates = self
            .instructions
            .prepare_updates(self.state, &prospective)?;
        let overhead = self.fixed_tokens.saturating_add(updates.instruction_tokens);
        anyhow::ensure!(
            overhead < self.input_token_limit,
            "applying skill {name} would require {overhead} irreducible instruction-state tokens, exhausting the configured {}-token input limit before conversation history or the invocation can be included",
            self.input_token_limit
        );
        Ok(PreparedSkillApplication {
            application,
            prospective,
            updates,
        })
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub fn with_skill_catalog(
        mut self,
        catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    ) -> Result<Self, SessionReplayError> {
        self.ensure_replay_valid()?;
        self.install_skill_catalog(catalog)?;
        Ok(self)
    }

    fn install_skill_catalog(
        &mut self,
        catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    ) -> Result<(), SessionReplayError> {
        self.skills.catalog = catalog;
        self.refresh_skill_policies()?;
        self.invalidate_mode_accounting();
        self.publish_skill_query_context()?;
        Ok(())
    }

    /// Mode permission is captured independently of initial catalog availability.
    pub fn with_skill_management(
        mut self,
        service: Arc<dyn zevria_instructions::skill::SkillManagementService>,
        mode_permissions: [bool; SessionMode::COUNT],
    ) -> Result<Self, SessionReplayError> {
        self.ensure_replay_valid()?;
        self.skills.management = Some(service);
        self.skills.mode_permissions = mode_permissions;
        self.refresh_skill_policies()?;
        self.invalidate_mode_accounting();
        Ok(self)
    }

    pub(super) fn refresh_skill_policies(&mut self) -> Result<(), SessionReplayError> {
        for mode in SessionMode::ALL {
            self.policies.policy_mut(mode).skills_enabled =
                self.skills.mode_permissions[mode.index()];
        }
        Ok(())
    }

    pub(super) async fn manage_skills(
        &mut self,
        request_id: String,
        request: zevria_instructions::skill::SkillManagementRequest,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        use zevria_instructions::skill::SkillManagementResult as Result;
        if request.is_mutation() {
            self.invalidate_model_preview();
        }
        let result = if !request.is_mutation() {
            Self::query_skills(&self.management_skill_context()?, &request)
        } else if self.conversation.is_read_only() {
            Result::error(
                "read_only",
                "skill mutations are unavailable in a read-only session",
            )
        } else if request.expected_revision() != Some(self.skills.catalog.revision()) {
            Result::error(
                "stale_revision",
                "skill catalog changed; refresh before mutating",
            )
        } else if let Some(service) = &self.skills.management {
            match service.update(request, self.skills.catalog.clone()).await {
                Err(error) => Result::error("update_failed", format!("{error:#}")),
                Ok(catalog) => {
                    let unchanged = catalog.revision() == self.skills.catalog.revision();
                    if !unchanged {
                        self.install_skill_catalog(catalog)?;
                        let revocations = self
                            .directive_state()?
                            .snapshot()
                            .directives
                            .into_iter()
                            .filter_map(|directive| {
                                let zevria_instructions::DirectivePayload::SkillBody {
                                    name, ..
                                } = directive.payload
                                else {
                                    return None;
                                };
                                (!self.skills.catalog.config().name_enabled(&name)).then(|| {
                                    TranscriptItem::Directive(
                                        zevria_instructions::DirectiveContent::new(
                                            zevria_instructions::DirectivePayload::SkillRevocation {
                                                name,
                                                reason: "disabled by management".into(),
                                            },
                                        )
                                        .expect("valid revocation"),
                                    )
                                })
                            })
                            .collect::<Vec<_>>();
                        if let Err(error) = self.record_required_items(revocations) {
                            self.persistence_failed(&error, events).await?;
                            let _ = events.send(SessionEvent::SkillsResult { request_id, result: Result::error("persistence_failed", format!("management changed but revocation persistence failed; generation requires reconciliation: {error:#}")) }).await;
                            return Ok(());
                        }
                    }
                    let counts = self.management_skill_context()?.management_counts();
                    let revision = self.skills.catalog.revision().to_string();
                    if !unchanged {
                        let _ = events
                            .send(SessionEvent::SkillsChanged {
                                revision: revision.clone(),
                                counts: counts.clone(),
                            })
                            .await;
                    }
                    Result::Changed {
                        revision,
                        counts,
                        unchanged,
                    }
                }
            }
        } else {
            Result::error(
                "unavailable",
                "skill management is unavailable for this session",
            )
        };
        let _ = events
            .send(SessionEvent::SkillsResult { request_id, result })
            .await;
        Ok(())
    }

    pub(super) fn query_skills(
        context: &zevria_instructions::skill::SkillContext,
        request: &zevria_instructions::skill::SkillManagementRequest,
    ) -> zevria_instructions::skill::SkillManagementResult {
        match context.management_view(request) {
            Ok(view) => zevria_instructions::skill::SkillManagementResult::View { view },
            Err(error) => zevria_instructions::skill::SkillManagementResult::error(
                "query_failed",
                format!("{error:#}"),
            ),
        }
    }

    pub(super) fn management_skill_context(
        &self,
    ) -> Result<zevria_instructions::skill::SkillContext, SessionReplayError> {
        let policy = self.policies.policy(self.selected_mode());
        Ok(self.captured_skill_context(
            self.active_skills()?,
            self.skill_activation_available(policy),
        ))
    }

    pub(super) fn captured_skill_context(
        &self,
        active: &ActiveSkills,
        enabled: bool,
    ) -> zevria_instructions::skill::SkillContext {
        zevria_instructions::skill::SkillContext {
            catalog: self.skills.catalog.clone(),
            pins: active.clone(),
            mode_enabled: enabled,
        }
    }
}
