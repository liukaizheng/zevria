//! Ordinary text through the real package validator, engine, and SkillTool.
//! Scripted selection proves visibility/ordering, not semantic model compliance.
use rig_agent::tool::{Tool, ToolContext, server::ToolServer};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolFunction};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use zevria_core::SessionEngine;
use zevria_foundation::ModelRole;
use zevria_foundation::SessionMode;
use zevria_foundation::SessionPolicies;
use zevria_foundation::TurnPolicy;
use zevria_instructions::DirectivePayload;
use zevria_instructions::skill::FixedSkillRoots;
use zevria_instructions::skill::SkillCatalog;
use zevria_instructions::skill::validate_skill_path;
use zevria_model::ModelRequest;
use zevria_model::OwnedModelRequestItem;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;
use zevria_session_api::SessionCommand;
use zevria_session_api::TurnCommand;
use zevria_session_api::session_event_channel;
use zevria_tools::SkillTool;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

#[derive(serde::Serialize)]
struct CapturedRequest {
    instructions: String,
    input: Vec<OwnedModelRequestItem>,
}
struct ScriptedProvider {
    responses: VecDeque<Message>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    actions: Arc<AtomicUsize>,
}
impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        _: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let mut requests = self.requests.lock().unwrap();
            // First and post-activation requests must precede any task action.
            if requests.len() <= 1 {
                assert_eq!(self.actions.load(Ordering::SeqCst), 0);
            }
            requests.push(CapturedRequest {
                instructions: request.instructions.into(),
                input: request
                    .input
                    .iter()
                    .map(|item| item.to_owned_item())
                    .collect::<anyhow::Result<Vec<_>>>()?,
            });
            zevria_model::ModelResponse::plain(
                self.responses.pop_front().expect("scripted response"),
            )
        })
    }
    fn reset(&mut self) {
        panic!("unexpected provider reset");
    }
}
struct TaskAction(Arc<AtomicUsize>);
impl Tool for TaskAction {
    const NAME: &'static str = "command";
    type Args = serde_json::Value;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Record a synthetic task action; never run Git".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("synthetic task action".into())
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall::from_dual_wire(
            format!("fc-{id}"),
            id,
            ToolFunction::new(name.to_string(), args),
        ))],
    }
}
fn captured_catalog(
    request: &CapturedRequest,
) -> Vec<zevria_instructions::skill::SkillPromptEntry> {
    serde_json::from_str(request.instructions.lines().last().unwrap()).unwrap()
}

#[tokio::test]
async fn ordinary_commit_discloses_metadata_then_pins_before_action_and_reapplies_bodylessly() {
    let workspace = tempfile::tempdir().unwrap();
    let roots = FixedSkillRoots::capture(workspace.path());
    std::fs::create_dir_all(roots.project()).unwrap();
    let source = roots.project().join("commit.md");
    std::fs::write(&source, include_str!("fixtures/commit.md")).unwrap();
    // Validate only the fixture path; never scan or depend on the real HOME.
    let (definition, diagnostics) = validate_skill_path(&roots, &source).unwrap();
    assert!(diagnostics.is_empty());
    let body = definition.body().to_string();
    let registry = Arc::new(SkillCatalog::new([definition]).unwrap());
    let catalog = registry.clone();
    let actions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(SkillTool)
        .tool(TaskAction(actions.clone()))
        .run();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: [
            call("activate", "skill", json!({"skill":"commit"})),
            call("task", "command", json!({})),
            Message::assistant("first task complete"),
            call("reapply", "skill", json!({"skill":"commit"})),
            Message::assistant("second application complete"),
        ]
        .into(),
        requests: requests.clone(),
        actions: actions.clone(),
    };
    let policy = |role| {
        TurnPolicy::new(
            "Follow the engine skill selection contract",
            None,
            role,
            true,
        )
    };
    let policies = SessionPolicies::new(policy(ModelRole::Build), policy(ModelRole::Plan));
    let writer = TranscriptWriter::create(&workspace.path().join("sessions")).unwrap();
    let path = writer.path().to_path_buf();
    let mut engine = SessionEngine::new(provider, tools, policies, writer, registry)
        .unwrap()
        .with_skill_catalog(catalog)
        .unwrap();
    let (events, _receiver) = session_event_channel(128);
    for text in ["commit the changes", "Please record this patch as a commit"] {
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(captured_catalog(&requests[0]).len(), 1);
    let entry = &captured_catalog(&requests[0])[0];
    assert_eq!(entry.name.as_str(), "commit");
    assert!(entry.description.contains("detailed Git commit"));
    assert!(
        !serde_json::to_string(&requests[0])
            .unwrap()
            .contains("Fixture commit procedure")
    );
    assert!(
        requests
            .iter()
            .all(|request| request.instructions == requests[0].instructions)
    );
    for request in &requests[1..] {
        let bodies = request
            .input
            .iter()
            .filter_map(|item| match item {
                OwnedModelRequestItem::DeveloperInstruction(d) => match &d.payload {
                    DirectivePayload::SkillBody { body, .. } => Some(body),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(bodies, [&body]);
    }
    assert_eq!(actions.load(Ordering::SeqCst), 1);
    let restored = transcript::load(&path).unwrap();
    assert_eq!(restored, engine.conversation().items());
    assert_eq!(
        transcript::replay_active_skills(&restored)
            .unwrap()
            .snapshots()
            .next()
            .unwrap()
            .body(),
        body
    );
    assert_eq!(
        restored
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::ToolResults {
                    skill_applications, ..
                } => Some(skill_applications),
                _ => None,
            })
            .flatten()
            .filter(|accepted| matches!(
                accepted.application,
                zevria_instructions::SkillApplication::Activate(_)
            ))
            .count(),
        1
    );
    assert!(
        !restored
            .iter()
            .any(|item| matches!(item, TranscriptItem::SkillInvocation(_))),
        "plain prompts are not rewritten"
    );
    assert_eq!(
        restored
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Directive(_)))
            .count(),
        1
    );
    let live = engine.conversation().items();
    let activation = live
        .iter()
        .position(|item| matches!(item, TranscriptItem::ToolResults { skill_applications, .. } if !skill_applications.is_empty()))
        .unwrap();
    assert!(matches!(
        &live[activation],
        TranscriptItem::ToolResults { .. }
    ));
    assert!(
        matches!(&live[activation + 1], TranscriptItem::Directive(d) if matches!(d.payload, DirectivePayload::SkillBody { .. }))
    );
}
