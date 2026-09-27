//! Exercise production argument validation and the engine's concurrent batch path together.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use rig_agent::tool::server::ToolServer;
use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolFunction, ToolResultContent, UserContent,
};
use serde_json::json;
use zevria_core::SessionEngine;
use zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::ModelRole;
use zevria_foundation::SessionMode;
use zevria_foundation::SessionPolicies;
use zevria_foundation::SubtaskKind;
use zevria_foundation::SubtaskOutcome;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::TurnPolicy;
use zevria_instructions::SkillCatalog;
use zevria_model::ModelRequest;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionUpdate;
use zevria_session_api::session_event_channel;
use zevria_session_api::subtask_channels;
use zevria_tools::LaunchSubtasksTool;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

const FIRST_TITLE: &str = "Summarize model lifecycle refactor";
const SECOND_TITLE: &str = "Summarize UI and CLI model changes";
const CALL_IDS: [&str; 2] = ["call-first", "call-second"];
const PROMPTS: [&str; 2] = [
    "Inspect the synthetic model lifecycle fixture and summarize its boundaries.",
    "Inspect the synthetic UI and CLI fixtures and summarize their model settings.",
];

struct CapturedRequest {
    messages: Vec<Message>,
    role: ModelRole,
    allowed_tools: Option<Vec<String>>,
}

struct ScriptedProvider {
    responses: VecDeque<Message>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        _progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("request log")
                .push(CapturedRequest {
                    messages: request.owned_messages(),
                    role: request.model_role,
                    allowed_tools: request.allowed_tool_names.map(<[_]>::to_vec),
                });
            zevria_model::ModelResponse::plain(
                self.responses
                    .pop_front()
                    .expect("no repair round expected"),
            )
        })
    }

    fn reset(&mut self) {
        panic!("the scripted turn must not fail or reset");
    }
}

fn launch_enabled_policies() -> SessionPolicies {
    let policy = |role| {
        TurnPolicy::new(
            "Synthetic launch-only test policy",
            Some(vec![LAUNCH_SUBTASKS_TOOL_NAME.to_string()]),
            role,
            false,
        )
    };
    SessionPolicies::new(policy(ModelRole::Build), policy(ModelRole::Plan))
}

fn assistant_batch(second_title: &str, separate_calls: bool, serialized: bool) -> Message {
    let tasks = [FIRST_TITLE, second_title].into_iter().enumerate().map(|(index, title)|
        json!({"title": title, "prompt": PROMPTS[index], "type": "explore", "workspace": null})
    ).collect::<Vec<_>>();
    let batches = if separate_calls {
        tasks.into_iter().map(|task| vec![task]).collect()
    } else {
        vec![tasks]
    };
    let mut content: Vec<_> = batches
        .into_iter()
        .enumerate()
        .map(|(index, tasks)| {
            AssistantContent::ToolCall(ToolCall::from_dual_wire(
                format!("fc-{}", CALL_IDS[index]),
                CALL_IDS[index],
                ToolFunction::new(
                    LAUNCH_SUBTASKS_TOOL_NAME.to_string(),
                    json!({"tasks": tasks}),
                ),
            ))
        })
        .collect();
    if serialized {
        // The submission gate serializes surrounding calls. Internal batch
        // fan-out must still reach the two-request barrier below.
        content.push(AssistantContent::ToolCall(ToolCall::from_dual_wire(
            "fc-submit",
            "submit",
            ToolFunction::new("submit_plan".into(), json!({})),
        )));
    }
    Message::Assistant { id: None, content }
}

async fn exercise_batch(
    mode: SessionMode,
    second_title: &str,
    separate_calls: bool,
    serialized: bool,
) {
    let valid = !second_title.trim().is_empty();
    let expected_launches = if valid {
        2
    } else if separate_calls {
        1
    } else {
        0
    };
    let (events_tx, mut events_rx) = session_event_channel(128);
    let mut channels = subtask_channels("synthetic-root", events_tx.clone());
    let startup = tempfile::tempdir().unwrap();
    let tools = ToolServer::new()
        .tool(LaunchSubtasksTool::new(
            channels.launcher,
            startup.path().to_path_buf(),
        ))
        .run();
    let assistant = assistant_batch(second_title, separate_calls, serialized);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: [
            assistant.clone(),
            Message::assistant("All outcomes considered."),
        ]
        .into(),
        requests: requests.clone(),
    };
    let directory = tempfile::tempdir().unwrap();
    let transcript = TranscriptWriter::create(directory.path()).unwrap();
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        launch_enabled_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let supervisor = async {
        let mut launches = Vec::new();
        for _ in 0..expected_launches {
            launches.push(channels.requests.recv().await.expect("launch request"));
        }
        // CRITICAL: not one report is released before every request arrives.
        // Correlate using distinct prompts, even when both titles are identical.
        launches.sort_by_key(|request| {
            PROMPTS
                .iter()
                .position(|prompt| *prompt == request.prompt)
                .unwrap()
        });
        let descriptors = launches
            .iter()
            .map(|request| request.descriptor.clone())
            .collect::<Vec<_>>();
        for (index, request) in launches.into_iter().enumerate().rev() {
            assert_eq!(request.descriptor.kind, SubtaskKind::Explore);
            assert_eq!(request.turn.mode, mode);
            request
                .outcome
                .send(SubtaskOutcome::Completed {
                    report: format!("REPORT_{index}"),
                })
                .unwrap();
        }
        descriptors
    };
    let (execution, descriptors) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            engine.handle_command(
                SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "Inspect both paths concurrently.".into(),
                    mode,
                }),
                &events_tx
            ),
            supervisor
        )
    })
    .await
    .expect("all launches must arrive before either report is released");
    execution.unwrap();
    assert!(channels.requests.try_recv().is_err());
    let mut events = Vec::new();
    while let Ok(update) = events_rx.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            events.push(event);
        }
    }
    let launches = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::SubtaskLaunched {
                call_id,
                entry_index,
                descriptor,
                ..
            } => Some((call_id, *entry_index, descriptor)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(launches.len(), expected_launches);
    for (index, descriptor) in descriptors.iter().enumerate() {
        assert!(
            launches
                .iter()
                .any(|(call_id, entry_index, launched)| call_id.as_str()
                    == CALL_IDS[if separate_calls { index } else { 0 }]
                    && *entry_index == if separate_calls { 0 } else { index }
                    && *launched == descriptor)
        );
    }
    if descriptors.len() == 2 {
        assert_ne!(descriptors[0].id, descriptors[1].id);
    }
    let batches = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(batches.len(), 1);
    let (results, metadata) = batches[0];
    let Message::User { content } = results else {
        panic!("tool results")
    };
    let launch_calls = if separate_calls { 2 } else { 1 };
    assert_eq!(content.len(), launch_calls + usize::from(serialized));
    assert_eq!(metadata.len(), content.len());
    for (slot, (content, metadata)) in content.iter().zip(metadata).take(launch_calls).enumerate() {
        let UserContent::ToolResult(result) = content else {
            panic!("tool result")
        };
        assert_eq!(result.call.as_str(), CALL_IDS[slot]);
        assert_eq!(result.provider.as_ref().unwrap().call_id, CALL_IDS[slot]);
        assert_eq!(metadata.id, CALL_IDS[slot]);
        assert_eq!(metadata.call_id.as_deref(), Some(CALL_IDS[slot]));
        let [ToolResultContent::Text(text)] = result.content.as_slice() else {
            panic!("text")
        };
        let accepted = valid || (separate_calls && slot == 0);
        if accepted {
            assert_eq!(metadata.outcome, ToolCallOutcome::Success);
            let expected = if separate_calls { 1 } else { 2 };
            assert_eq!(metadata.subtasks().len(), expected);
            for (index, entry) in metadata.subtasks().iter().enumerate() {
                assert_eq!(entry.index, index);
                assert_eq!(entry.status, zevria_foundation::SubtaskStatus::Completed);
                let original = if separate_calls { slot } else { index };
                assert_eq!(entry.launch.as_ref().unwrap().id, descriptors[original].id);
                assert!(text.text.contains(&format!("REPORT_{original}")));
            }
            if !separate_calls {
                assert!(text.text.find("REPORT_0") < text.text.find("REPORT_1"));
            }
            assert!(text.text.contains(&format!("launched: {expected}")));
        } else {
            assert_eq!(metadata.outcome, ToolCallOutcome::Error);
            assert!(metadata.detail.is_none());
            assert!(text.text.contains("title must not be empty"));
        }
        for prompt in PROMPTS {
            assert!(!text.text.contains(prompt));
        }
    }
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one launch response and one continuation"
    );
    assert_eq!(requests[1].messages.last(), Some(results));
    for request in requests.iter() {
        assert_eq!(
            request.role,
            if mode == SessionMode::Plan {
                ModelRole::Plan
            } else {
                ModelRole::Build
            }
        );
        assert_eq!(
            request.allowed_tools.as_deref(),
            Some([LAUNCH_SUBTASKS_TOOL_NAME.to_string()].as_slice())
        );
    }
    let restored = transcript::load(&transcript_path).unwrap();
    let restored_batches = restored
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restored_batches, batches);
}

#[tokio::test]
async fn exactly_one_native_call_launches_both_before_either_report_including_serialized_dispatch()
{
    for mode in [SessionMode::Build, SessionMode::Plan] {
        for serialized in [false, true] {
            exercise_batch(mode, FIRST_TITLE, false, serialized).await;
        }
    }
}

#[tokio::test]
async fn invalid_second_entry_rejects_the_entire_batch() {
    for mode in [SessionMode::Build, SessionMode::Plan] {
        exercise_batch(mode, " \n\t ", false, false).await;
    }
}

#[tokio::test]
async fn multiple_native_batches_still_overlap_and_isolate_batch_validation() {
    exercise_batch(SessionMode::Build, SECOND_TITLE, true, false).await;
    exercise_batch(SessionMode::Plan, " ", true, false).await;
}
