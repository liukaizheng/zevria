use super::*;
use crate::{
    CompactionBackend, CompactionTrigger, ModelProfileRef, OwnedModelRequestItem, PlanId,
    SessionMode,
};
use serde_json::json;

fn selections() -> SessionModels {
    SessionModels::new(
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("Provider/with:separators", "Model:A/B"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("other", "Model:A/B"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap()
}

fn header() -> String {
    serde_json::to_string(&TranscriptItem::SessionModels(selections())).unwrap()
}

fn mode() -> String {
    serde_json::to_string(&TranscriptItem::SessionMode(SessionMode::Build)).unwrap()
}

#[test]
fn selection_roundtrip_is_exact_and_not_conversation_content() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation.replace_session_models(selections()).unwrap();
    assert!(conversation.model_input().is_empty());
    assert!(conversation.retained_user_candidates().is_empty());
    assert_eq!(conversation.prompt_position(0), None);
    let item = &conversation.items()[0];
    assert!(
        item.message().is_none()
            && item.provider_replay().is_none()
            && item.model_request_item().is_none()
    );
    assert!(!is_prompt_item(item) && !is_compaction_prompt_item(item));
    let loaded = load_report(conversation.path()).unwrap();
    assert_eq!(loaded.session_models().unwrap(), Some(&selections()));
    assert_eq!(loaded.items, conversation.items());
    assert!(is_abandoned_root(conversation.path()));
    assert!(list_sessions(directory.path()).unwrap().is_empty());
    assert_eq!(latest_session_file(directory.path()).unwrap(), None);
}

#[test]
fn complete_reasoning_selections_roundtrip_without_model_input() {
    use zevria_foundation::ReasoningLevel as Level;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&header()).unwrap(),
        json!({"zevria_session_models":{"version":1,"build":{"profile":{"provider":"Provider/with:separators","model":"Model:A/B"},"reasoning_level":"medium"},"plan":{"profile":{"provider":"other","model":"Model:A/B"},"reasoning_level":"medium"}}})
    );
    for saved in [
        selections(),
        selections().with_reasoning(SessionMode::Build, Level::High),
        selections()
            .with_reasoning(SessionMode::Build, Level::High)
            .with_reasoning(SessionMode::Plan, Level::Low),
    ] {
        let bytes = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionModels>(&bytes).unwrap(),
            saved
        );
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = Conversation::new(TranscriptWriter::create(dir.path()).unwrap());
        conversation.replace_session_models(saved.clone()).unwrap();
        assert!(conversation.model_input().is_empty());
        assert_eq!(
            load_report(conversation.path())
                .unwrap()
                .session_models()
                .unwrap(),
            Some(&saved)
        );
    }
    let saved = selections()
        .with_reasoning(SessionMode::Build, Level::High)
        .with_reasoning(SessionMode::Plan, Level::Low);
    let same = saved
        .with_selection(
            SessionMode::Build,
            saved.for_mode(SessionMode::Build).clone(),
        )
        .unwrap();
    assert_eq!(same, saved);
    let changed = saved
        .with_selection(
            SessionMode::Build,
            zevria_model::models::ModelSelection::new(
                ModelProfileRef::new("new", "model"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
    assert_eq!(
        changed.reasoning_for_mode(SessionMode::Build),
        Level::Medium
    );
    assert_eq!(changed.reasoning_for_mode(SessionMode::Plan), Level::Low);
    let cleared = saved
        .with_reasoning(SessionMode::Build, Level::Medium)
        .with_reasoning(SessionMode::Plan, Level::Medium);
    assert_eq!(
        serde_json::to_string(&TranscriptItem::SessionModels(cleared)).unwrap(),
        header()
    );
    for extra in [
        r#""reasoning":{"review":"high"}"#,
        r#""reasoning":{"build":"unknown"}"#,
    ] {
        let invalid = serde_json::to_string(&selections()).unwrap().replacen(
            "\"version\":1",
            &format!("\"version\":1,{extra}"),
            1,
        );
        assert!(serde_json::from_str::<SessionModels>(&invalid).is_err());
    }
}

#[test]
fn invalid_selection_metadata_is_never_legacy_or_tail_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.jsonl");
    let good = header();
    let valid: serde_json::Value = serde_json::from_str(&good).unwrap();
    let mut records = vec![
        format!("{good}\n{good}\n"),
        format!("{{\"error\":\"earlier\"}}\n{good}\n"),
        format!("\n{good}\n"),
        "{\"zevria_session_models\":".into(),
        "{\"zevria_session_models_v999\":{}}\n".into(),
        good.replace("\"version\":1", "\"version\":1,\"version\":1"),
        format!(
            "{{\"zevria_session_models\":{},\"zevria_session_models\":{}}}",
            valid[SESSION_MODELS_KEY], valid[SESSION_MODELS_KEY]
        ),
    ];
    for field in ["build", "plan", "version"] {
        let mut value = valid.clone();
        value[SESSION_MODELS_KEY]
            .as_object_mut()
            .unwrap()
            .remove(field);
        records.push(value.to_string());
    }
    for role in ["build", "plan"] {
        for field in ["provider", "model"] {
            for invalid in [json!(" \t"), json!(null), json!(4)] {
                let mut value = valid.clone();
                value[SESSION_MODELS_KEY][role]["profile"][field] = invalid;
                records.push(value.to_string());
            }
        }
    }
    for version in [json!(0), json!(2), json!(99), json!("1"), json!(null)] {
        let mut value = valid.clone();
        value[SESSION_MODELS_KEY]["version"] = version;
        records.push(value.to_string());
    }
    for role in ["build", "plan"] {
        let mut value = valid.clone();
        value[SESSION_MODELS_KEY][role]
            .as_object_mut()
            .unwrap()
            .remove("reasoning_level");
        records.push(value.to_string());
        let mut value = valid.clone();
        value[SESSION_MODELS_KEY][role]["reasoning_level"] = json!("unknown");
        records.push(value.to_string());
    }
    records.push(r#"{"zevria_session_models":{"version":1,"build":{"provider":"p","model":"m"},"plan":{"provider":"p","model":"m"}}}"#.into());
    let mut mixed = valid;
    mixed["error"] = json!("not metadata-only");
    records.push(mixed.to_string());
    for original in records {
        std::fs::write(&path, &original).unwrap();
        assert!(load_report(&path).is_err(), "{original}");
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!is_abandoned_root(&path));
        assert_eq!(list_sessions(dir.path()).unwrap().len(), 1);
    }
}

#[test]
fn valid_selections_do_not_authorize_partial_restoration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.jsonl");
    for tail in [
        "{\"zevria_skill_activation_v999\":{}}\n",
        "broken\n",
        "\u{a0}\n",
        "{\"partial\":",
    ] {
        let original = format!("{}\n{}\n{tail}", header(), mode());
        std::fs::write(&path, &original).unwrap();
        if tail == "{\"partial\":" {
            let outcome = load_report(&path).unwrap();
            assert_eq!(outcome.session_models().unwrap(), Some(&selections()));
        } else {
            assert!(load_report(&path).is_err());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!is_abandoned_root(&path));
        assert_eq!(list_sessions(dir.path()).unwrap().len(), 1);
    }
    // Marker-like message strings are ordinary data in child transcripts.
    let ordinary = TranscriptItem::Message(Message::user("{\"zevria_session_models\": invalid}"));
    std::fs::write(
        &path,
        format!("{}\n{}", mode(), serde_json::to_string(&ordinary).unwrap()),
    )
    .unwrap();
    let outcome = load_report(&path).unwrap();
    assert!(!outcome.blocked);
    assert_eq!(outcome.session_models().unwrap(), None);
}

#[test]
fn edits_compaction_and_repair_preserve_current_not_creation_selections() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(dir.path()).unwrap());
    conversation.replace_session_models(selections()).unwrap();
    conversation
        .push_required(TranscriptItem::Message(Message::user("old prompt")))
        .unwrap();
    let current = selections()
        .with_selection(
            SessionMode::Plan,
            zevria_model::models::ModelSelection::new(
                ModelProfileRef::new("New", "chosen/later"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
    conversation
        .replace_session_models(current.clone())
        .unwrap();
    assert_eq!(conversation.prompt_position(0), Some(1));
    conversation
        .replace_from(
            1,
            TranscriptItem::Message(Message::user("edited early prompt")),
        )
        .unwrap();
    let checkpoint = CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user("summary"))],
        vec![],
    )
    .unwrap();
    conversation
        .push_required(TranscriptItem::Compaction(checkpoint))
        .unwrap();
    conversation
        .replace_from_items(
            0,
            vec![TranscriptItem::Plan(PlanRecord::Started {
                id: PlanId::new(),
            })],
        )
        .unwrap();
    assert_eq!(conversation.session_models(), Some(&current));
    conversation.truncate(0).unwrap();
    assert_eq!(conversation.items().len(), 1);
    conversation
        .push_required(TranscriptItem::Message(Message::user("current prompt")))
        .unwrap();
    assert_eq!(
        conversation.items()[0],
        TranscriptItem::SessionModels(current.clone())
    );
    // A failed append's in-memory completed work must repair with this header.
    conversation.persistence_error = Some("injected partial append".into());
    conversation.ensure_durable().unwrap();
    let path = conversation.path().to_path_buf();
    drop(conversation);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    let writer = TranscriptWriter::append_to(path.clone()).unwrap();
    assert_eq!(writer.recovered_malformed_lines(), 1);
    assert_eq!(
        load_report(&path).unwrap().session_models().unwrap(),
        Some(&current)
    );
}

#[test]
fn failed_selection_replacement_leaves_memory_and_original_bytes_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(dir.path()).unwrap());
    conversation.replace_session_models(selections()).unwrap();
    let path = conversation.path().to_path_buf();
    let original = std::fs::read(&path).unwrap();
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    let changed = selections()
        .with_selection(
            SessionMode::Build,
            zevria_model::models::ModelSelection::new(
                ModelProfileRef::new("p", "new"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
    assert!(conversation.replace_session_models(changed).is_err());
    assert_eq!(conversation.session_models(), Some(&selections()));
    assert_eq!(std::fs::read(saved).unwrap(), original);
}
