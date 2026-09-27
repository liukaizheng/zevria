//! Offline scratch sessions exercise the real engine and JSONL writer across reopen.
use super::*;
use zevria_instructions::DirectivePayload;

struct Provider {
    normal: ScriptedProvider,
    native: bool,
    application: &'static str,
    maintenance: Vec<CapturedRequest>,
}
impl Provider {
    fn new(native: bool, application: &'static str) -> Self {
        Self {
            normal: ScriptedProvider::new(
                (0..12).map(|_| Ok(Message::assistant("completed summary or reply"))),
            ),
            native,
            application,
            maintenance: Vec::new(),
        }
    }
}
impl ModelProvider for Provider {
    fn application_prompt(&self) -> &str {
        self.application
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        self.normal.complete(request, progress)
    }
    fn compact<'a>(&'a mut self, request: ModelRequest<'a>) -> CompactFuture<'a> {
        Box::pin(async move {
            zevria_model::maintenance::validate_maintenance_input(&request.input)?;
            assert_eq!(request.allowed_tool_names, Some([].as_slice()));
            self.maintenance.push(CapturedRequest::of(&request));
            if self.native {
                Ok(CompactResult::Replacement(vec![
                    OwnedModelRequestItem::message(Message::user("native summary")),
                ]))
            } else {
                Ok(CompactResult::Unsupported)
            }
        })
    }
    fn reset(&mut self) {
        self.normal.reset();
    }
}

fn roots(directory: &std::path::Path, version: &str) -> zevria_instructions::GuidanceRoots {
    let global = directory.join("global");
    let project = directory.join("project");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        global.join("AGENTS.md"),
        format!("{version}_GLOBAL_SENTINEL"),
    )
    .unwrap();
    std::fs::write(
        project.join("AGENTS.md"),
        format!("{version}_PROJECT_SENTINEL"),
    )
    .unwrap();
    zevria_instructions::GuidanceRoots::fixture(Some(&global), &project)
}

async fn run(engine: &mut SessionEngine<Provider>, command: SessionCommand) {
    let (events, mut receiver) = session_event_channel(256);
    engine.handle_command(command, &events).await.unwrap();
    let emitted = collect_events(&mut receiver).await;
    assert!(
        !emitted.iter().any(|event| matches!(
            event,
            SessionEvent::TurnRejected { .. }
                | SessionEvent::TurnFailed { .. }
                | SessionEvent::PersistenceChanged { error: Some(_), .. }
        )),
        "{emitted:?}"
    );
}
fn submit(text: &str) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::Submit {
        behavior: zevria_foundation::RequestBehavior::Standard,
        text: text.into(),
        mode: SessionMode::Build,
    })
}
fn compact() -> SessionCommand {
    SessionCommand::Turn(TurnCommand::Compact {
        mode: SessionMode::Build,
    })
}

fn inspect(engine: &SessionEngine<Provider>, pinned: bool) {
    let path = engine.conversation.path();
    let persisted = zevria_transcript::transcript::load(path).unwrap();
    assert_eq!(persisted, engine.conversation.items());
    SessionReplayError::validate(&persisted).unwrap();
    let bytes = std::fs::read_to_string(path).unwrap();
    for sentinel in [
        "APPLICATION_SENTINEL",
        "GLOBAL_SENTINEL",
        "PROJECT_SENTINEL",
    ] {
        assert!(!bytes.contains(sentinel));
    }
    for line in bytes.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value.get("zevria_instruction_prefix").is_none());
        assert!(value.get("zevria_directive").is_none());
        if let Some(checkpoint) = value.get("zevria_compaction") {
            assert_eq!(checkpoint["version"], 1);
            assert!(checkpoint.get("instruction_snapshot").is_none());
        }
    }
    let pins = replay_active_skills(&persisted).unwrap();
    assert_eq!(pins.len(), usize::from(pinned));
    if pinned {
        let expected = test_skills().get("review").unwrap().snapshot();
        assert_eq!(pins.get(expected.name()), Some(&expected));
        assert!(bytes.contains("Review instructions"));
    }
}
fn assert_current_maintenance(request: &CapturedRequest) {
    let borrowed = request
        .input
        .iter()
        .map(OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    zevria_model::maintenance::validate_maintenance_input(&borrowed).unwrap();
    let text = &request.instructions;
    assert!(text.ends_with("## Eligible skills\nSkill selection is unavailable."));
    for expected in [
        "CURRENT_APPLICATION_SENTINEL",
        "CURRENT_GLOBAL_SENTINEL",
        "CURRENT_PROJECT_SENTINEL",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    for excluded in [
        "OLD_APPLICATION_SENTINEL",
        "OLD_GLOBAL_SENTINEL",
        "OLD_PROJECT_SENTINEL",
        "Review instructions",
        "Build test instructions",
    ] {
        assert!(!text.contains(excluded), "unexpected {excluded}");
    }
}

#[tokio::test]
async fn scratch_prompt_skill_compact_quit_resume_and_submit_retains_pins_and_directives() {
    for native in [false, true] {
        let (directory, writer) = test_transcript();
        let old_roots = roots(directory.path(), "OLD");
        let mut engine = SessionEngine::new(
            Provider::new(native, "OLD_APPLICATION_SENTINEL"),
            test_skill_tools(),
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap()
        .with_guidance_roots(old_roots);
        assert!(
            std::fs::read(engine.conversation.path())
                .unwrap()
                .is_empty(),
            "instruction sets never create a record"
        );
        run(&mut engine, submit("first prompt")).await;
        inspect(&engine, false);
        run(
            &mut engine,
            SessionCommand::Turn(TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: "inspect".into(),
                mode: SessionMode::Build,
            }),
        )
        .await;
        inspect(&engine, true);
        run(&mut engine, compact()).await;
        inspect(&engine, true);
        let path = engine.conversation.path().to_path_buf();
        drop(engine);
        let current_roots = roots(directory.path(), "CURRENT");
        let mut resumed = SessionEngine::new(
            Provider::new(native, "CURRENT_APPLICATION_SENTINEL"),
            test_skill_tools(),
            test_policies(),
            TranscriptWriter::append_to(path.clone()).unwrap(),
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_guidance_roots(current_roots);
        assert!(resumed.refresh_application_guidance().unwrap().is_empty());
        assert_eq!(
            resumed.directive_state().unwrap().snapshot().directives,
            vec![zevria_instructions::DirectiveContent::skill(
                &test_skills().get("review").unwrap().snapshot(),
            )]
        );
        assert_eq!(resumed.active_skills().unwrap().len(), 1);
        // Manual compaction uses current guidance and excludes restored directives.
        run(&mut resumed, compact()).await;
        assert_current_maintenance(resumed.provider.maintenance.last().unwrap());
        if !native {
            assert_current_maintenance(
                resumed
                    .provider
                    .normal
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap(),
            );
        }
        assert_eq!(
            resumed.directive_state().unwrap().snapshot().directives,
            vec![zevria_instructions::DirectiveContent::skill(
                &test_skills().get("review").unwrap().snapshot(),
            )]
        );
        inspect(&resumed, true);
        run(&mut resumed, submit("after resume")).await;
        let text = resumed.rendered_instructions(resumed.policies.policy(SessionMode::Build));
        assert!(
            format!("{:?}", resumed.directive_state().unwrap().snapshot())
                .contains("Review instructions")
        );
        for expected in [
            "CURRENT_APPLICATION_SENTINEL",
            "CURRENT_GLOBAL_SENTINEL",
            "CURRENT_PROJECT_SENTINEL",
            "Build test instructions",
            "## Eligible skills",
        ] {
            assert!(text.contains(expected));
        }
        let first = resumed
            .model_input()
            .into_iter()
            .map(ModelRequestItem::to_owned_item)
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        let directive_count = resumed
            .conversation
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Directive(_)))
            .count();
        run(&mut resumed, submit("unchanged next turn")).await;
        assert_eq!(
            resumed
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            directive_count
        );
        assert!(
            snapshot_model_input(resumed.model_input())
                .unwrap()
                .starts_with(&first)
        );
        inspect(&resumed, true);
        let legacy = directory.path().join("legacy.jsonl");
        let bytes = b"{\"zevria_instruction_prefix\":{\"text\":\"PRIVATE_LEGACY_BODY\"}}\n{\"zevria_directive\":{\"text\":\"PRIVATE_LEGACY_BODY\"}}\n";
        std::fs::write(&legacy, bytes).unwrap();
        let error = TranscriptWriter::append_to(legacy.clone())
            .err()
            .expect("unsupported history");
        assert!(error.to_string().contains("unsupported history"));
        assert!(!error.to_string().contains("PRIVATE_LEGACY_BODY"));
        assert_eq!(std::fs::read(legacy).unwrap(), bytes);
    }
}

#[tokio::test]
async fn resumed_session_reuses_the_pre_shutdown_prefix_after_tool_activation() {
    for native in [false, true] {
        let (directory, writer) = test_transcript();
        let guidance = roots(directory.path(), "CURRENT");
        let mut provider = Provider::new(native, "CURRENT_APPLICATION_SENTINEL");
        provider.normal = ScriptedProvider::new([
            Ok(Message::Assistant {
                id: None,
                content: vec![named_tool_call(
                    "activate-review",
                    "skill",
                    json!({"skill":"review"}),
                )],
            }),
            Ok(Message::assistant("review activated")),
            Ok(Message::assistant("second reply")),
        ]);
        let mut engine = SessionEngine::new(
            provider,
            test_skill_tools(),
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap()
        .with_guidance_roots(guidance.clone());
        run(&mut engine, submit("review this change")).await;
        run(&mut engine, submit("continue reviewing")).await;
        inspect(&engine, true);
        let before = engine
            .provider
            .normal
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let items = engine.conversation.items().to_vec();
        let directive_index = items
            .iter()
            .position(|item| matches!(item, TranscriptItem::Directive(_)))
            .unwrap();
        assert!(matches!(
            &items[directive_index - 1],
            TranscriptItem::ToolResults { skill_applications, .. } if skill_applications.len() == 1
        ));
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            1
        );
        let path = engine.conversation.path().to_path_buf();
        drop(engine);

        let mut resumed = SessionEngine::new(
            Provider::new(native, "CURRENT_APPLICATION_SENTINEL"),
            test_skill_tools(),
            test_policies(),
            TranscriptWriter::append_to(path.clone()).unwrap(),
            test_skills(),
        )
        .unwrap()
        .with_guidance_roots(guidance.clone());
        let review = zevria_instructions::DirectiveContent::skill(
            &test_skills().get("review").unwrap().snapshot(),
        );
        assert_eq!(
            resumed.directive_state().unwrap().snapshot().directives,
            vec![review]
        );
        assert_eq!(zevria_transcript::transcript::load(&path).unwrap(), items);
        assert_eq!(resumed.conversation.items(), items);
        run(&mut resumed, submit("ok")).await;
        {
            let requests = resumed.provider.normal.requests.lock().unwrap();
            let after = requests.last().unwrap();
            assert!(after.input.starts_with(&before.input));
            assert_eq!(after.instructions, before.instructions);
            assert_eq!(after.allowed_tool_names, before.allowed_tool_names);
        }
        assert_eq!(
            resumed.conversation.items()[directive_index],
            items[directive_index]
        );
        assert_eq!(
            resumed
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            1
        );
        inspect(&resumed, true);

        // Plan disables skills. Persist its revocation, then resume into the same
        // scope so reconciliation must not move or repeat that directive either.
        let plan_prompt = |text: &str| {
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: text.into(),
                mode: SessionMode::Plan,
            })
        };
        run(&mut resumed, plan_prompt("plan without skills")).await;
        let before = resumed
            .provider
            .normal
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let items = resumed.conversation.items().to_vec();
        let revocation_index = items.iter().position(|item| matches!(item,
            TranscriptItem::Directive(d) if matches!(d.payload, DirectivePayload::SkillRevocation { .. })
        )).unwrap();
        assert!(revocation_index > directive_index);
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            2
        );
        assert!(
            resumed
                .directive_state()
                .unwrap()
                .snapshot()
                .directives
                .is_empty()
        );
        drop(resumed);

        let mut resumed = SessionEngine::new(
            Provider::new(native, "CURRENT_APPLICATION_SENTINEL"),
            test_skill_tools(),
            test_policies(),
            TranscriptWriter::append_to(path.clone()).unwrap(),
            test_skills(),
        )
        .unwrap()
        .with_guidance_roots(guidance);
        assert_eq!(zevria_transcript::transcript::load(&path).unwrap(), items);
        assert_eq!(resumed.conversation.items(), items);
        assert!(
            resumed
                .directive_state()
                .unwrap()
                .snapshot()
                .directives
                .is_empty()
        );
        assert_eq!(resumed.active_skills().unwrap().len(), 1);
        run(&mut resumed, plan_prompt("ok")).await;
        {
            let requests = resumed.provider.normal.requests.lock().unwrap();
            let after = requests.last().unwrap();
            assert!(after.input.starts_with(&before.input));
            assert_eq!(after.instructions, before.instructions);
            assert_eq!(after.allowed_tool_names, before.allowed_tool_names);
        }
        assert_eq!(&resumed.conversation.items()[..items.len()], items);
        assert_eq!(
            resumed.conversation.items()[revocation_index],
            items[revocation_index]
        );
        assert_eq!(
            resumed
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            2
        );
        inspect(&resumed, true);
    }
}

#[tokio::test]
async fn analysis_resume_and_compaction_keep_current_policy_and_scratch_as_history_data() {
    use zevria_instructions::prompts::{
        ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS, INSPECTION_POLICY_INSTRUCTIONS,
    };
    const OLD: &str = "Older generic Zevria boilerplate: Do not modify files or access outside the startup workspace.";
    const SCRATCH: &str = "/system-temp/zevria-inspection-fixture-only";
    let policies = |text: &str| {
        let policy = TurnPolicy::new(text, Some(vec!["command".into()]), ModelRole::Review, false)
            .with_contract(WorkspaceContract::SourceReadOnlyScratch);
        SessionPolicies::new(policy.clone(), policy)
    };
    for native in [false, true] {
        let (_directory, writer) = test_transcript();
        let path = writer.path().to_path_buf();
        let mut provider = Provider::new(native, "application");
        provider.normal = ScriptedProvider::new([
            Ok(command_call(
                "scratch-data",
                &format!("rtk echo SCRATCH={SCRATCH}"),
            )),
            Ok(Message::assistant(
                "Historical refusal: workspace-only inspection.",
            )),
        ]);
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new()
                .tool(CommandTestTool {
                    calls: Arc::new(Mutex::new(Vec::new())),
                })
                .run(),
            policies(OLD),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        run(&mut engine, submit(OLD)).await;
        let prefix = engine.rendered_instructions(engine.policies.policy(SessionMode::Build));
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains(SCRATCH));
        assert!(!persisted.contains("zevria_directive"));
        drop(engine);

        let mut resumed = SessionEngine::new(
            Provider::new(native, "application"),
            ToolServer::new().run(),
            policies(ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS),
            TranscriptWriter::append_to(path.clone()).unwrap(),
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        run(
            &mut resumed,
            submit("Genuine feedback: inspect under the current worker policy."),
        )
        .await;
        {
            let requests = resumed.provider.normal.requests.lock().unwrap();
            let request = requests.last().unwrap();
            assert_eq!(
                request
                    .instructions
                    .matches(INSPECTION_POLICY_INSTRUCTIONS)
                    .count(),
                1
            );
            assert!(
                request
                    .instructions
                    .starts_with(zevria_instructions::prompts::ENGINE_PROTOCOL_INSTRUCTIONS)
            );
            assert!(!request.instructions.contains(OLD));
            assert!(!request.instructions.contains(SCRATCH));
            let data = format!("{:?}", request.input);
            assert!(data.contains(OLD));
            assert!(data.contains(SCRATCH));
            assert_eq!(request.allowed_tool_names, Some(vec!["command".into()]));
        }
        run(&mut resumed, compact()).await;
        // Maintenance has a separate tool-free policy, not analysis permissions.
        let maintenance = format!("{:?}", resumed.provider.maintenance.last().unwrap());
        assert!(!maintenance.contains("## Inspection and scratch policy"));
        run(&mut resumed, submit("after compaction")).await;
        let state = resumed.rendered_instructions(resumed.policies.policy(SessionMode::Build));
        assert!(state.contains("source-read-only with temporary investigative execution"));
        assert!(!state.contains(OLD));
        assert!(!state.contains(SCRATCH));
        let count = resumed
            .conversation
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Directive(_)))
            .count();
        let input = snapshot_model_input(resumed.model_input()).unwrap();
        run(&mut resumed, submit("unchanged policy")).await;
        assert_eq!(
            resumed
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Directive(_)))
                .count(),
            count
        );
        assert!(
            snapshot_model_input(resumed.model_input())
                .unwrap()
                .starts_with(&input)
        );
        assert_eq!(
            resumed.rendered_instructions(resumed.policies.policy(SessionMode::Build)),
            state
        );
        assert_ne!(state, prefix);
        assert!(!prefix.contains(SCRATCH));
        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("zevria_directive"));
        assert!(!persisted.contains("## Inspection and scratch policy"));
    }
}

#[tokio::test]
async fn edited_prefix_compaction_before_first_resumed_turn_uses_current_maintenance_guidance() {
    for native in [false, true] {
        let (directory, mut writer) = test_transcript();
        let source = vec![
            TranscriptItem::Message(Message::user("historic data ".repeat(2500))),
            TranscriptItem::Message(Message::assistant("old answer")),
            TranscriptItem::Message(Message::user("replace this prompt")),
        ];
        writer.rewrite(&source).unwrap();
        let current_roots = roots(directory.path(), "CURRENT");
        let mut engine = SessionEngine::new(
            Provider::new(native, "CURRENT_APPLICATION_SENTINEL"),
            test_skill_tools(),
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap()
        .with_guidance_roots(current_roots)
        .with_compaction_policy(test_compaction_policy(100_000, 1, 100));
        assert!(
            engine
                .directive_state()
                .unwrap()
                .snapshot()
                .directives
                .is_empty()
        );
        run(
            &mut engine,
            prompt_message_edit(1, "edited prompt", SessionMode::Build),
        )
        .await;
        assert_eq!(engine.provider.maintenance.len(), 1);
        assert_current_maintenance(&engine.provider.maintenance[0]);
        if !native {
            assert_current_maintenance(&engine.provider.normal.requests.lock().unwrap()[0]);
        }
        let checkpoint = engine.conversation.latest_compaction().unwrap().0;
        assert_eq!(&engine.conversation.items()[..checkpoint], &source[..2]);
        assert!(matches!(
            engine.conversation.items()[checkpoint + 1],
            TranscriptItem::Message(_)
        ));
        assert!(
            engine
                .conversation
                .items()
                .iter()
                .all(|item| item.message() != Some(&Message::user("replace this prompt")))
        );
        inspect(&engine, false);
    }
}

#[tokio::test]
async fn resumed_disabled_and_skill_free_scopes_keep_full_pins_but_not_active_bodies() {
    for worker in [false, true] {
        let (_directory, mut writer) = test_transcript();
        let pin = test_skills().get("review").unwrap().snapshot();
        writer
            .append(&TranscriptItem::SkillInvocation(SkillInvocation::new(
                pin.name().clone(),
                "original",
                SkillApplication::Activate(pin.clone()),
            )))
            .unwrap();
        let mut policies = test_policies();
        if worker {
            policies.policy_mut(SessionMode::Build).skills_enabled = false;
        }
        let disabled = SkillCatalog::default()
            .with_config(zevria_instructions::skill::SkillsConfig {
                enabled: false,
                rules: vec![],
            })
            .unwrap();
        let mut engine = SessionEngine::new(
            Provider::new(false, "CURRENT_APPLICATION_SENTINEL"),
            test_skill_tools(),
            policies,
            writer,
            Arc::new(disabled),
        )
        .unwrap();
        run(&mut engine, submit("resume disabled")).await;
        let snapshot = engine.directive_state().unwrap().snapshot();
        assert!(
            !snapshot
                .directives
                .iter()
                .any(|d| matches!(d.payload, DirectivePayload::SkillBody { .. }))
        );
        let catalog = engine
            .instruction_set(engine.policies.policy(SessionMode::Build))
            .catalog;
        assert_eq!(catalog.is_some(), !worker);
        assert!(catalog.iter().all(|catalog| !catalog.enabled));
        assert_eq!(engine.active_skills().unwrap().get(pin.name()), Some(&pin));
        inspect(&engine, true);
        engine = engine
            .with_skill_catalog(Arc::new(SkillCatalog::default()))
            .unwrap();
        run(&mut engine, submit("reenabled configuration")).await;
        let snapshot = engine.directive_state().unwrap().snapshot();
        assert_eq!(snapshot.directives.iter().any(|d| matches!(&d.payload, DirectivePayload::SkillBody { body, .. } if body == pin.body())), !worker);
        assert_eq!(
            engine
                .instruction_set(engine.policies.policy(SessionMode::Build))
                .catalog
                .is_some(),
            !worker
        );
        inspect(&engine, true);
    }
}

#[test]
fn maintenance_preparation_uses_only_captured_guidance() {
    let (directory, writer) = test_transcript();
    let captured = roots(directory.path(), "CURRENT");
    std::fs::remove_file(directory.path().join("global/AGENTS.md")).unwrap();
    let engine = SessionEngine::new(
        Provider::new(false, "CURRENT_APPLICATION_SENTINEL"),
        test_skill_tools(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap()
    .with_guidance_roots(captured);
    let set = engine.prepare_maintenance_guidance();
    assert!(set.system.iter().any(|(component, text)| component
        == zevria_instructions::GLOBAL_GUIDANCE_COMPONENT
        && text.is_empty()));
    assert!(set.render().contains("CURRENT_PROJECT_SENTINEL"));
    assert!(!set.render().contains("Scope: global"));
    assert!(set.catalog.is_none());
    assert!(engine.conversation.items().is_empty());
}
