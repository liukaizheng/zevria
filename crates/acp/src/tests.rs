#[path = "mode_protocol_tests.rs"]
mod mode_protocol_tests;
#[path = "skill_tests.rs"]
mod skill_tests;
#[path = "worker_tests.rs"]
mod worker_tests;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::SystemTime,
};

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        CancelNotification, ClientCapabilities, CloseSessionRequest, ContentBlock,
        CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
        ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
        ElicitationFormCapabilities, ImageContent, InitializeRequest, ListSessionsRequest,
        LoadSessionRequest, NewSessionRequest, PromptRequest, RequestPermissionRequest,
        ResumeSessionRequest, SessionNotification, SessionUpdate as AcpSessionUpdate,
        SetSessionModeRequest, StopReason,
    },
};
use agent_client_protocol::{Channel, Client};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolCallId, ToolFunction};
use tokio::sync::{mpsc, oneshot};
use zevria_foundation::QuestionOption;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequest;
use zevria_foundation::QuestionRequestId;
use zevria_foundation::QuestionResponse;
use zevria_foundation::SessionMode;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::ToolResultDetail;
use zevria_foundation::ToolResultMetadata;
use zevria_foundation::TurnId;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::session_event_channel;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanHandoff;
use zevria_workflow::PlanId;
use zevria_workflow::PlanResolution;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;

use super::*;
use crate::agent::serve_on;
use crate::elicitation::question_form;
use crate::stream::StreamSegments;

#[test]
fn retry_diagnostics_announce_rounded_delays_and_keep_the_existing_channel() {
    use std::time::Duration;
    for (delay, expected) in [
        (Duration::ZERO, "Provider retry 2/5: offline"),
        (
            Duration::from_millis(500),
            "Provider retry 2/5 (next attempt in 1s): offline",
        ),
        (
            Duration::from_millis(1500),
            "Provider retry 2/5 (next attempt in 2s): offline",
        ),
        (
            Duration::from_secs(4),
            "Provider retry 2/5 (next attempt in 4s): offline",
        ),
    ] {
        let update = crate::session::retry_diagnostic(TurnId::new(7), 2, 5, delay, "offline");
        let AcpSessionUpdate::AgentThoughtChunk(chunk) = update else {
            panic!("retry must use the existing thought diagnostic channel");
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("plain diagnostic text")
        };
        assert_eq!(text.text, expected);
        assert_eq!(
            chunk.message_id.unwrap().0.as_ref(),
            "zevria-turn-7-status-retry"
        );
    }
}

#[test]
fn resetting_a_retry_stream_replays_shared_prefixes_with_a_new_segment() {
    let turn = TurnId::new(7);
    let mut streams = StreamSegments::default();
    let first = streams.snapshot(turn, &Message::assistant("partial"));
    streams.reset();
    let replay = streams.snapshot(turn, &Message::assistant("partial and resumed"));
    assert_eq!(chunk_text(&replay[0]), "partial and resumed");
    assert_ne!(chunk_id(&first[0]), chunk_id(&replay[0]));
}

#[test]
fn acp_config_defaults_and_validation_are_strict() {
    assert_eq!(
        toml::from_str::<AcpConfig>("").expect("partial config"),
        AcpConfig::default()
    );
    assert!(
        AcpConfig {
            max_sessions: 0,
            expose_session_list: true,
        }
        .validate()
        .is_err()
    );
    assert!(toml::from_str::<AcpConfig>("unknown = true").is_err());
}

#[test]
fn stream_snapshots_append_suffixes_and_divergence_starts_an_authoritative_segment() {
    let turn = TurnId::new(7);
    let mut streams = StreamSegments::default();
    let first = streams.snapshot(turn, &Message::assistant("hel"));
    let second = streams.snapshot(turn, &Message::assistant("hello"));
    let divergent = streams.terminal(turn, &Message::assistant("help"));

    assert_eq!(chunk_text(&first[0]), "hel");
    assert_eq!(chunk_text(&second[0]), "lo");
    assert_eq!(chunk_id(&first[0]), chunk_id(&second[0]));
    assert_eq!(chunk_text(&divergent[0]), "help");
    assert_ne!(chunk_id(&first[0]), chunk_id(&divergent[0]));
    assert!(chunk_id(&divergent[0]).ends_with("-final"));
}

#[test]
fn terminal_monotonic_snapshot_keeps_the_existing_message_id() {
    let turn = TurnId::new(9);
    let mut streams = StreamSegments::default();
    let first = streams.snapshot(turn, &Message::assistant("coalesced"));
    let final_update = streams.terminal(turn, &Message::assistant("coalesced stream"));
    assert_eq!(chunk_text(&final_update[0]), " stream");
    assert_eq!(chunk_id(&first[0]), chunk_id(&final_update[0]));
}

pub(super) fn chunk_text(update: &AcpSessionUpdate) -> &str {
    let AcpSessionUpdate::AgentMessageChunk(chunk) = update else {
        panic!("expected agent message chunk, got {update:?}");
    };
    let ContentBlock::Text(text) = &chunk.content else {
        panic!("expected text chunk");
    };
    &text.text
}

pub(super) fn chunk_id(update: &AcpSessionUpdate) -> &str {
    let AcpSessionUpdate::AgentMessageChunk(chunk) = update else {
        panic!("expected agent message chunk, got {update:?}");
    };
    chunk.message_id.as_ref().expect("message id").0.as_ref()
}

#[test]
fn question_forms_round_trip_optional_text_multi_select_and_other_values() {
    let request = QuestionRequest {
        id: QuestionRequestId::new("batch"),
        questions: vec![
            QuestionPrompt {
                id: "note".to_string(),
                header: "Note".to_string(),
                question: "Optional note".to_string(),
                options: Vec::new(),
                kind: QuestionPromptKind::Text {
                    min_length: None,
                    max_length: Some(20),
                },
                required: false,
                default: Some(zevria_foundation::QuestionAnswerValue::String(
                    "default".to_string(),
                )),
            },
            QuestionPrompt {
                id: "targets".to_string(),
                header: "Targets".to_string(),
                question: "Choose targets".to_string(),
                options: vec![QuestionOption {
                    label: "Core".to_string(),
                    description: "Core crate".to_string(),
                }],
                kind: QuestionPromptKind::MultiSelect {
                    min_selections: Some(1),
                    max_selections: Some(2),
                    allow_other: true,
                },
                required: true,
                default: None,
            },
        ],
        source_label: Some("Test".to_string()),
        dismissible: true,
    };
    let form = question_form(
        &agent_client_protocol::schema::v1::SessionId::new("session"),
        &request,
    )
    .expect("question form");
    let response = form
        .response(CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new().content(BTreeMap::from([
                (
                    "question_1".to_string(),
                    ElicitationContentValue::StringArray(vec![
                        "option_0".to_string(),
                        "__zevria_other__".to_string(),
                    ]),
                ),
                (
                    "question_1_other".to_string(),
                    ElicitationContentValue::String("Frontend".to_string()),
                ),
            ])),
        )))
        .expect("accepted form response");
    let QuestionResponse::Answered { answers } = response else {
        panic!("expected answered question");
    };
    assert_eq!(answers[0].id, "note");
    assert!(answers[0].answer.is_none());
    assert_eq!(
        answers[1]
            .answer
            .as_ref()
            .and_then(|answer| answer.as_strings()),
        Some(["Core".to_string(), "Frontend".to_string()].as_slice())
    );
}

#[test]
fn tool_results_omit_titles_and_preserve_correlated_payloads_for_every_outcome() {
    use crate::project::{KnownTools, project_tool_calls, project_tool_results};
    use agent_client_protocol::schema::v1::{ToolCallStatus, ToolKind};

    let workspace = std::path::Path::new("workspace");
    for (name, arguments, kind) in [
        (
            "command",
            serde_json::json!({"command": "rtk cargo test"}),
            ToolKind::Execute,
        ),
        (
            "read",
            serde_json::json!({"file_path": "src/lib.rs"}),
            ToolKind::Read,
        ),
    ] {
        let mut known = KnownTools::new();
        let calls = project_tool_calls(
            &Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall::new(
                    ToolCallId::new_or_mint("call"),
                    ToolFunction::new(name.to_string(), arguments.clone()),
                ))],
            },
            workspace,
            &mut known,
        );
        let initial = serde_json::to_value(&calls[0]).unwrap();
        assert_eq!(
            initial["title"],
            if name == "command" {
                "command: rtk cargo test"
            } else {
                "read: src/lib.rs"
            }
        );
        assert_eq!(initial["rawInput"], arguments);
        for outcome in [
            ToolCallOutcome::Success,
            ToolCallOutcome::Error,
            ToolCallOutcome::Skipped,
            ToolCallOutcome::Denied,
            ToolCallOutcome::Cancelled,
            ToolCallOutcome::Partial,
        ] {
            let updates = project_tool_results(
                &Message::tool_result("call", name, "output"),
                &[ToolResultMetadata {
                    diagnostic: Some("display-only diagnostic".into()),
                    id: "call".to_string(),
                    call_id: None,
                    tool_name: name.to_string(),
                    outcome,
                    detail: Some(ToolResultDetail::FileChanges(vec![
                        zevria_foundation::FileChangeOutput {
                            path: "new.txt".into(),
                            change: zevria_foundation::FileChange::Add {
                                content: "new content".to_string(),
                            },
                        },
                    ])),
                }],
                workspace,
                &known,
            );
            assert_eq!(updates.len(), 1);
            let AcpSessionUpdate::ToolCallUpdate(update) = &updates[0] else {
                panic!("tool result update")
            };
            assert_eq!(update.tool_call_id.to_string(), "call");
            assert_eq!(update.fields.kind, Some(kind));
            assert_eq!(
                update.fields.status,
                Some(if outcome == ToolCallOutcome::Success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                })
            );
            let wire = serde_json::to_value(update).unwrap();
            assert!(wire.get("title").is_none());
            assert_eq!(wire["rawInput"], arguments);
            assert_eq!(wire["rawOutput"], "output");
            assert_eq!(
                wire["_meta"][zevria_foundation::TOOL_RESULT_META_KEY]["diagnostic"],
                "display-only diagnostic"
            );
            assert_eq!(
                wire["_meta"][zevria_foundation::TOOL_RESULT_META_KEY]["outcome"],
                serde_json::to_value(outcome).unwrap()
            );
            assert_eq!(wire["content"][0]["content"]["text"], "output");
            assert_eq!(wire["content"][1]["newText"], "new content");
        }
    }
    let unknown = project_tool_results(
        &Message::tool_result("unknown", "command", "unmatched output"),
        &[ToolResultMetadata {
            diagnostic: None,
            id: "unknown".to_string(),
            call_id: None,
            tool_name: "command".to_string(),
            outcome: ToolCallOutcome::Error,
            detail: None,
        }],
        workspace,
        &KnownTools::new(),
    );
    let wire = serde_json::to_value(&unknown[0]).unwrap();
    assert_eq!(wire["toolCallId"], "unknown");
    assert_eq!(wire["status"], "failed");
    assert_eq!(wire["rawOutput"], "unmatched output");
    for absent in ["title", "rawInput", "kind"] {
        assert!(wire.get(absent).is_none(), "{absent}: {wire}");
    }
}

#[test]
fn native_question_companions_preserve_requiredness_defaults_and_round_trip_answers() {
    use serde_json::json;
    use zevria_foundation::QuestionAnswerValue;

    for multi in [false, true] {
        for required in [false, true] {
            let answer = |custom: bool| {
                if multi {
                    QuestionAnswerValue::Strings(if custom {
                        vec!["Core".into(), "Custom".into()]
                    } else {
                        vec!["Core".into()]
                    })
                } else {
                    QuestionAnswerValue::String(if custom { "Custom" } else { "Core" }.into())
                }
            };
            let request = QuestionRequest {
                id: QuestionRequestId::new("native"),
                questions: vec![QuestionPrompt {
                    id: "target".into(),
                    header: "Target".into(),
                    question: "Choose a target".into(),
                    options: vec![QuestionOption {
                        label: "Core".into(),
                        description: "Core crate".into(),
                    }],
                    kind: if multi {
                        QuestionPromptKind::MultiSelect {
                            min_selections: Some(1),
                            max_selections: Some(2),
                            allow_other: true,
                        }
                    } else {
                        QuestionPromptKind::SingleSelect { allow_other: true }
                    },
                    required,
                    default: Some(answer(true)),
                }],
                source_label: None,
                dismissible: true,
            };
            let form = question_form(&"session".into(), &request).unwrap();
            let wire = serde_json::to_value(&form.request).unwrap();
            let schema = &wire["requestedSchema"];
            assert_eq!(
                schema["properties"]["question_0_other"]["_meta"],
                json!({"zevria": {
                    "questionId": "question_0", "isOtherAnswer": true, "otherValue": "__zevria_other__"
                }})
            );
            assert_eq!(
                schema["properties"]["question_0_other"]["description"],
                "Custom answer used when Other is selected."
            );
            assert_eq!(
                schema["properties"]["question_0_other"]["default"],
                "Custom"
            );
            assert_eq!(
                schema["properties"]["question_0"]["default"],
                if multi {
                    json!(["option_0", "__zevria_other__"])
                } else {
                    json!("__zevria_other__")
                }
            );
            let required_fields = schema["required"].as_array().cloned().unwrap_or_default();
            assert_eq!(
                required_fields,
                if required {
                    vec![json!("question_0")]
                } else {
                    vec![]
                }
            );
            let respond = |content| {
                form.response(CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(content),
                )))
            };
            for custom in [false, true] {
                let primary = if multi {
                    ElicitationContentValue::StringArray(if custom {
                        vec!["option_0".into(), "__zevria_other__".into()]
                    } else {
                        vec!["option_0".into()]
                    })
                } else {
                    ElicitationContentValue::String(
                        if custom {
                            "__zevria_other__"
                        } else {
                            "option_0"
                        }
                        .into(),
                    )
                };
                let mut content = BTreeMap::from([("question_0".into(), primary)]);
                if custom {
                    assert!(
                        respond(content.clone()).is_err(),
                        "token requires companion text"
                    );
                    content.insert(
                        "question_0_other".into(),
                        ElicitationContentValue::String(" \n".into()),
                    );
                    assert!(
                        respond(content.clone()).is_err(),
                        "blank companion rejected"
                    );
                    content.insert(
                        "question_0_other".into(),
                        ElicitationContentValue::String("Custom".into()),
                    );
                }
                let QuestionResponse::Answered { answers } = respond(content).unwrap() else {
                    panic!("answered")
                };
                assert_eq!(answers.len(), 1);
                assert_eq!(answers[0].id, "target");
                assert_eq!(answers[0].answer, Some(answer(custom)));
            }
            let missing = respond(BTreeMap::from([(
                "question_0_other".into(),
                ElicitationContentValue::String("Custom".into()),
            )]));
            if required {
                assert!(
                    missing.is_err(),
                    "companion alone cannot satisfy required primary"
                );
            } else {
                let QuestionResponse::Answered { answers } = missing.unwrap() else {
                    panic!("optional skip")
                };
                assert!(answers[0].answer.is_none());
            }
            for action in [ElicitationAction::Decline, ElicitationAction::Cancel] {
                assert_eq!(
                    form.response(CreateElicitationResponse::new(action))
                        .unwrap(),
                    QuestionResponse::Dismissed
                );
            }
        }
    }
}

type ModeAcknowledgment = (
    String,
    SessionMode,
    oneshot::Sender<zevria_session_api::ModeSelectionResult>,
);

#[derive(Clone)]
struct FakeFactory {
    next_id: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    answers: Arc<Mutex<Vec<QuestionResponse>>>,
    persisted: Arc<Vec<TranscriptItem>>,
    selected_mode: SessionMode,
    mode_requests: Option<mpsc::UnboundedSender<ModeAcknowledgment>>,
    commands: Arc<Mutex<Vec<SessionCommand>>>,
}

impl FakeFactory {
    fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicUsize::new(1)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            answers: Arc::new(Mutex::new(Vec::new())),
            commands: Arc::new(Mutex::new(Vec::new())),
            selected_mode: SessionMode::Build,
            mode_requests: None,
            persisted: Arc::new(vec![
                TranscriptItem::SessionMode(SessionMode::Build),
                TranscriptItem::SessionModels(
                    zevria_model::models::SessionModels::new(
                        zevria_model::models::ModelSelection::new(
                            zevria_foundation::ModelProfileRef::new("persisted", "build"),
                            zevria_foundation::ReasoningLevel::Medium,
                        ),
                        zevria_model::models::ModelSelection::new(
                            zevria_foundation::ModelProfileRef::new("persisted", "plan"),
                            zevria_foundation::ReasoningLevel::Medium,
                        ),
                    )
                    .unwrap(),
                ),
                TranscriptItem::Message(Message::user("durable user")),
                TranscriptItem::Message(Message::assistant("durable assistant")),
            ]),
        }
    }
}

impl SessionRuntimeFactory for FakeFactory {
    fn start(
        &self,
        request: StartSessionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<StartedSession>> + Send + '_>>
    {
        Box::pin(async move {
            let session_id = match &request.start {
                SessionStart::New => {
                    format!("fake-{}", self.next_id.fetch_add(1, Ordering::AcqRel))
                }
                SessionStart::Existing { session_id } if session_id == "persisted" => {
                    session_id.clone()
                }
                SessionStart::Existing { session_id } => {
                    anyhow::bail!("no persisted session matches ID {session_id:?}")
                }
            };
            let transcript_items = match request.start {
                SessionStart::New => Vec::new(),
                SessionStart::Existing { .. } => self.persisted.as_ref().clone(),
            };
            let plan_state =
                zevria_workflow::replay_plan_state(transcript_items.iter().filter_map(|item| {
                    match item {
                        TranscriptItem::Plan(record) => Some(record),
                        _ => None,
                    }
                }))?;
            let initial_artifact = plan_state.artifact().cloned();
            let recorded = self.commands.clone();
            let (events_tx, events) = session_event_channel(64);
            let retained_events = events_tx.clone();
            let (commands, mut command_rx) = mpsc::unbounded_channel::<SessionCommand>();
            let (exit_tx, exit_rx) = oneshot::channel();
            let answers = Arc::clone(&self.answers);
            let runtime_session_id = session_id.clone();
            let mut selected_mode = self.selected_mode;
            let mode_requests = self.mode_requests.clone();
            let task = tokio::spawn(async move {
                let mut next_turn = 1u64;
                let mut active = None;
                let mut current_artifact = initial_artifact;
                use zevria_instructions::skill::*;
                let definition = SkillDefinition::new(
                    SkillName::parse("review").unwrap(),
                    "Review code",
                    "PRIVATE MAIN BODY",
                    SkillSource::Programmatic("ACP fixture".into()),
                )
                .unwrap();
                let registry = Arc::new(SkillCatalog::new([definition]).unwrap());
                let mut skills = SkillContext {
                    catalog: registry,
                    pins: ActiveSkills::default(),
                    mode_enabled: true,
                };
                while let Some(command) = command_rx.recv().await {
                    recorded.lock().unwrap().push(command.clone());
                    let mut skill_display = None;
                    let (command, revise) = match command {
                        SessionCommand::Turn(
                            zevria_session_api::TurnCommand::RevisePlanWithSkill {
                                expected,
                                name,
                                args,
                            },
                        ) => (
                            SessionCommand::Turn(zevria_session_api::TurnCommand::InvokeSkill {
                                name,
                                args,
                                mode: SessionMode::Plan,
                            }),
                            Some(expected),
                        ),
                        command => (command, None),
                    };
                    let command = match command {
                        SessionCommand::Manage(
                            zevria_session_api::ManagementCommand::SetMode { request_id, mode },
                        ) => {
                            let result = if let Some(requests) = &mode_requests {
                                let (ack, response) = oneshot::channel();
                                requests.send((request_id.clone(), mode, ack)).unwrap();
                                response.await.unwrap()
                            } else {
                                zevria_session_api::ModeSelectionResult::Accepted {
                                    mode,
                                    changed: selected_mode != mode,
                                }
                            };
                            if let zevria_session_api::ModeSelectionResult::Accepted {
                                mode, ..
                            } = &result
                            {
                                selected_mode = *mode;
                            }
                            let _ = events_tx
                                .send(SessionEvent::ModeResult { request_id, result })
                                .await;
                            continue;
                        }
                        SessionCommand::Manage(zevria_session_api::ManagementCommand::Skills {
                            request_id,
                            request,
                        }) => {
                            if matches!(&request, SkillManagementRequest::List { query, .. } if query == "fatal-runtime")
                            {
                                if let Some(turn_id) = active {
                                    events_tx
                                        .try_send(SessionEvent::TurnCompleted {
                                            display_attempt_id: None,
                                            turn_id,
                                            message: Message::assistant("contradictory success"),
                                        })
                                        .unwrap();
                                }
                                let _ = exit_tx.send(RuntimeExit::failed(
                                    "fake runtime",
                                    "invalid skill replay: original fatal diagnostic",
                                ));
                                return;
                            }
                            let result = if !request.is_mutation() {
                                match skills.management_view(&request) {
                                    Ok(view) => SkillManagementResult::View { view },
                                    Err(error) => SkillManagementResult::error(
                                        "query_failed",
                                        error.to_string(),
                                    ),
                                }
                            } else if active.is_some() {
                                SkillManagementResult::error("busy", "active turn")
                            } else if request.expected_revision() != Some(skills.catalog.revision())
                            {
                                SkillManagementResult::error("stale_revision", "stale")
                            } else {
                                let before = skills.catalog.revision().to_string();
                                if let SkillManagementRequest::SetEnabled {
                                    name, enabled, ..
                                } = request
                                {
                                    let mut config = skills.catalog.config().clone();
                                    config.rules.push(SkillEnableRule { name, enabled });
                                    skills.catalog = Arc::new(
                                        skills
                                            .catalog
                                            .as_ref()
                                            .clone()
                                            .with_config(config)
                                            .unwrap(),
                                    );
                                }
                                let revision = skills.catalog.revision().to_string();
                                let counts = skills.management_counts();
                                let unchanged = before == revision;
                                if !unchanged {
                                    let _ = events_tx
                                        .send(SessionEvent::SkillsChanged {
                                            revision: revision.clone(),
                                            counts: counts.clone(),
                                        })
                                        .await;
                                }
                                SkillManagementResult::Changed {
                                    revision,
                                    counts,
                                    unchanged,
                                }
                            };
                            let _ = events_tx
                                .send(SessionEvent::SkillsResult { request_id, result })
                                .await;
                            continue;
                        }
                        SessionCommand::Turn(zevria_session_api::TurnCommand::InvokeSkill {
                            name,
                            args,
                            mode,
                        }) => {
                            if let Err(error) =
                                skills.resolve(&name, SkillInvocationOrigin::Explicit)
                            {
                                let _ = events_tx
                                    .send(SessionEvent::TurnRejected {
                                        turn_id: TurnId::new(next_turn),
                                        error: error.to_string(),
                                    })
                                    .await;
                                next_turn += 1;
                                continue;
                            }
                            if let Some(expected) = revise {
                                let artifact = current_artifact
                                    .as_ref()
                                    .filter(|artifact| artifact.version == expected)
                                    .expect("current fake Plan")
                                    .clone();
                                let _ = events_tx
                                    .send(SessionEvent::PlanStateChanged {
                                        state: PlanWorkflowState::Planning {
                                            id: expected.id,
                                            previous: Some(artifact),
                                        },
                                    })
                                    .await;
                            }
                            let snapshot = skills
                                .resolve(&name, SkillInvocationOrigin::Explicit)
                                .unwrap();
                            let (_, prospective) = skills.pins.prepare(snapshot).unwrap();
                            skills.pins = prospective;
                            skill_display = Some(args.with_prefix(format!("${name} ")).trimmed());
                            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                                behavior: zevria_foundation::RequestBehavior::Standard,
                                text: args,
                                mode,
                            })
                        }
                        command => command,
                    };
                    match command {
                        SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                            behavior: _,
                            text,
                            mode,
                        }) => {
                            let turn_id = TurnId::new(next_turn);
                            next_turn += 1;
                            if text.is_blank() {
                                let _ = events_tx
                                    .send(SessionEvent::TurnRejected {
                                        turn_id,
                                        error: "a message turn requires non-empty text".into(),
                                    })
                                    .await;
                                continue;
                            }
                            active = Some(turn_id);
                            selected_mode = mode;
                            let _ = events_tx.send(SessionEvent::ModeChanged { mode }).await;
                            let _ = events_tx
                                .send(SessionEvent::TurnStarted {
                                    turn_id,
                                    message: skill_display
                                        .unwrap_or_else(|| text.clone())
                                        .to_message(),
                                    mode,
                                })
                                .await;
                            if text == zevria_content::UserPrompt::from_text("make plan")
                                && mode == SessionMode::Plan
                            {
                                let artifact = PlanArtifact {
                                    version: PlanVersion {
                                        id: PlanId::new(),
                                        revision: 1,
                                    },
                                    title: "Implement the fake plan".to_string(),
                                    markdown: "# Implement the fake plan\n\n## Goal\nTest ACP Plan handoff.\n\n## Decisions\n- Keep the same session.\n\n## Implementation\n1. Build it.\n\n## Validation\n- Verify the result.\n\n## Risks\n- None.".to_string(),
                                    source_turn_id: turn_id,
                                };
                                let call = ToolCall::new(
                                    ToolCallId::new_or_mint("submit-plan-call"),
                                    ToolFunction::new(
                                        "submit_plan".to_string(),
                                        serde_json::json!({
                                            "title": artifact.title.clone(),
                                            "markdown": artifact.markdown.clone(),
                                        }),
                                    ),
                                );
                                let _ = events_tx
                                    .send(SessionEvent::Intermediate {
                                        display_attempt_id: None,
                                        turn_id,
                                        message: Message::Assistant {
                                            id: None,
                                            content: vec![AssistantContent::ToolCall(call)],
                                        },
                                    })
                                    .await;
                                let _ = events_tx
                                    .send(SessionEvent::ToolResults {
                                        turn_id,
                                        message: Message::tool_result(
                                            "submit-plan-call",
                                            "submit_plan",
                                            "Plan accepted",
                                        ),
                                        metadata: vec![ToolResultMetadata {
                                            diagnostic: None,
                                            id: "submit-plan-call".to_string(),
                                            call_id: None,
                                            tool_name: "submit_plan".to_string(),
                                            outcome: ToolCallOutcome::Success,
                                            detail: None,
                                        }],
                                    })
                                    .await;
                                let _ = events_tx
                                    .send(SessionEvent::PlanStateChanged {
                                        state: PlanWorkflowState::Ready {
                                            artifact: artifact.clone(),
                                        },
                                    })
                                    .await;
                                let _ = events_tx
                                    .send(SessionEvent::TurnCompleted {
                                        display_attempt_id: None,
                                        turn_id,
                                        message: Message::assistant("Plan submitted"),
                                    })
                                    .await;
                                current_artifact = Some(artifact);
                                active = None;
                                continue;
                            }
                            if text == zevria_content::UserPrompt::from_text("wait") {
                                continue;
                            }
                            if text == zevria_content::UserPrompt::from_text("question") {
                                let _ = events_tx
                                    .send(SessionEvent::QuestionAsked {
                                        turn_id,
                                        request: QuestionRequest {
                                            id: QuestionRequestId::new("question-1"),
                                            questions: vec![QuestionPrompt {
                                                id: "scope".to_string(),
                                                header: "Scope".to_string(),
                                                question: "Choose scope".to_string(),
                                                options: vec![QuestionOption {
                                                    label: "Focused".to_string(),
                                                    description: "Keep it narrow".to_string(),
                                                }],
                                                kind: QuestionPromptKind::SingleSelect {
                                                    allow_other: true,
                                                },
                                                required: true,
                                                default: None,
                                            }],
                                            source_label: None,
                                            dismissible: true,
                                        },
                                    })
                                    .await;
                                continue;
                            }
                            events_tx.stream_updated(turn_id, Message::assistant("hel"));
                            events_tx.stream_updated(turn_id, Message::assistant("hello"));
                            let call = ToolCall::new(
                                ToolCallId::new_or_mint("call-1"),
                                ToolFunction::new(
                                    "command".to_string(),
                                    serde_json::json!({"command": "printf ok"}),
                                ),
                            );
                            let _ = events_tx
                                .send(SessionEvent::Intermediate {
                                    display_attempt_id: None,
                                    turn_id,
                                    message: Message::Assistant {
                                        id: None,
                                        content: vec![AssistantContent::ToolCall(call)],
                                    },
                                })
                                .await;
                            let _ = events_tx
                                .send(SessionEvent::ToolResults {
                                    turn_id,
                                    message: Message::tool_result("call-1", "command", "ok"),
                                    metadata: vec![ToolResultMetadata {
                                        diagnostic: None,
                                        id: "call-1".to_string(),
                                        call_id: None,
                                        tool_name: "command".to_string(),
                                        outcome: ToolCallOutcome::Success,
                                        detail: None,
                                    }],
                                })
                                .await;
                            let _ = events_tx
                                .send(SessionEvent::TurnCompleted {
                                    display_attempt_id: None,
                                    turn_id,
                                    message: Message::assistant("hello world"),
                                })
                                .await;
                            active = None;
                        }
                        SessionCommand::Control(
                            zevria_session_api::ControlCommand::AnswerQuestion { response, .. },
                        ) => {
                            answers
                                .lock()
                                .expect("fake answer lock poisoned")
                                .push(response);
                            if let Some(turn_id) = active.take() {
                                let _ = events_tx
                                    .send(SessionEvent::TurnCompleted {
                                        display_attempt_id: None,
                                        turn_id,
                                        message: Message::assistant("question answered"),
                                    })
                                    .await;
                            }
                        }
                        SessionCommand::Control(
                            zevria_session_api::ControlCommand::CancelTurn { .. },
                        ) => {
                            if let Some(turn_id) = active.take() {
                                let _ = events_tx
                                    .send(SessionEvent::TurnCancelled { turn_id })
                                    .await;
                            }
                        }
                        SessionCommand::Turn(zevria_session_api::TurnCommand::ResolvePlan {
                            expected,
                            decision,
                        }) => {
                            let Some(artifact) = current_artifact.clone() else {
                                continue;
                            };
                            if artifact.version != expected {
                                continue;
                            }
                            match decision {
                                PlanDecision::Revise => {
                                    selected_mode = SessionMode::Plan;
                                    let _ = events_tx
                                        .send(SessionEvent::ModeChanged {
                                            mode: selected_mode,
                                        })
                                        .await;
                                    let _ = events_tx
                                        .send(SessionEvent::PlanStateChanged {
                                            state: PlanWorkflowState::Planning {
                                                id: artifact.version.id,
                                                previous: Some(artifact),
                                            },
                                        })
                                        .await;
                                }
                                PlanDecision::ImplementCurrent => {
                                    selected_mode = SessionMode::Build;
                                    let _ = events_tx
                                        .send(SessionEvent::ModeChanged {
                                            mode: selected_mode,
                                        })
                                        .await;
                                    let _ = events_tx
                                        .send(SessionEvent::PlanStateChanged {
                                            state: PlanWorkflowState::Resolved {
                                                artifact: artifact.clone(),
                                                resolution: PlanResolution::ImplementedCurrent,
                                            },
                                        })
                                        .await;
                                    let turn_id = TurnId::new(next_turn);
                                    next_turn += 1;
                                    let handoff =
                                        PlanHandoff::new(artifact, runtime_session_id.clone());
                                    let _ = events_tx
                                        .send(SessionEvent::PlanHandoffStarted {
                                            turn_id,
                                            handoff: handoff.clone(),
                                        })
                                        .await;
                                    let _ = events_tx
                                        .send(SessionEvent::TurnStarted {
                                            turn_id,
                                            message: handoff.prompt,
                                            mode: SessionMode::Build,
                                        })
                                        .await;
                                    let _ = events_tx
                                        .send(SessionEvent::TurnCompleted {
                                            display_attempt_id: None,
                                            turn_id,
                                            message: Message::assistant("implemented plan"),
                                        })
                                        .await;
                                }
                                PlanDecision::ImplementFresh => {}
                            }
                        }
                        SessionCommand::Control(zevria_session_api::ControlCommand::Worker(_)) => {
                            panic!("worker review controls must never enter an ACP child runtime")
                        }
                        SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown) => {
                            break;
                        }
                        SessionCommand::Turn(zevria_session_api::TurnCommand::StartFromPlan {
                            ..
                        })
                        | SessionCommand::Turn(zevria_session_api::TurnCommand::EditTranscript(
                            _,
                        ))
                        | SessionCommand::Turn(zevria_session_api::TurnCommand::InvokeSkill {
                            ..
                        })
                        | SessionCommand::Turn(zevria_session_api::TurnCommand::Compact {
                            ..
                        })
                        | SessionCommand::Turn(zevria_session_api::TurnCommand::RunEnsemble {
                            ..
                        }) => {}
                        SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                            ..
                        })
                        | SessionCommand::Manage(
                            zevria_session_api::ManagementCommand::SetMode { .. },
                        )
                        | SessionCommand::Manage(zevria_session_api::ManagementCommand::Skills {
                            ..
                        })
                        | SessionCommand::Turn(
                            zevria_session_api::TurnCommand::RevisePlanWithSkill { .. },
                        ) => {
                            unreachable!("handled before fake prompt routing")
                        }
                    }
                }
                let _ = exit_tx.send(RuntimeExit::clean("fake runtime"));
            });
            Ok(StartedSession {
                session_id,
                workspace: request.workspace,
                transcript_items,
                selected_mode: self.selected_mode,
                plan_state,
                startup_notices: Vec::new(),
                commands: commands.clone(),
                events,
                background_exit: Box::pin(async move {
                    exit_rx
                        .await
                        .unwrap_or_else(|_| RuntimeExit::failed("fake runtime", "exit dropped"))
                }),
                lifecycle: Box::new(FakeLifecycle {
                    _events: retained_events,
                    commands,
                    task: Some(task),
                    shutdowns: Arc::clone(&self.shutdowns),
                }),
            })
        })
    }

    fn list(
        &self,
        workspace: PathBuf,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Vec<SessionDescriptor>>> + Send + '_>,
    > {
        Box::pin(async move {
            Ok(vec![SessionDescriptor {
                id: "persisted".to_string(),
                workspace,
                modified: SystemTime::now(),
                preview: Some("durable user".to_string()),
            }])
        })
    }
}

struct FakeLifecycle {
    _events: zevria_session_api::SessionEventSender,
    commands: mpsc::UnboundedSender<SessionCommand>,
    task: Option<tokio::task::JoinHandle<()>>,
    shutdowns: Arc<AtomicUsize>,
}

impl SessionRuntimeLifecycle for FakeLifecycle {
    fn shutdown(
        mut self: Box<Self>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
        let _ = self.commands.send(SessionCommand::Control(
            zevria_session_api::ControlCommand::Shutdown,
        ));
        let task = self.task.take();
        let shutdowns = Arc::clone(&self.shutdowns);
        Box::pin(async move {
            if let Some(task) = task {
                task.await?;
            }
            shutdowns.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
    }
}

#[tokio::test]
async fn fatal_runtime_exit_resolves_pending_work_prioritizes_diagnostic_and_shuts_down_once() {
    let workspace = tempfile::tempdir().unwrap();
    let factory = Arc::new(FakeFactory::new());
    let shutdowns = factory.shutdowns.clone();
    let updates = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let log = updates.clone();
    let (client_transport, server_transport) = Channel::duplex();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            factory,
            server_transport,
        )
        .await
    });
    let client = Client.builder()
        .on_receive_notification(async move |notification: SessionNotification, _connection| { log.lock().unwrap().push(notification); Ok(()) }, agent_client_protocol::on_receive_notification!())
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let id = connection.send_request(NewSessionRequest::new(workspace)).block_task().await?.session_id;
                let prompt = connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from("wait")])).block_task();
                let skill = connection.send_request(crate::skills::SkillsListRequest { version: 1, session_id: id.clone(), query: "fatal-runtime".into() }).block_task();
                let (prompt, skill) = tokio::join!(prompt, skill);
                let error = prompt.unwrap_err();
                assert!(error.data.unwrap().to_string().contains("original fatal diagnostic"));
                let result = skill.unwrap().result;
                assert!(matches!(result, zevria_instructions::skill::SkillManagementResult::Error { code, message } if code == "unavailable" && message.contains("original fatal diagnostic")));
                assert!(connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from("later work")])).block_task().await.is_err());
                assert!(connection.send_request(crate::skills::SkillsListRequest { version: 1, session_id: id, query: String::new() }).block_task().await.is_err());
                Ok(())
            }
        });
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(shutdowns.load(Ordering::Acquire), 1);
    let serialized = serde_json::to_string(&*updates.lock().unwrap()).unwrap();
    assert!(serialized.contains("original fatal diagnostic"));
    assert!(!serialized.contains("contradictory success"));
}

#[tokio::test]
async fn in_process_v1_lifecycle_streams_replays_elicits_cancels_and_closes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let factory = Arc::new(FakeFactory::new());
    let permission_requested = Arc::new(AtomicBool::new(false));
    let notifications = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let (client_transport, server_transport) = Channel::duplex();

    let server_factory: Arc<dyn SessionRuntimeFactory> = factory.clone();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            server_factory,
            server_transport,
        )
        .await
    });

    let notification_log = Arc::clone(&notifications);
    let permission_flag = Arc::clone(&permission_requested);
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                notification_log
                    .lock()
                    .expect("notification log poisoned")
                    .push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _connection| {
                permission_flag.store(true, Ordering::Release);
                responder.respond_with_error(
                    agent_client_protocol::schema::v1::Error::internal_error()
                        .data("Zevria must not request ACP permission"),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateElicitationRequest, responder, _connection| {
                responder.respond(CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(BTreeMap::from([(
                        "question_0".to_string(),
                        ElicitationContentValue::String("option_0".to_string()),
                    )])),
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            let notifications = Arc::clone(&notifications);
            async move |connection| {
                let initialized = connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            ClientCapabilities::new().elicitation(
                                ElicitationCapabilities::new()
                                    .form(ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                assert!(initialized.agent_capabilities.load_session);
                assert!(initialized.agent_capabilities.session_capabilities.resume.is_some());
                assert!(initialized.agent_capabilities.session_capabilities.close.is_some());
                assert!(initialized.agent_capabilities.session_capabilities.list.is_some());

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(workspace.clone()))
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(listed.sessions[0].session_id.to_string(), "persisted");

                connection
                    .send_request(LoadSessionRequest::new("persisted", workspace.clone()))
                    .block_task()
                    .await?;
                assert!(notifications
                    .lock()
                    .expect("notification log poisoned")
                    .iter()
                    .any(|notification| matches!(
                        &notification.update,
                        AcpSessionUpdate::UserMessageChunk(chunk)
                            if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "durable user")
                    )));
                connection
                    .send_request(CloseSessionRequest::new("persisted"))
                    .block_task()
                    .await?;

                notifications
                    .lock()
                    .expect("notification log poisoned")
                    .clear();
                connection
                    .send_request(ResumeSessionRequest::new("persisted", workspace.clone()))
                    .block_task()
                    .await?;
                assert!(!notifications
                    .lock()
                    .expect("notification log poisoned")
                    .iter()
                    .any(|notification| matches!(
                        &notification.update,
                        AcpSessionUpdate::UserMessageChunk(_)
                    )));
                connection
                    .send_request(CloseSessionRequest::new("persisted"))
                    .block_task()
                    .await?;

                let created = connection
                    .send_request(NewSessionRequest::new(workspace.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(SetSessionModeRequest::new(
                        created.session_id.clone(),
                        "build",
                    ))
                    .block_task()
                    .await?;
                let completed = connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::from("run")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(completed.stop_reason, StopReason::EndTurn);
                assert!(notifications
                    .lock()
                    .expect("notification log poisoned")
                    .iter()
                    .any(|notification| matches!(
                        notification.update,
                        AcpSessionUpdate::ToolCall(_)
                    )));

                let questioned = connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::from("question")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(questioned.stop_reason, StopReason::EndTurn);

                let waiting = connection.send_request(PromptRequest::new(
                    created.session_id.clone(),
                    vec![ContentBlock::from("wait")],
                ));
                let mut waiting = Box::pin(waiting.block_task());
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                connection.send_notification(CancelNotification::new(created.session_id.clone()))?;
                let cancelled = waiting.as_mut().await?;
                assert_eq!(cancelled.stop_reason, StopReason::Cancelled);

                connection
                    .send_request(CloseSessionRequest::new(created.session_id))
                    .block_task()
                    .await?;
                Ok(())
            }
        });

    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("client timed out")
        .expect("client connection failed");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("server timed out")
        .expect("server task panicked")
        .expect("server connection failed");

    assert!(!permission_requested.load(Ordering::Acquire));
    assert!(factory.shutdowns.load(Ordering::Acquire) >= 3);
    let answers = factory.answers.lock().expect("fake answer lock poisoned");
    assert!(matches!(
        answers.as_slice(),
        [QuestionResponse::Answered { .. }]
    ));
}

#[tokio::test]
async fn root_load_and_resume_seed_plan_mode_and_ready_content_without_startup_events() {
    use zevria_workflow::PlanRecord;
    let artifact = PlanArtifact {
        version: PlanVersion { id: PlanId::new(), revision: 1 },
        title: "Restore Canonical Root Plan".into(),
        markdown: "# Restore Canonical Root Plan\n\n## Goal\nRestore.\n\n## Decisions\nSeed snapshots.\n\n## Implementation\nKeep state.\n\n## Validation\nTest resume.\n\n## Risks\nNone.\n".into(),
        source_turn_id: TurnId::new(1),
    };
    for (records, expected_mode, ready) in [
        (vec![], "build", false),
        (
            vec![PlanRecord::Started {
                id: artifact.version.id,
            }],
            "plan",
            false,
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
                PlanRecord::RevisionRequested {
                    artifact: artifact.clone(),
                },
            ],
            "plan",
            false,
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
            ],
            "plan",
            true,
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
                PlanRecord::Resolved {
                    id: artifact.version.id,
                    artifact: Some(artifact.clone()),
                    resolution: PlanResolution::ImplementedCurrent,
                },
            ],
            "build",
            false,
        ),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let mut factory = FakeFactory::new();
        factory.selected_mode = if expected_mode == "plan" {
            SessionMode::Plan
        } else {
            SessionMode::Build
        };
        Arc::make_mut(&mut factory.persisted).extend(records.into_iter().map(TranscriptItem::Plan));
        let commands = factory.commands.clone();
        let updates = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
        let log = updates.clone();
        let (client_transport, server_transport) = Channel::duplex();
        let server_workspace = workspace.path().to_path_buf();
        let server = tokio::spawn(async move {
            serve_on(
                AcpConfig::default(),
                server_workspace,
                Arc::new(factory),
                server_transport,
            )
            .await
        });
        let command_log = commands.clone();
        let expected = artifact.clone();
        let client = Client.builder()
            .on_receive_notification(async move |notification: SessionNotification, _| { log.lock().unwrap().push(notification); Ok(()) }, agent_client_protocol::on_receive_notification!())
            .connect_with(client_transport, {
                let workspace = workspace.path().to_path_buf();
                async move |connection| {
                    connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                    let loaded = connection.send_request(LoadSessionRequest::new("persisted", workspace.clone())).block_task().await?;
                    assert_eq!(loaded.modes.unwrap().current_mode_id.to_string(), expected_mode);
                    connection.send_request(CloseSessionRequest::new("persisted")).block_task().await?;
                    let resumed = connection.send_request(ResumeSessionRequest::new("persisted", workspace)).block_task().await?;
                    assert_eq!(resumed.modes.unwrap().current_mode_id.to_string(), expected_mode);
                    assert!(command_log.lock().unwrap().iter().all(|command| matches!(command, SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown))), "restoration must not allocate a turn or request approval");
                    if ready {
                        // The retained artifact is immediately actionable without
                        // waiting for an engine startup snapshot.
                        let response = connection.send_request(PromptRequest::new("persisted", vec![ContentBlock::from("/implement")])).block_task().await?;
                        assert_eq!(response.stop_reason, StopReason::EndTurn);
                        assert!(command_log.lock().unwrap().iter().any(|command| matches!(command, SessionCommand::Turn(zevria_session_api::TurnCommand::ResolvePlan { expected: version, decision: PlanDecision::ImplementCurrent }) if *version == expected.version)));
                    }
                    connection.send_request(CloseSessionRequest::new("persisted")).block_task().await?;
                    Ok(())
                }
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let updates = updates.lock().unwrap();
        assert_eq!(updates.iter().filter(|notification| matches!(&notification.update,
            AcpSessionUpdate::AgentMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Text(text) if text.text == artifact.markdown)
        )).count(), if ready { 2 } else { 0 }, "one host-driven Ready publication for each load/resume");
        if !ready {
            assert!(commands.lock().unwrap().iter().all(|command| matches!(
                command,
                SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown)
            )));
        }
    }
}

#[tokio::test]
async fn session_limit_text_validation_and_concurrent_prompt_guards_are_enforced() {
    let workspace = tempfile::tempdir().expect("workspace");
    let factory = Arc::new(FakeFactory::new());
    let (client_transport, server_transport) = Channel::duplex();
    let server_factory: Arc<dyn SessionRuntimeFactory> = factory;
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig {
                max_sessions: 1,
                expose_session_list: true,
            },
            server_workspace,
            server_factory,
            server_transport,
        )
        .await
    });

    let client = Client
        .builder()
        .on_receive_notification(
            async move |_notification: SessionNotification, _connection| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let first = connection
                    .send_request(NewSessionRequest::new(workspace.clone()))
                    .block_task()
                    .await?;
                let limit_error = connection
                    .send_request(NewSessionRequest::new(workspace.clone()))
                    .block_task()
                    .await
                    .expect_err("second active session must exceed the configured limit");
                assert!(limit_error.to_string().contains("session limit"));

                let blank_error = connection
                    .send_request(PromptRequest::new(
                        first.session_id.clone(),
                        vec![ContentBlock::from("   ")],
                    ))
                    .block_task()
                    .await
                    .expect_err("blank prompt must fail");
                assert!(blank_error.to_string().contains("non-empty text"));
                let image_error = connection
                    .send_request(PromptRequest::new(
                        first.session_id.clone(),
                        vec![ContentBlock::Image(ImageContent::new("data", "image/png"))],
                    ))
                    .block_task()
                    .await
                    .expect_err("malformed image must fail");
                assert!(
                    image_error
                        .to_string()
                        .contains("image content does not match")
                );

                let waiting = connection.send_request(PromptRequest::new(
                    first.session_id.clone(),
                    vec![ContentBlock::from("wait")],
                ));
                let mut waiting = Box::pin(waiting.block_task());
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let duplicate = connection
                    .send_request(PromptRequest::new(
                        first.session_id.clone(),
                        vec![ContentBlock::from("second")],
                    ))
                    .block_task()
                    .await
                    .expect_err("concurrent prompt must fail");
                assert!(duplicate.to_string().contains("in-flight prompt"));
                connection.send_notification(CancelNotification::new(first.session_id.clone()))?;
                assert_eq!(waiting.as_mut().await?.stop_reason, StopReason::Cancelled);
                connection
                    .send_request(CloseSessionRequest::new(first.session_id))
                    .block_task()
                    .await?;

                let second = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new(second.session_id))
                    .block_task()
                    .await?;
                Ok(())
            }
        });

    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("guard client timed out")
        .expect("guard client failed");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("guard server timed out")
        .expect("guard server task panicked")
        .expect("guard server failed");
}

#[tokio::test]
async fn plan_form_implementation_keeps_the_original_prompt_pending_through_build() {
    let workspace = tempfile::tempdir().expect("workspace");
    let factory = Arc::new(FakeFactory::new());
    let notifications = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let (client_transport, server_transport) = Channel::duplex();
    let server_factory: Arc<dyn SessionRuntimeFactory> = factory;
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            server_factory,
            server_transport,
        )
        .await
    });

    let notification_log = Arc::clone(&notifications);
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                notification_log
                    .lock()
                    .expect("notification log poisoned")
                    .push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_request: CreateElicitationRequest, responder, _connection| {
                responder.respond(CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(BTreeMap::from([(
                        "decision".to_string(),
                        ElicitationContentValue::String("implement_current".to_string()),
                    )])),
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            ClientCapabilities::new().elicitation(
                                ElicitationCapabilities::new()
                                    .form(ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                connection
                    .send_request(SetSessionModeRequest::new(
                        created.session_id.clone(),
                        "plan",
                    ))
                    .block_task()
                    .await?;
                let response = connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::from("make plan")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                connection
                    .send_request(CloseSessionRequest::new(created.session_id))
                    .block_task()
                    .await?;
                Ok(())
            }
        });

    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("plan client timed out")
        .expect("plan client failed");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("plan server timed out")
        .expect("plan server task panicked")
        .expect("plan server failed");

    let notifications = notifications.lock().expect("notification log poisoned");
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        AcpSessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.starts_with("# Implement the fake plan"))
    )));
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        AcpSessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("implemented plan"))
    )));
}

#[tokio::test]
async fn plan_without_elicitation_waits_for_exact_implement_command() {
    let workspace = tempfile::tempdir().expect("workspace");
    let factory = Arc::new(FakeFactory::new());
    let notifications = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let (client_transport, server_transport) = Channel::duplex();
    let server_factory: Arc<dyn SessionRuntimeFactory> = factory;
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            server_factory,
            server_transport,
        )
        .await
    });

    let notification_log = Arc::clone(&notifications);
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                notification_log
                    .lock()
                    .expect("notification log poisoned")
                    .push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            let notifications = Arc::clone(&notifications);
            async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                connection
                    .send_request(SetSessionModeRequest::new(
                        created.session_id.clone(),
                        "plan",
                    ))
                    .block_task()
                    .await?;
                let planned = connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::from("make plan")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(planned.stop_reason, StopReason::EndTurn);
                assert!(!notifications
                    .lock()
                    .expect("notification log poisoned")
                    .iter()
                    .any(|notification| matches!(
                        &notification.update,
                        AcpSessionUpdate::AgentMessageChunk(chunk)
                            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("implemented plan"))
                    )));

                let implemented = connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::from("/implement")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(implemented.stop_reason, StopReason::EndTurn);
                connection
                    .send_request(CloseSessionRequest::new(created.session_id))
                    .block_task()
                    .await?;
                Ok(())
            }
        });

    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("fallback client timed out")
        .expect("fallback client failed");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("fallback server timed out")
        .expect("fallback server task panicked")
        .expect("fallback server failed");
    assert!(notifications
        .lock()
        .expect("notification log poisoned")
        .iter()
        .any(|notification| matches!(
            &notification.update,
            AcpSessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("implemented plan"))
        )));
}
