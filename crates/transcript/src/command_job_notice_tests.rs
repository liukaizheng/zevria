use crate::*;
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobNotice, CommandJobNoticeKind, CommandJobSnapshot,
    CommandJobStatus,
};

fn notice(kind: CommandJobNoticeKind) -> TranscriptItem {
    TranscriptItem::CommandJobNotice(CommandJobNotice::new(
        kind,
        vec![CommandJobSnapshot {
            id: CommandJobId::new("job-test"),
            call_id: None,
            command: "do not re-render me".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Exited {
                code: Some(1),
                success: false,
                signal: None,
            },
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
            stdout_bytes: 5,
            stderr_bytes: 0,
        }],
        vec![],
        "Command job update:  exact persisted bytes\r\n<stdout>\nraw é  \n</stdout>\n".into(),
    ))
}

#[test]
fn notice_roundtrip_preserves_exact_text_and_is_conversation_not_prompt() {
    let item = notice(CommandJobNoticeKind::Update);
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("model_message"));
    let loaded: TranscriptItem = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded, item);
    assert_eq!(loaded.message(), item.message());
    assert_eq!(loaded.display_message().as_ref(), item.message());
    assert_eq!(
        loaded.model_request_item().unwrap().message_ref(),
        item.message()
    );
    assert!(is_conversation_item(&loaded));
    assert!(!is_prompt_item(&loaded));
    assert!(!is_compaction_prompt_item(&loaded));
    assert!(retained_user_candidates(std::slice::from_ref(&loaded)).is_empty());
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation.push_required(item.clone()).unwrap();
    assert_eq!(load(conversation.path()).unwrap(), vec![item]);
    assert_eq!(conversation.prompt_position(0), None);
}

#[test]
fn notice_envelope_rejects_nonstandalone_duplicate_and_invalid_fields() {
    let json = serde_json::to_value(notice(CommandJobNoticeKind::Update)).unwrap();
    let envelope = &json["zevria_command_job_notice"];
    for record in [
        format!(
            "{{\"zevria_command_job_notice\":{envelope},\"role\":\"user\",\"content\":\"mirror\"}}"
        ),
        format!(
            "{{\"zevria_command_job_notice\":{envelope},\"zevria_command_job_notice\":{envelope}}}"
        ),
        format!(
            "{{\"zevria_command_job_notice\":{}}}",
            envelope
                .to_string()
                .replacen("\"version\":1", "\"version\":1,\"version\":1", 1)
        ),
    ] {
        assert!(
            serde_json::from_str::<TranscriptItem>(&record).is_err(),
            "{record}"
        );
    }
    // The tool-result decoder runs before the notice decoder. A reserved
    // notice must not be swallowed as an ignored Rig message field there.
    let mut combined = serde_json::to_value(TranscriptItem::ToolResults {
        message: Message::tool_result(
            CallId::from_wire("call"),
            ToolName::new("command").unwrap(),
            "done",
        ),
        metadata: vec![],
        skill_applications: vec![],
    })
    .unwrap();
    combined["zevria_command_job_notice"] = envelope.clone();
    assert!(serde_json::from_value::<TranscriptItem>(combined).is_err());
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("jobs", serde_json::json!([])),
        ("text", serde_json::json!("  ")),
    ] {
        let mut invalid = json.clone();
        invalid["zevria_command_job_notice"][field] = value;
        assert!(serde_json::from_value::<TranscriptItem>(invalid).is_err());
    }
}

#[test]
fn continuation_owns_only_a_standard_boundary_and_updates_are_not_owners() {
    let boundary = |behavior| {
        TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(
            RequestMetadata::new(behavior),
        ))
    };
    assert!(
        InstructionReplayState::replay(&[
            notice(CommandJobNoticeKind::Continuation),
            boundary(RequestBehavior::Standard)
        ])
        .is_ok()
    );
    assert!(
        InstructionReplayState::replay(&[
            notice(CommandJobNoticeKind::Continuation),
            boundary(RequestBehavior::Orchestrate)
        ])
        .is_err()
    );
    assert!(
        InstructionReplayState::replay(&[
            notice(CommandJobNoticeKind::Update),
            boundary(RequestBehavior::Standard)
        ])
        .is_err()
    );
}

#[test]
fn notices_cannot_split_batches_and_survive_checkpoint_suffix_projection() {
    let call = TranscriptItem::Message(Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire("call"),
            ToolFunction::new(
                ToolName::new("command").unwrap(),
                serde_json::json!({"command":"test"}),
            ),
        )),
    ])));
    assert!(
        InstructionReplayState::replay(&[call.clone(), notice(CommandJobNoticeKind::Update)])
            .is_err()
    );
    let result = TranscriptItem::ToolResults {
        message: Message::tool_result(
            CallId::from_wire("call"),
            ToolName::new("command").unwrap(),
            "started",
        ),
        metadata: vec![],
        skill_applications: vec![],
    };
    let checkpoint = TranscriptItem::Compaction(
        CompactionCheckpoint::new(
            CompactionTrigger::AutomaticMidTurn,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user("summary"))],
            vec![],
        )
        .unwrap(),
    );
    let update = notice(CommandJobNoticeKind::Update);
    let items = vec![call, result, checkpoint, update.clone()];
    assert!(InstructionReplayState::replay(&items).is_ok());
    let input = model_input(&items);
    assert_eq!(input.last().unwrap().message_ref(), update.message());
}
