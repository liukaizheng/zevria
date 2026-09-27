//! Narrow private-admission wrappers for the synthetic Criterion harness.
use super::*;

struct NoNetwork;
impl ModelProvider for NoNetwork {
    fn complete<'a>(&'a mut self, _: ModelRequest<'a>, _: ProgressReporter) -> ProviderFuture<'a> {
        Box::pin(async { anyhow::bail!("benchmark provider must not be called") })
    }
    fn reset(&mut self) {}
    fn count_input_tokens<'a>(&'a mut self, _: ModelRequest<'a>) -> InputTokenCountFuture<'a> {
        Box::pin(async { Ok(InputTokenCount::Exact(1)) })
    }
}

/// Never executed: admission only needs the advertised engine-owned capability.
struct SkillCapability;
impl rig_agent::tool::Tool for SkillCapability {
    const NAME: &'static str = SKILL_TOOL_NAME;
    type Error = std::convert::Infallible;
    type Args = SkillRequest;
    type Output = String;
    fn description(&self) -> String {
        "Synthetic skill capability".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(&self, _: &mut ToolContext, _: SkillRequest) -> Result<String, Self::Error> {
        panic!("admission benchmarks must not execute tools")
    }
}

pub struct AdmissionFixture {
    engine: SessionEngine<NoNetwork>,
    _directory: tempfile::TempDir,
}
impl AdmissionFixture {
    /// Requires an entered Tokio runtime; setup is excluded by `iter_batched`.
    pub fn new(items: Vec<TranscriptItem>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create_with_id(directory.path(), "benchmark").unwrap();
        writer.rewrite(&items).unwrap();
        let policy = TurnPolicy::new("benchmark policy", None, ModelRole::Build, true);
        let engine = SessionEngine::new(
            NoNetwork,
            rig_agent::tool::server::ToolServer::new().run(),
            SessionPolicies::new(policy.clone(), policy),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_transcript_items(items)
        .unwrap();
        Self {
            engine,
            _directory: directory,
        }
    }
    /// Synthetic admission history with a reconciled prefix. Setup, persistence,
    /// and authoritative replay are all outside the measured operation.
    pub fn prompt(records: usize, payload_bytes: usize, activated: bool) -> Self {
        use zevria_instructions::skill::{SkillDefinition, SkillSource};
        let definition = SkillDefinition::new(
            SkillName::parse("benchmark-skill").unwrap(),
            "Synthetic admission skill",
            "Pinned benchmark instructions",
            SkillSource::Programmatic("benchmark".into()),
        )
        .unwrap();
        let snapshot = definition.snapshot();
        let catalog = Arc::new(SkillCatalog::new([definition]).unwrap());
        let mut fixture = Self::new(Vec::new());
        fixture.engine.tools = rig_agent::tool::server::ToolServer::new()
            .tool(SkillCapability)
            .run();
        fixture.engine = fixture.engine.with_skill_catalog(catalog).unwrap();
        let policy = fixture.engine.policies.policy(SessionMode::Build).clone();
        fixture.engine.reconcile_before_dispatch(&policy).unwrap();
        if activated {
            fixture
                .engine
                .record_required(TranscriptItem::SkillInvocation(SkillInvocation::new(
                    snapshot.name().clone(),
                    "first activation",
                    SkillApplication::Activate(snapshot),
                )))
                .unwrap();
            fixture.engine.reconcile_before_dispatch(&policy).unwrap();
        }
        let history = crate::transcript_bench::fixture(
            crate::transcript_bench::Workload::Replay,
            records,
            payload_bytes,
        );
        fixture
            .engine
            .record_required_items(history.items.into_iter().skip(2).collect())
            .unwrap();
        fixture
    }

    fn prompt_input(&self, edit: bool, skill: bool) -> (TurnAnchor, PromptTurnInput) {
        let anchor = if edit {
            // Retain almost all history so payload size and replay costs matter.
            TurnAnchor::ReplaceFrom(self.engine.conversation.items().len() - 2)
        } else {
            TurnAnchor::Append
        };
        let input = if skill {
            PromptTurnInput::Skill {
                name: SkillName::parse("benchmark-skill").unwrap(),
                args: "next".into(),
            }
        } else {
            PromptTurnInput::Message {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "next".into(),
            }
        };
        (anchor, input)
    }

    /// Exercise synchronous preparation only, excluding projection and durability.
    pub fn admit(&self, edit: bool, skill: bool) {
        let (anchor, input) = self.prompt_input(edit, skill);
        let checked = self
            .engine
            .preflight_prompt(anchor, input, SessionMode::Build)
            .unwrap_or_else(|_| panic!("synthetic prompt must pass preflight"));
        let admitted = self
            .engine
            .prepare_prompt(checked)
            .unwrap_or_else(|_| panic!("synthetic prompt must be admitted"));
        std::hint::black_box(admitted);
    }

    /// Preparation + one projection and deterministic exact measurement, no I/O.
    pub async fn measure(&mut self, edit: bool, skill: bool) {
        let (anchor, input) = self.prompt_input(edit, skill);
        let checked = self
            .engine
            .preflight_prompt(anchor, input, SessionMode::Build)
            .unwrap_or_else(|_| panic!("synthetic preflight"));
        let (context, prepared) = self
            .engine
            .prepare_prompt(checked)
            .unwrap_or_else(|_| panic!("synthetic preparation"));
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        let measured = self
            .engine
            .measure_prompt(&context, &prepared, None, true, &turn)
            .await
            .unwrap_or_else(|_| panic!("synthetic measurement"));
        std::hint::black_box(measured);
    }

    /// Complete admission including authoritative replay and durable I/O, but
    /// no completion/dispatch. Fixture setup is excluded by the Criterion caller.
    pub async fn accept(&mut self, edit: bool, skill: bool) {
        let (anchor, input) = self.prompt_input(edit, skill);
        let (events, _receiver) = zevria_session_api::session_event_channel(64);
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        let accepted = self
            .engine
            .prepare_and_accept_prompt(anchor, input, SessionMode::Build, &events, &turn)
            .await
            .unwrap_or_else(|_| panic!("synthetic acceptance"));
        std::hint::black_box(accepted);
    }

    pub fn required(&mut self, text: &str) {
        self.engine
            .record_required(TranscriptItem::Message(Message::user(text)))
            .unwrap();
    }
    pub fn completed(&mut self, text: &str) {
        self.engine
            .record_completed(TranscriptItem::Message(Message::assistant(text)))
            .unwrap();
    }
    pub fn prepare(&self, text: &str) {
        let checked = self
            .engine
            .preflight_prompt(
                TurnAnchor::Append,
                PromptTurnInput::Message {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                },
                SessionMode::Build,
            )
            .unwrap_or_else(|_| panic!("synthetic prompt must pass preflight"));
        let prepared = self
            .engine
            .prepare_prompt(checked)
            .unwrap_or_else(|_| panic!("synthetic prompt must prepare"));
        std::hint::black_box(prepared);
    }
    pub fn account_completed(&mut self, items: &[TranscriptItem]) {
        self.engine.append_context_estimates(items);
        self.engine.context.arm_all();
    }
}
