use super::*;
use zevria_content::web_search::RESPONSE_DISPLAY_META_KEY;
use zevria_content::web_search::ResponseDisplay;

pub(super) fn display_with_terminal_evidence() -> ResponseDisplay {
    let mut attempt = zevria_content::WebSearchAttemptRecord::new(
        zevria_foundation::ModelProfileRef::new("p", "m"),
    );
    attempt.activity.push(zevria_content::WebSearchActivity {
        output_index: 1,
        item_id: Some("search".into()),
        status: zevria_content::WebSearchStatus::Completed,
        action: None,
    });
    attempt
        .terminal
        .insert(1, zevria_content::WebSearchStatus::Completed);
    let tool_call_id = attempt.activity[0].client_id(&attempt.id);
    ResponseDisplay {
        version: 1,
        attempt,
        bindings: vec![zevria_content::web_search::DisplayProjectionBinding::Tool {
            tool_call_id,
            output_index: 1,
            native: false,
        }],
    }
}

fn metadata() -> serde_json::Map<String, serde_json::Value> {
    let display = display_with_terminal_evidence();
    let mut meta = serde_json::Map::new();
    meta.insert(
        RESPONSE_DISPLAY_META_KEY.into(),
        serde_json::to_value(display).unwrap(),
    );
    meta
}

#[test]
fn terminal_evidence_round_trips_through_display_and_event() {
    let display = display_with_terminal_evidence();
    display.validate().unwrap();
    assert_eq!(
        display.attempt.outcome,
        zevria_content::WebSearchAttemptOutcome::InProgress
    );
    let bytes = serde_json::to_vec(&display).unwrap();
    assert_eq!(
        display,
        serde_json::from_slice::<ResponseDisplay>(&bytes).unwrap()
    );

    let event = AgentRunEvent::ResponseDisplay {
        display: Box::new(display),
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    assert_eq!(
        event,
        serde_json::from_slice::<AgentRunEvent>(&bytes).unwrap()
    );
}

#[test]
fn terminal_evidence_round_trips_through_worker_record_bytes() {
    let record = AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::ResponseDisplay {
            display: Box::new(display_with_terminal_evidence()),
        },
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        record,
        serde_json::from_slice::<AgentRunTranscriptRecord>(&bytes).unwrap()
    );
}

#[test]
fn notification_metadata_is_typed_persistable_and_not_a_workflow_or_report_event() {
    let notification = SessionNotification::new(
        SessionId::new("session"),
        AcpSessionUpdate::SessionInfoUpdate(
            agent_client_protocol::schema::v1::SessionInfoUpdate::new(),
        ),
    )
    .meta(metadata());
    let wire = serde_json::to_vec(&notification).unwrap();
    let notification = serde_json::from_slice(&wire).unwrap();
    let events = normalize_notification(notification);
    assert_eq!(
        events.len(),
        1,
        "the empty metadata carrier must not create a text boundary"
    );
    let AgentRunEvent::ResponseDisplay { display } = &events[0] else {
        panic!("typed display event")
    };
    display.validate().unwrap();
    assert!(!run_log_event_requires_sync(&events[0]));
    assert!(
        !events[0].is_hosted_search_activity(),
        "metadata is not a tool"
    );
    let record = AgentRunTranscriptRecord::Event {
        event: events[0].clone(),
    };
    let decoded: AgentRunTranscriptRecord =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    assert_eq!(record, decoded);
}

#[test]
fn terminal_evidence_persists_through_streaming_projection_and_reopen_without_review_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker.jsonl");
    let header = AgentRunTranscriptHeader {
        version: AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        descriptor: AgentRunDescriptor {
            id: AgentRunId::new(),
            agent: "test".into(),
            label: "Test".into(),
            safe_mode: "read-only".into(),
        },
        prompt: "synthetic prompt".into(),
    };
    let input = WorkerInput {
        generation: 1,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::Initial,
        text: header.prompt.clone(),
    };
    let mut records = vec![AgentRunTranscriptRecord::Header {
        header: header.clone(),
    }];
    for event in [
        AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::InputAccepted { input }),
        },
        AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            }),
        },
        AgentRunEvent::Prompt {
            text: "synthetic prompt".into(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: Some("# Proposal".into()),
                entries: vec![],
            },
        },
        AgentRunEvent::Elicitation {
            field_count: 1,
            outcome: AgentElicitationOutcome::Accepted,
            decision_unavailable: None,
            decision: Some(AgentUserDecisionBatch {
                request_id: QuestionRequestId::new("request"),
                answers: vec![AgentUserDecisionAnswer {
                    decision_id: AgentUserDecisionId::from_question(
                        &QuestionRequestId::new("request"),
                        "scope",
                    ),
                    question_id: "scope".into(),
                    header: "Scope".into(),
                    question: "Which scope?".into(),
                    answer: AgentUserDecisionValue::String {
                        value: "current".into(),
                    },
                }],
            }),
        },
        AgentRunEvent::AgentMessage {
            text: "ordinary ".into(),
            message_id: None,
        },
        AgentRunEvent::AgentMessage {
            text: "report".into(),
            message_id: None,
        },
    ] {
        records.push(AgentRunTranscriptRecord::Event { event });
    }
    let expected = AgentRunProjection::from_records(&records);
    assert_eq!(expected.report, "ordinary report");
    assert_eq!(expected.user_decisions.len(), 1);
    assert!(expected.review.as_ref().unwrap().state.active.is_some());
    records.insert(
        records.len() - 1,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::ResponseDisplay {
                display: Box::new(display_with_terminal_evidence()),
            },
        },
    );
    let mut writer = AgentRunTranscriptWriter::create(path.clone(), header).unwrap();
    for record in &records[1..] {
        writer.append(record).unwrap();
    }
    drop(writer);
    let original = std::fs::read(&path).unwrap();
    let restored = zevria_transcript::AgentRunTranscriptReader::open(&path)
        .unwrap()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(restored, records);
    assert_eq!(load_agent_run_projection(&path).unwrap(), expected);
    assert_eq!(AgentRunProjection::from_records(&restored), expected);
    drop(AgentRunTranscriptWriter::append_to(path.clone()).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(load_agent_run_projection(&path).unwrap(), expected);
}

#[test]
fn malformed_and_unsupported_metadata_preserve_ordinary_updates() {
    for invalid in [
        serde_json::json!({"version":999}),
        serde_json::Value::String("not metadata".into()),
    ] {
        let update = AcpSessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("ordinary answer"),
                ),
            ),
        );
        let expected = normalize_update(update.clone());
        let mut meta = metadata();
        meta.insert(RESPONSE_DISPLAY_META_KEY.into(), invalid);
        assert_eq!(
            normalize_notification(
                SessionNotification::new(SessionId::new("session"), update).meta(meta)
            ),
            expected
        );
    }
}

#[test]
fn valid_metadata_does_not_remove_unrelated_titles_text_or_tools() {
    let update =
        AcpSessionUpdate::AgentMessageChunk(agent_client_protocol::schema::v1::ContentChunk::new(
            agent_client_protocol::schema::v1::ContentBlock::Text(
                agent_client_protocol::schema::v1::TextContent::new("ordinary answer"),
            ),
        ));
    let expected = normalize_update(update.clone());
    let normalized = normalize_notification(
        SessionNotification::new(SessionId::new("session"), update).meta(metadata()),
    );
    assert_eq!(&normalized[..1], &expected);
    assert!(matches!(
        normalized[1],
        AgentRunEvent::ResponseDisplay { .. }
    ));
}
