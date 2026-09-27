use super::*;

#[path = "inline_web_interaction_tests.rs"]
mod interaction;
use crate::app::ConversationState;
use zevria_content::AssistantPartIdentity;
use zevria_content::AssistantPresentationContent;
use zevria_content::AssistantPresentationPart;
use zevria_content::AssistantSourceAddress;
use zevria_content::AssistantStreamSnapshot;
use zevria_content::WebSearchActivity;
use zevria_content::WebSearchAttemptOutcome as Outcome;
use zevria_content::WebSearchAttemptRecord;
use zevria_content::WebSearchStatus as Status;
use zevria_content::web_search::DisplayProjectionBinding as Binding;
use zevria_content::web_search::DisplayProjectionKind as Kind;
use zevria_content::web_search::ResponseDisplay;

fn part(
    output: u64,
    identity: AssistantPartIdentity,
    content: AssistantPresentationContent,
) -> AssistantPresentationPart {
    AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index: output,
            part: identity,
            item_id: Some(format!("item-{output}")),
        },
        content,
    }
}
fn reasoning(output: u64, text: &str) -> AssistantPresentationPart {
    part(
        output,
        AssistantPartIdentity::Summary(0),
        AssistantPresentationContent::Reasoning { text: text.into() },
    )
}
fn action(output: u64, action: Option<serde_json::Value>) -> WebSearchActivity {
    WebSearchActivity {
        item_id: Some(format!("web-{output}")),
        output_index: output,
        status: Status::Searching,
        action,
    }
}
fn ordered_attempt() -> WebSearchAttemptRecord {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("provider", "model"));
    attempt.presentation = vec![
        reasoning(0, "first thought"),
        reasoning(2, "second thought"),
        part(
            4,
            AssistantPartIdentity::Content(0),
            AssistantPresentationContent::Answer {
                text: "cited answer [source](https://example.com)".into(),
            },
        ),
    ];
    attempt.activity = vec![
        action(1, Some(json!({"type":"search","query":"readable query"}))),
        action(
            3,
            Some(json!({"type":"open_page","url":"https://example.com/full/path"})),
        ),
    ];
    attempt.touch();
    attempt
}
fn visible(conversation: &ConversationState) -> Vec<String> {
    conversation
        .history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(
                entry
                    .blocks
                    .iter()
                    .filter(|block| block.visible(false))
                    .map(|block| block.primary_copy()),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}
fn publish(app: &mut App, attempt: WebSearchAttemptRecord) {
    apply_turn_event(
        app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: AssistantStreamSnapshot {
                message: Some(Message::assistant("must not duplicate the generic tail")),
                attempt: Some(attempt),
            },
        },
    );
}

#[test]
fn native_web_content_replaces_pending_header_and_keeps_its_call_on_revisions() {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("search"),
        mode: SessionMode::Build,
    });
    app.reduce_without_effects(SessionEvent::ModelCallStarted {
        turn_id: TEST_TURN_ID,
        call: 1,
    });
    assert!(app.render_parts().pending_header);
    let mut attempt = ordered_attempt();
    attempt.presentation.clear(); // Web activity alone owns the current header.
    publish(&mut app, attempt.clone());
    assert!(!app.render_parts().pending_header);
    assert_eq!(
        rendered_text(&mut app, 120, 40)
            .matches("● Assistant · #(1 - 1)")
            .count(),
        1
    );
    app.reduce_without_effects(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        message: Message::assistant("canonical"),
        display_attempt_id: Some(attempt.id.clone()),
    });
    app.reduce_without_effects(SessionEvent::ModelCallStarted {
        turn_id: TEST_TURN_ID,
        call: 2,
    });
    attempt.finish(Outcome::Completed);
    app.reduce_without_effects(SessionEvent::WebSearchUpdated {
        turn_id: TEST_TURN_ID,
        attempt,
    });
    let text = rendered_text(&mut app, 120, 40);
    assert_eq!(
        text.matches("● Assistant · #(1 - 1)").count(),
        1,
        "old activity cannot be relabeled"
    );
    assert_eq!(text.matches("● Assistant · #(1 - 2)").count(), 1);
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        turn_id: TEST_TURN_ID,
        message: Message::assistant("final"),
        display_attempt_id: None,
    });
    assert_eq!(
        rendered_text(&mut app, 120, 40)
            .matches("● Assistant · #(1 - 2)")
            .count(),
        1
    );
}

#[test]
fn restore_numbers_explicit_bindings_once_but_not_ambiguous_retry_activity() {
    for provider in [false, true] {
        for late_activity in [false, true] {
            let mut bound = ordered_attempt();
            bound.finish(Outcome::Completed);
            let mut unbound = ordered_attempt();
            unbound.finish(Outcome::Interrupted);
            let mut revised = unbound.clone();
            revised.touch();
            let response = if provider {
                TranscriptItem::provider_message(ProviderReplay::openai_responses(
                    bound.profile.clone(), vec![json!({
                        "type":"message", "id":"answer", "role":"assistant", "status":"completed",
                        "content":[{"type":"output_text", "text":"canonical answer", "annotations":[]}]
                    })],
                )).unwrap()
            } else {
                TranscriptItem::Message(Message::assistant("canonical answer"))
            }.with_display_attempt(Some(bound.id.clone())).unwrap();
            let mut items = vec![
                TranscriptItem::Message(Message::user("prompt")),
                TranscriptItem::WebSearchAttempt(unbound),
                TranscriptItem::WebSearchAttempt(revised),
            ];
            if late_activity {
                items.push(response.clone());
            }
            items.push(TranscriptItem::WebSearchAttempt(bound));
            if !late_activity {
                items.push(response);
            }
            items.push(TranscriptItem::Message(Message::assistant("next response")));
            let mut restored = App::new();
            restored.restore(items);
            let labels = restored
                .history()
                .iter()
                .filter_map(|entry| match entry {
                    HistoryEntry::Conversation(entry) => {
                        Some(entry.header.map(|header| header.to_string()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                labels,
                [
                    Some("#1".into()),
                    None,
                    Some("#(1 - 1)".into()),
                    Some("#(1 - 2)".into())
                ]
            );
            // Unbound failed attempts represent no durable response: rebuilding
            // deliberately omits their runtime dispatch count.
            assert!(!rendered_text(&mut restored, 120, 50).contains("#(1 - 3)"));
        }
    }
}

#[test]
fn unknown_actions_split_and_refresh_only_changed_layouts() {
    let mut attempt = ordered_attempt();
    attempt.activity = vec![action(1, None), action(2, None)];
    attempt
        .presentation
        .retain(|part| part.source.output_index != 2);
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    let initial = rendered_text(&mut app, 80, 24);
    assert!(initial.contains("Web actions · 2"));
    assert!(!initial.contains("Provider web activity"));
    assert!(!initial.contains("provider/model"));
    assert!(app.streaming().is_none());
    let rebuilds = app.view_cache().block_rebuilds;
    app.select_for_test(cursor(0, 1));
    rendered_text(&mut app, 80, 24);
    let selected_rebuilds = app.view_cache().block_rebuilds;
    assert!(selected_rebuilds >= rebuilds);
    let full = format!("日本語 {}", "copy all these query words ".repeat(20));
    attempt.activity[0].action = Some(json!({"type":"search","query":full}));
    attempt.terminal.insert(1, Status::Completed);
    attempt.activity[0].status = Status::Completed;
    attempt.touch();
    publish(&mut app, attempt.clone());
    rendered_text(&mut app, 80, 24);
    assert_eq!(
        app.view_cache().block_rebuilds - selected_rebuilds,
        2,
        "unchanged reasoning and answer layouts remain cached"
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: format!("Web search: {full} ✓")
        })
    );
    let text = rendered_text(&mut app, 24, 60);
    assert!(!text.contains("Provider web activity"));
    assert!(!text.contains("must not duplicate"));
    let mut stale = attempt.clone();
    stale.revision -= 1;
    stale.activity[0].action = None;
    publish(&mut app, stale);
    assert!(
        visible(app.conversation_projection_mut())
            .iter()
            .any(|text| text.contains(&full))
    );
}

#[test]
fn web_action_icons_follow_terminal_evidence_instead_of_attempt_outcome() {
    for (status, confirmed, outcome, glyph) in [
        (Status::InProgress, None, Outcome::InProgress, "◐"),
        (Status::Searching, None, Outcome::InProgress, "◐"),
        (
            Status::Completed,
            Some(Status::Completed),
            Outcome::Failed,
            "✓",
        ),
        (
            Status::Failed,
            Some(Status::Failed),
            Outcome::Completed,
            "✗",
        ),
        (
            Status::Interrupted,
            Some(Status::Interrupted),
            Outcome::Interrupted,
            "◼",
        ),
        (Status::Completed, None, Outcome::Completed, "•"),
        (Status::Searching, None, Outcome::Interrupted, "•"),
    ] {
        let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("provider", "model"));
        let mut action = action(
            0,
            Some(json!({"type": "search", "query": "readable query"})),
        );
        action.status = status;
        if let Some(confirmed) = confirmed {
            attempt.terminal.insert(0, confirmed);
        }
        attempt.finish(outcome);
        let activity =
            crate::presentation::WebActivityPresentation::from_actions(&attempt, &[&action]);
        assert_eq!(
            activity.copy_text(),
            format!("Web search: readable query {glyph}")
        );
    }
}

#[test]
fn metadata_free_acp_hosted_search_uses_the_shared_status_icons() {
    for (status, glyph) in [
        ("pending", "○"),
        ("in_progress", "◐"),
        ("running", "◐"),
        ("completed", "✓"),
        ("finished", "✓"),
        ("failed", "✗"),
        ("error", "✗"),
        ("denied", "⊘"),
        ("interrupted", "◼"),
        ("cancelled", "◼"),
        ("unknown", "?"),
    ] {
        let tool = crate::presentation::AcpToolPresentation {
            metadata: None,
            id: "hosted-search:test:0".into(),
            title: "Search".into(),
            kind: "search".into(),
            status: status.into(),
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(json!({
                "origin": "provider_hosted_web_search",
                "action": {"type": "search", "query": "readable query"},
            })),
            raw_output: None,
        };
        assert_eq!(
            tool.input_text(),
            format!("Web search: readable query {glyph}")
        );
    }
}

#[test]
fn terminal_unknown_groups_retain_mixed_evidence_and_one_incomplete_marker() {
    let mut attempt = ordered_attempt();
    attempt.presentation.clear();
    attempt.activity = vec![action(0, None), action(1, None), action(2, None)];
    attempt.terminal.insert(0, Status::Completed);
    attempt.terminal.insert(1, Status::Failed);
    attempt.finish(Outcome::Interrupted);
    let mut conversation = ConversationState::default();
    conversation.update_web_search(attempt.clone());
    conversation.update_web_search(attempt);
    let rows = visible(&conversation);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], "Web actions · 3 · 1 ✓ · 1 ✗ · 1 •");
    assert_eq!(rows[1], "Incomplete response · interrupted");
}

#[test]
fn live_committed_and_restored_attempts_have_identical_order_without_answer_duplication() {
    let mut attempt = ordered_attempt();
    attempt.finish(Outcome::Completed);
    let message = Message::assistant("canonical answer lives independently of the display trail");
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    let before = visible(app.conversation_projection_mut());
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: message.clone(),
            display_attempt_id: Some(attempt.id.clone()),
        },
    );
    assert_eq!(visible(app.conversation_projection_mut()), before);
    let record = TranscriptItem::Message(message)
        .with_display_attempt(Some(attempt.id.clone()))
        .unwrap();
    let mut restored = App::new();
    restored.restore(vec![TranscriptItem::WebSearchAttempt(attempt), record]);
    assert_eq!(visible(restored.conversation_projection_mut()), before);
    let rendered = rendered_text(&mut restored, 80, 30);
    assert_eq!(rendered.matches("reasoning").count(), 2);
    assert_eq!(rendered.matches("cited answer").count(), 1);
    assert!(!rendered.contains("ordering unavailable"));
}

#[test]
fn compacted_native_attempt_restores_the_live_text_and_order_without_duplication() {
    let mut attempt = ordered_attempt();
    assert_eq!(attempt.version, 1);
    let replay = ProviderReplay::openai_responses(
        attempt.profile.clone(),
        vec![
            json!({"type":"reasoning", "id":"item-0", "summary":[{"type":"summary_text", "text":"first thought"}]}),
            json!({"type":"web_search_call", "id":"web-1", "status":"completed", "action":{"type":"search", "query":"readable query"}}),
            json!({"type":"reasoning", "id":"item-2", "summary":[{"type":"summary_text", "text":"second thought"}]}),
            json!({"type":"web_search_call", "id":"web-3", "status":"completed", "action":{"type":"open_page", "url":"https://example.com/full/path"}}),
            json!({"type":"message", "id":"item-4", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"cited answer [source](https://example.com)", "annotations":[]}]}),
        ],
    );
    attempt.reconcile_native_presentation(&replay.items);
    attempt.finish(Outcome::Completed);
    let record = TranscriptItem::provider_message(replay)
        .unwrap()
        .with_display_attempt(Some(attempt.id.clone()))
        .unwrap();
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    let before = visible(app.conversation_projection_mut());
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: record.message().unwrap().clone(),
            display_attempt_id: Some(attempt.id.clone()),
        },
    );
    assert_eq!(visible(app.conversation_projection_mut()), before);
    let mut items = vec![TranscriptItem::WebSearchAttempt(attempt), record];
    zevria_transcript::transcript::compact_linked_attempts(&mut items);
    assert!(
        matches!(&items[0], TranscriptItem::WebSearchAttempt(saved) if saved.presentation_elided)
    );
    let mut restored = App::new();
    restored.restore(items);
    assert_eq!(visible(restored.conversation_projection_mut()), before);
    let rendered = rendered_text(&mut restored, 80, 30);
    assert_eq!(rendered.matches("reasoning").count(), 2);
    assert_eq!(rendered.matches("cited answer").count(), 1);
    assert!(!rendered.contains("ordering unavailable"));
}

#[test]
fn headings_follow_visible_runs_across_committed_and_streamed_boundaries() {
    let message = |text: &str| Message::Assistant {
        id: None,
        content: vec![AssistantContent::Reasoning(Reasoning::summaries(vec![
            text.into(),
        ]))],
    };
    let mut app = App::new();
    app.seed_history_entry(history_message(message("one")));
    app.seed_history_entry(history_message(message("two")));
    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (message("three")).into(),
        },
    );
    assert_eq!(
        rendered_text(&mut app, 80, 20).matches("reasoning").count(),
        1
    );
    for content in [
        vec![ReasoningContent::Summary("   \n\t".into())],
        vec![ReasoningContent::Encrypted("opaque".into())],
        vec![],
    ] {
        let mut reasoning = Reasoning::summaries(Vec::new());
        reasoning.content = content;
        assert!(
            HistoryEntry::from_message(
                Message::Assistant {
                    id: None,
                    content: vec![AssistantContent::Reasoning(reasoning)]
                },
                ToolCallStatus::Finished
            )
            .is_none()
        );
    }
}

fn metadata(attempt: WebSearchAttemptRecord) -> ResponseDisplay {
    let mut bindings = Vec::new();
    for kind in [Kind::Message, Kind::Thought] {
        let parts = attempt
            .presentation
            .iter()
            .filter(|part| {
                matches!(
                    (&part.content, kind),
                    (AssistantPresentationContent::Answer { .. }, Kind::Message)
                        | (
                            AssistantPresentationContent::Reasoning { .. },
                            Kind::Thought
                        )
                )
            })
            .collect::<Vec<_>>();
        let text = parts
            .iter()
            .map(|part| match &part.content {
                AssistantPresentationContent::Answer { text }
                | AssistantPresentationContent::Reasoning { text } => text.as_str(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        bindings.push(Binding::Text {
            kind,
            message_id: format!("{kind:?}"),
            start: 0,
            end: text.len(),
            sources: parts.into_iter().map(|part| part.source.clone()).collect(),
        });
    }
    bindings.extend(attempt.activity.iter().map(|action| Binding::Tool {
        tool_call_id: action.client_id(&attempt.id),
        output_index: action.output_index,
        native: false,
    }));
    ResponseDisplay {
        version: 1,
        attempt,
        bindings,
    }
}
fn standard_events(attempt: &WebSearchAttemptRecord) -> Vec<AgentRunEvent> {
    let mut events = vec![AgentRunEvent::Thought {
        message_id: Some("Thought".into()),
        text: "first thought\nsecond thought".into(),
    }];
    events.extend(
        attempt
            .activity
            .iter()
            .map(|action| AgentRunEvent::ToolCall {
                id: action.client_id(&attempt.id),
                title: action.label(),
                kind: "search".into(),
                status: "in_progress".into(),
                content: vec![],
                locations: vec![],
                raw_input: Some(
                    json!({"origin":"provider_hosted_web_search", "action":action.action}),
                ),
                raw_output: None,
            }),
    );
    events.push(AgentRunEvent::AgentMessage {
        message_id: Some("Message".into()),
        text: "cited answer [source](https://example.com)".into(),
    });
    events
}

#[test]
fn delayed_acp_metadata_reorders_shared_ids_and_never_discards_unrelated_text() {
    let mut attempt = ordered_attempt();
    attempt.finish(Outcome::Completed);
    let mut reducer = AgentTranscriptReducer::default();
    let mut conversation = ConversationState::default();
    for event in standard_events(&attempt) {
        reducer.apply_event(&mut conversation, event);
    }
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::AgentMessage {
            message_id: Some("unrelated".into()),
            text: "unrelated text stays".into(),
        },
    );
    let display = metadata(attempt.clone());
    display.validate().unwrap();
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(display.clone()),
        },
    );
    let rows = visible(&conversation);
    assert_eq!(
        rows,
        vec![
            "first thought",
            "Web search: readable query •",
            "second thought",
            "Open page: https://example.com/full/path •",
            "cited answer [source](https://example.com)",
            "unrelated text stays"
        ]
    );
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(display.clone()),
        },
    );
    assert_eq!(visible(&conversation), rows);
    reducer.reconcile_report(
        &mut conversation,
        "cited answer [source](https://example.com)\nunrelated text stays",
    );
    assert_eq!(
        visible(&conversation),
        rows,
        "terminal flattened reports must not replace indexed displays"
    );
    let mut stale = display;
    stale.attempt.revision = 0;
    stale.attempt.outcome = Outcome::Failed;
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(stale),
        },
    );
    assert_eq!(visible(&conversation), rows);
}

#[test]
fn invalid_or_delayed_bindings_fall_back_and_suffixes_outside_coverage_survive() {
    let attempt = ordered_attempt();
    let mut reducer = AgentTranscriptReducer::default();
    let mut conversation = ConversationState::default();
    let display = metadata(attempt.clone());
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(display.clone()),
        },
    );
    assert!(visible(&conversation).is_empty());
    for event in standard_events(&attempt) {
        reducer.apply_event(&mut conversation, event);
    }
    let before = visible(&conversation);
    assert_eq!(before.len(), 5);
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::AgentMessage {
            message_id: Some("Message".into()),
            text: " additional unrelated suffix".into(),
        },
    );
    assert!(
        visible(&conversation)
            .iter()
            .any(|text| text == " additional unrelated suffix")
    );
    let mut malformed = display;
    malformed.attempt.revision += 1;
    if let Binding::Text { end, .. } = &mut malformed.bindings[0] {
        *end += 1;
    }
    assert!(malformed.validate().is_err());
    let before = visible(&conversation);
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(malformed),
        },
    );
    assert_eq!(visible(&conversation), before);
}
