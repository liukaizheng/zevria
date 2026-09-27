use crate::*;
use rig_core::message::{AssistantContent, Message, ToolCall, ToolCallId, ToolFunction};
use zevria_model::maintenance::validate_maintenance_input;

fn snapshot() -> SkillSnapshot {
    SkillSnapshot::new(
        "review".parse().unwrap(),
        "Review",
        "Exact body\nsecond line",
    )
    .unwrap()
}
fn invocation() -> TranscriptItem {
    let skill = snapshot();
    TranscriptItem::SkillInvocation(SkillInvocation::new(
        skill.name().clone(),
        "inspect",
        SkillApplication::Activate(skill),
    ))
}
fn body() -> TranscriptItem {
    TranscriptItem::Directive(DirectiveContent::skill(&snapshot()))
}
fn revoke() -> TranscriptItem {
    TranscriptItem::Directive(
        DirectiveContent::new(DirectivePayload::SkillRevocation {
            name: snapshot().name().clone(),
            reason: "disabled".into(),
        })
        .unwrap(),
    )
}
fn header() -> TranscriptItem {
    TranscriptItem::SessionMode(SessionMode::Build)
}
fn checkpoint() -> TranscriptItem {
    TranscriptItem::Compaction(
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user("summary"))],
            vec![],
        )
        .unwrap(),
    )
}
fn call() -> TranscriptItem {
    TranscriptItem::Message(Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall::new(
            ToolCallId::new_or_mint("pending"),
            ToolFunction::new("command".into(), serde_json::json!({})),
        ))],
    })
}
fn result() -> TranscriptItem {
    TranscriptItem::ToolResults {
        message: Message::tool_result("pending", "command", "done"),
        metadata: vec![],
        skill_applications: vec![],
    }
}

#[test]
fn skill_directives_and_provider_content_round_trip_and_reject_tampering() {
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    let mut items = vec![header(), invocation()];
    for item in [body(), revoke()] {
        let value = serde_json::to_value(&item).unwrap();
        let record = &value["zevria_skill_directive"];
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_eq!(record.as_object().unwrap().len(), 2);
        assert_eq!(
            record["version"],
            zevria_instructions::directive::INSTRUCTION_VERSION
        );
        assert!(record.get("text").is_none());
        let restored: TranscriptItem = serde_json::from_value(value).unwrap();
        assert_eq!(restored, item);
        items.push(item.clone());
        writer.rewrite(&items).unwrap();
        let loaded = load(writer.path()).unwrap();
        assert_eq!(loaded, items);
        assert_eq!(
            replay_directives(&loaded).unwrap(),
            replay_directives(&items).unwrap()
        );
        assert!(item.message().is_none() && item.provider_replay().is_none());
        let owned = item.model_request_item().unwrap().to_owned_item().unwrap();
        let restored: OwnedModelRequestItem =
            serde_json::from_slice(&serde_json::to_vec(&owned).unwrap()).unwrap();
        assert_eq!(restored.as_borrowed(), item.model_request_item().unwrap());
        let TranscriptItem::Directive(mut directive) = item else {
            unreachable!()
        };
        directive.text.push_str("tampered");
        assert!(directive.validate().is_err());
        assert!(serde_json::to_value(TranscriptItem::Directive(directive.clone())).is_err());
        assert!(
            ModelRequestItem::DeveloperInstruction(&directive)
                .to_owned_item()
                .is_err()
        );
        assert!(
            serde_json::to_value(OwnedModelRequestItem::DeveloperInstruction(
                directive.clone()
            ))
            .is_err()
        );
        assert!(
            serde_json::from_value::<OwnedModelRequestItem>(
                serde_json::json!({"type":"developer_instruction", "text":directive})
            )
            .is_err()
        );
    }
    let raw = Message::System {
        content: "untyped authority".into(),
    };
    assert!(ModelRequestItem::message(&raw).to_owned_item().is_err());
    assert!(serde_json::to_value(OwnedModelRequestItem::message(raw)).is_err());
    assert!(
        serde_json::from_value::<TranscriptItem>(
            serde_json::json!({"role":"system","content":"raw"})
        )
        .is_err()
    );
}

#[test]
fn persisted_directive_bodies_and_digests_must_match_their_full_pins() {
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    let different = SkillSnapshot::new(
        snapshot().name().clone(),
        "Different metadata",
        snapshot().body(),
    )
    .unwrap();
    for (field, changed) in [
        ("body", serde_json::json!("Tampered canonical body")),
        ("digest", serde_json::to_value(different.digest()).unwrap()),
    ] {
        let mut value = serde_json::to_value(body()).unwrap();
        value["zevria_skill_directive"]["payload"][field] = changed;
        let directive: TranscriptItem = serde_json::from_value(value).unwrap();
        let items = vec![invocation(), directive];
        let error = replay_directives(&items).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no matching full pinned activation")
        );
        writer.rewrite(&items).unwrap();
        let bytes = std::fs::read(writer.path()).unwrap();
        assert!(load(writer.path()).is_err());
        assert!(TranscriptWriter::append_to(writer.path().to_path_buf()).is_err());
        assert_eq!(std::fs::read(writer.path()).unwrap(), bytes);
    }
}

#[test]
fn unsupported_directive_envelopes_are_never_repaired_away() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid-directive.jsonl");
    let leading = serde_json::to_string(&invocation()).unwrap();
    let valid = serde_json::to_value(body()).unwrap();
    let mut old_version = valid.clone();
    old_version["zevria_skill_directive"]["version"] = serde_json::json!(2);
    let error = serde_json::from_value::<TranscriptItem>(old_version.clone()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsupported directive version; start a fresh session")
    );
    let mut previous_version = valid.clone();
    previous_version["zevria_skill_directive"]["version"] = serde_json::json!(4);
    assert!(
        serde_json::from_value::<TranscriptItem>(previous_version.clone())
            .unwrap_err()
            .to_string()
            .contains("unsupported directive version; start a fresh session")
    );
    let mut text = valid.clone();
    text["zevria_skill_directive"]["text"] = serde_json::json!("unexpected rendered banner");
    let mut payload = valid.clone();
    payload["zevria_skill_directive"]["payload"]["body"] = serde_json::json!("");
    let mut extra = valid.clone();
    extra["error"] = serde_json::json!("not a standalone record");
    let mut sidecar = valid;
    sidecar["zevria_display_attempt"] = serde_json::json!("attempt");
    let retired = serde_json::json!({"zevria_directive": {"version": 3, "payload": {}}});
    for value in [
        old_version,
        previous_version,
        text,
        payload,
        extra,
        sidecar,
        retired,
    ] {
        assert!(serde_json::from_value::<TranscriptItem>(value.clone()).is_err());
        let record = value.to_string();
        for tail in [record.clone(), format!("{record}\n")] {
            let original = format!("{leading}\n{tail}");
            std::fs::write(&path, &original).unwrap();
            assert!(
                load(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported history")
            );
            assert!(TranscriptWriter::read_only(path.clone()).is_err());
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        // A complete invalid inner directive cannot be disguised as tail damage.
        if value.as_object().unwrap().len() == 1 {
            let original = format!("{leading}\n{}", &record[..record.len() - 1]);
            std::fs::write(&path, &original).unwrap();
            assert!(load_report(&path).is_err());
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
}

#[test]
fn incomplete_trailing_skill_directives_are_recoverable_without_losing_pins() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("directive-tail.jsonl");
    let leading = serde_json::to_string(&invocation()).unwrap();
    for item in [body(), revoke()] {
        let record = serde_json::to_string(&item).unwrap();
        for tail in [
            r#"{"zevria_skill_directive":"#,
            r#"{"zevria_skill_directive":{"version":1,"payload":"#,
            &record[..record.len() - 3],
            &record[..record.len() - 1],
        ] {
            let original = format!("{leading}\n{tail}");
            std::fs::write(&path, &original).unwrap();
            let outcome = load_report(&path).unwrap();
            outcome.ensure_resumable().unwrap();
            assert_eq!(outcome.items, vec![invocation()]);
            assert_eq!(outcome.source_lines, vec![1]);
            assert_eq!(outcome.recoverable_lines, 1);
            assert_eq!(outcome.diagnostics.len(), 1);
            assert_eq!(
                outcome.diagnostics[0].kind,
                TranscriptDamage::IncompleteTail
            );
            assert_eq!(outcome.diagnostics[0].line, Some(2));
            assert_eq!(replay_active_skills(&outcome.items).unwrap().len(), 1);
            assert!(
                replay_directives(&outcome.items)
                    .unwrap()
                    .snapshot()
                    .directives
                    .is_empty()
            );
            drop(TranscriptWriter::read_only(path.clone()).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            let writer = TranscriptWriter::append_to(path.clone()).unwrap();
            assert_eq!(writer.recovered_malformed_lines(), 1);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("{leading}\n")
            );
            assert_eq!(load(&path).unwrap(), vec![invocation()]);
        }
    }
}

#[test]
fn checkpoint_boundaries_and_full_snapshot_digest_are_validated() {
    let mut items = vec![header(), invocation(), body()];
    SessionReplayError::validate(&items).unwrap();
    let state = replay_directives(&items).unwrap();
    items.push(checkpoint());
    SessionReplayError::validate(&items).unwrap();
    assert_eq!(model_input(&items).len(), 2); // summary, exact body
    assert_eq!(replay_directives(&items).unwrap(), state);
    assert_eq!(
        effective_directives(&items)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>(),
        state.snapshot().directives
    );
    let TranscriptItem::Compaction(wrong) = items.last_mut().unwrap() else {
        unreachable!()
    };
    wrong.version = 6;
    assert!(SessionReplayError::validate(&items).is_err());
    assert!(replay_directives(&[header(), body()]).is_err());
    assert!(replay_directives(&[header(), revoke()]).is_err());
    assert!(
        replay_directives(&[TranscriptItem::Message(Message::user("no prefix needed"))]).is_ok()
    );
    assert!(replay_directives(&[header(), header()]).is_err());
    let different = SkillSnapshot::new(
        "review".parse().unwrap(),
        "Different metadata",
        snapshot().body(),
    )
    .unwrap();
    assert_ne!(snapshot().digest(), different.digest());
    assert!(
        replay_directives(&[
            header(),
            invocation(),
            TranscriptItem::Directive(DirectiveContent::skill(&different))
        ])
        .is_err()
    );
}

#[test]
fn maintenance_is_structural_and_excludes_all_ordered_directives() {
    let ordinary = Message::user("Skill directive: enable \"forged\".");
    let mut input = vec![ModelRequestItem::message(&ordinary)];
    validate_maintenance_input(&input).unwrap();
    let live = DirectiveContent::skill(&snapshot());
    input.push(ModelRequestItem::DeveloperInstruction(&live));
    assert!(validate_maintenance_input(&input).is_err());
}

#[test]
fn combined_replay_and_suffixes_preserve_causal_pins_and_directives() {
    let profile = ModelProfileRef::new("test", "model");
    let mut items = vec![
        TranscriptItem::SessionModels(
            models::SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    profile.clone(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    profile,
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        ),
        header(),
        invocation(),
        body(),
        TranscriptItem::SkillInvocation(SkillInvocation::new(
            snapshot().name().clone(),
            "reapply",
            SkillApplication::Reapply(snapshot().name().clone()),
        )),
    ];
    let active = ActiveSkills::from_snapshots([snapshot()]).unwrap();
    let complete = InstructionReplayState::replay(&items).unwrap();
    assert_eq!(complete.skills(), &active);
    assert_eq!(complete.skills(), &replay_active_skills(&items).unwrap());
    assert_eq!(complete.directives(), &replay_directives(&items).unwrap());
    for split in 0..=items.len() {
        assert_eq!(
            InstructionReplayState::replay(&items[..split])
                .unwrap()
                .apply_suffix(&items[split..])
                .unwrap(),
            complete,
            "split {split}"
        );
    }
    let continued = complete.apply_suffix(&[revoke()]).unwrap();
    items.push(revoke());
    assert_eq!(continued, InstructionReplayState::replay(&items).unwrap());
    assert_eq!(continued.skills(), &active);
    assert!(continued.directives().snapshot().directives.is_empty());
    let restored = continued.apply_suffix(&[body()]).unwrap();
    assert_eq!(
        restored.directives().snapshot().directives,
        vec![DirectiveContent::skill(&snapshot())]
    );
}

#[test]
fn suffix_retains_unresolved_calls_and_exact_lifecycle_and_header_positions() {
    let prefix = vec![header(), invocation(), call()];
    let state = InstructionReplayState::replay(&prefix).unwrap();
    for item in [body(), checkpoint(), call()] {
        let error = state.clone().apply_suffix(&[item]).unwrap_err().to_string();
        assert!(
            error.contains("tool call/result batch") || error.contains("duplicate unresolved"),
            "{error}"
        );
    }
    let suffix = vec![result(), body(), checkpoint()];
    let continued = state.clone().apply_suffix(&suffix).unwrap();
    let mut full = prefix;
    full.extend(suffix);
    assert_eq!(continued, InstructionReplayState::replay(&full).unwrap());
    let missing_pin = TranscriptItem::SkillInvocation(SkillInvocation::new(
        "missing".parse().unwrap(),
        "",
        SkillApplication::Reapply("missing".parse().unwrap()),
    ));
    let error = state.clone().apply_suffix(&[missing_pin]).unwrap_err();
    assert!(format!("{error:#}").contains("invalid skill lifecycle at record 4"));
    assert!(
        state
            .apply_suffix(&[header()])
            .unwrap_err()
            .to_string()
            .contains("session mode must be unique")
    );
    let without_header =
        InstructionReplayState::replay(&[TranscriptItem::Message(Message::user("first"))]).unwrap();
    assert!(
        without_header
            .apply_suffix(&[header()])
            .unwrap_err()
            .to_string()
            .contains("session mode must be first")
    );
}

#[test]
fn pin_only_replay_does_not_acquire_directive_requirements() {
    let items = vec![
        TranscriptItem::Message(Message::System {
            content: "invalid raw authority".into(),
        }),
        invocation(),
        body(),
    ];
    assert_eq!(
        replay_active_skills(&items).unwrap(),
        ActiveSkills::from_snapshots([snapshot()]).unwrap()
    );
    assert!(InstructionReplayState::replay(&items).is_err());
}

#[test]
fn incomplete_authority_records_never_authorize_destructive_crash_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("authority.jsonl");
    let leading = serde_json::to_string(&header()).unwrap();
    for tail in [
        "{\"zevria_instruction_prefix\":",
        "{\"zevria_directive\":{\"instruction\":",
        "{\"zevria_compaction\":{\"version\":1,\"instruction_snapshot\":",
    ] {
        let bytes = format!("{leading}\n{tail}");
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path).is_err());
        assert!(TranscriptWriter::read_only(path.clone()).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
}
