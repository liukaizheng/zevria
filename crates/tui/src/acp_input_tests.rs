//! ACP input presentation must not alter prompts, copy payloads, or review authority.
use super::*;
use crate::presentation::{
    BlockVisibility, DiagnosticTone, PresentationBlock, PromptOrigin, PromptPhase,
    TranscriptAppearance,
};
use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;
use zevria_workflow::WorkerControlId;
use zevria_workflow::WorkerInput;
use zevria_workflow::WorkerPromptKind;
use zevria_workflow::WorkerReviewEvent as Review;
use zevria_workflow::WorkerReviewState;

fn descriptor() -> AgentRunDescriptor {
    AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "test".into(),
        label: "Worker".into(),
        safe_mode: "read-only".into(),
    }
}
fn input(generation: u64, kind: WorkerPromptKind, text: impl Into<UserPrompt>) -> WorkerInput {
    WorkerInput {
        generation,
        request_id: WorkerControlId(format!("input-{generation}")),
        kind,
        text: text.into(),
    }
}
fn review(app: &mut App, reducer: &mut AgentTranscriptReducer, event: Review) {
    apply_agent_event(
        app,
        reducer,
        AgentRunEvent::Review {
            event: Box::new(event),
        },
    );
}
fn settled(generation: u64, failure: Option<&str>) -> Review {
    let mut evidence = WorkerReviewState::new(descriptor()).evidence;
    evidence.failure = failure.map(str::to_owned);
    Review::Settled {
        generation,
        failure: failure.map(str::to_owned),
        connected: true,
        evidence: Box::new(evidence),
    }
}
fn blocks(app: &App) -> impl Iterator<Item = &PresentationBlock> {
    app.history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(entry.blocks.iter()),
            _ => None,
        })
        .flatten()
}
fn header(app: &App, generation: u64) -> &PresentationBlock {
    blocks(app)
        .find(|block| {
            block
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.generation == Some(generation))
        })
        .unwrap()
}
fn phase(app: &App, generation: u64) -> Option<PromptPhase> {
    header(app, generation).prompt.as_ref().unwrap().phase
}
fn notices(app: &App) -> Vec<(DiagnosticTone, String)> {
    blocks(app)
        .filter_map(|block| match &block.kind {
            PresentationBlockKind::Diagnostic(diagnostic)
                if block.visibility == BlockVisibility::Always =>
            {
                Some((diagnostic.tone, diagnostic.text.clone()))
            }
            _ => None,
        })
        .collect()
}
fn plan(id: &str, markdown: Option<&str>) -> AgentStructuredPlan {
    AgentStructuredPlan {
        plan_id: Some(id.into()),
        markdown: markdown.map(str::to_owned),
        entries: Vec::new(),
    }
}

#[test]
fn prompt_origins_are_decorative_and_body_is_verbatim() {
    let (mut app, mut reducer) = acp_transcript_app();
    let text = "continuation prompt: user-authored\n\n  ```rust\n  e\u{301} 界 👩‍💻\n  ```\n";
    for (index, (kind, origin)) in [
        (WorkerPromptKind::Initial, PromptOrigin::Initial),
        (WorkerPromptKind::UserFeedback, PromptOrigin::Feedback),
        (WorkerPromptKind::RecoveryContinuation, PromptOrigin::Retry),
        (WorkerPromptKind::SemanticRecovery, PromptOrigin::Recovery),
    ]
    .into_iter()
    .enumerate()
    {
        let generation = index as u64 + 1;
        review(
            &mut app,
            &mut reducer,
            Review::InputAccepted {
                input: input(generation, kind, text),
            },
        );
        let block = header(&app, generation);
        assert_eq!(block.primary_copy(), text);
        assert_eq!(block.secondary_copy(), None);
        assert!(!block.is_editable());
        assert_eq!(block.prompt.as_ref().unwrap().origin, origin);
        assert_eq!(block.prompt_group, Some(block.id));
    }
    let (mut ordinary, mut reducer) = acp_transcript_app();
    apply_agent_event(
        &mut ordinary,
        &mut reducer,
        AgentRunEvent::Prompt {
            text: text.into(),
            continuation: true,
            repair: None,
        },
    );
    let block = blocks(&ordinary).next().unwrap();
    assert_eq!(block.primary_copy(), text);
    assert_eq!(
        block.prompt.as_ref().unwrap().origin,
        PromptOrigin::Continuation
    );
    assert_eq!(block.prompt.as_ref().unwrap().phase, None);
}

#[test]
fn prompt_phase_badges_use_icons_and_keep_attempts_colors_and_copy() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::UserFeedback, "verbatim prompt"),
        },
    );
    for (phase, attempt, badge, color) in [
        (
            Some(PromptPhase::Queued),
            None,
            " ○",
            ZEVRIA_DARK.text.muted,
        ),
        (
            Some(PromptPhase::Dispatched),
            None,
            " ◐",
            ZEVRIA_DARK.feedback.info,
        ),
        (
            Some(PromptPhase::Recovering),
            Some(3),
            " ◐ recovering 3",
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            Some(PromptPhase::Recovering),
            None,
            " ◐ recovering 0",
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            Some(PromptPhase::Cancelling),
            None,
            " ◐ cancelling",
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            Some(PromptPhase::Failed),
            None,
            " ✗",
            ZEVRIA_DARK.feedback.error,
        ),
        (
            Some(PromptPhase::Interrupted),
            None,
            " ◼",
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            Some(PromptPhase::Succeeded),
            None,
            "",
            ZEVRIA_DARK.text.muted,
        ),
        (None, None, "", ZEVRIA_DARK.text.muted),
    ] {
        let mut block = header(&app, 1).clone();
        let prompt = block.prompt.as_mut().unwrap();
        prompt.phase = phase;
        prompt.latest_attempt = attempt;
        let mut lines = Vec::new();
        crate::layout::prepare::render_conversation_block(
            &block,
            &mut lines,
            crate::layout::prepare::ConversationBlockContext {
                width: 80,
                header_role: Some(PresentationRole::User),
                header: None,
                separator_before: false,
                selected: false,
                folded: false,
                reasoning_heading: false,
                appearance: TranscriptAppearance::Acp,
            },
        );
        assert_eq!(
            lines[0].to_string().trim(),
            format!("● You · feedback{badge}")
        );
        assert_eq!(lines[0].spans.last().unwrap().style.fg, Some(color));
        if matches!(
            phase,
            Some(PromptPhase::Dispatched | PromptPhase::Recovering | PromptPhase::Cancelling)
        ) {
            assert_eq!(
                lines[0]
                    .spans
                    .iter()
                    .find(|span| span.content == "◐")
                    .unwrap()
                    .style
                    .fg,
                Some(ZEVRIA_DARK.feedback.info)
            );
        }
        assert_eq!(block.primary_copy(), "verbatim prompt");
        assert_eq!(lines[1].to_string().trim(), "verbatim prompt");
    }
}

#[test]
fn lifecycle_is_in_place_terminal_and_generation_correlated() {
    let (mut app, mut reducer) = acp_transcript_app();
    let first = input(1, WorkerPromptKind::Initial, "initial");
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: first.clone(),
        },
    );
    let id = header(&app, 1).id;
    review(
        &mut app,
        &mut reducer,
        Review::CancelRequested { generation: 1 },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    assert_eq!(phase(&app, 1), Some(PromptPhase::Cancelling));
    let revision = header(&app, 1).revision;
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted { input: first },
    );
    assert_eq!(header(&app, 1).revision, revision);
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(2, WorkerPromptKind::UserFeedback, "feedback"),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Recovering {
            generation: 1,
            attempt: 3,
        },
    );
    let revision = header(&app, 1).revision;
    review(
        &mut app,
        &mut reducer,
        Review::Recovering {
            generation: 1,
            attempt: 2,
        },
    );
    assert_eq!(header(&app, 1).revision, revision);
    assert_eq!(phase(&app, 2), Some(PromptPhase::Queued));
    review(&mut app, &mut reducer, settled(1, None));
    assert_eq!(phase(&app, 1), Some(PromptPhase::Succeeded));
    assert_eq!(header(&app, 1).id, id);
    let revision = header(&app, 1).revision;
    for event in [
        Review::Dispatched {
            generation: 1,
            attempt: 8,
        },
        Review::Recovering {
            generation: 1,
            attempt: 9,
        },
        Review::CancelRequested { generation: 1 },
    ] {
        review(&mut app, &mut reducer, event);
    }
    assert_eq!(header(&app, 1).revision, revision);
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 99,
            attempt: 1,
        },
    );
    assert_eq!(phase(&app, 2), Some(PromptPhase::Queued));
    assert_eq!(
        blocks(&app).filter(|block| block.prompt.is_some()).count(),
        2
    );
    let buffer = rendered_buffer(&mut app, 120, 40);
    let text = buffer_text(&buffer);
    assert!(text.contains("● You · feedback ○"));
    assert!(!text.contains("cancelling"));
    assert!(!text.contains("Input generation 1 · dispatched"));
    assert!(
        notices(&app)
            .iter()
            .all(|(tone, _)| *tone != DiagnosticTone::Success)
    );
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn host_lifecycle_diagnostics_do_not_split_streamed_assistant_text() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::Initial, "input"),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: "before".into(),
            message_id: Some("answer".into()),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Recovering {
            generation: 1,
            attempt: 2,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: " after".into(),
            message_id: Some("answer".into()),
        },
    );
    assert_eq!(assistant_presentation_texts(&app), ["before after"]);
    let normal = buffer_text(&rendered_buffer(&mut app, 120, 30));
    assert!(!normal.contains("Host review"));
    app.handle_event(key(KeyCode::Char('d')));
    let diagnostic = buffer_text(&rendered_buffer(&mut app, 120, 40));
    assert!(diagnostic.contains("Host review"));
    assert_eq!(diagnostic.matches("● You").count(), 1);
    assert_eq!(diagnostic.matches("● Assistant").count(), 1);
}

#[test]
fn publication_proof_tracks_surviving_markdown_not_ever_published_generations() {
    for scenario in 0..7 {
        let (mut app, mut reducer) = acp_transcript_app();
        review(
            &mut app,
            &mut reducer,
            Review::InputAccepted {
                input: input(1, WorkerPromptKind::Initial, "input"),
            },
        );
        review(
            &mut app,
            &mut reducer,
            Review::Dispatched {
                generation: 1,
                attempt: 1,
            },
        );
        let publication = match scenario {
            1 => plan("a", Some(" \n")),
            2 => plan("a", None),
            _ => plan("a", Some("# Complete proposal")),
        };
        review(
            &mut app,
            &mut reducer,
            Review::Published {
                generation: 1,
                plan: publication,
                replay: scenario == 0,
            },
        );
        match scenario {
            3 => review(
                &mut app,
                &mut reducer,
                Review::Removed {
                    plan_id: "a".into(),
                },
            ),
            4 => review(
                &mut app,
                &mut reducer,
                Review::Removed {
                    plan_id: "unrelated".into(),
                },
            ),
            5 => review(
                &mut app,
                &mut reducer,
                Review::Published {
                    generation: 1,
                    plan: plan("a", None),
                    replay: false,
                },
            ),
            6 => {
                review(
                    &mut app,
                    &mut reducer,
                    Review::Published {
                        generation: 1,
                        plan: plan("b", Some("replacement")),
                        replay: false,
                    },
                );
                review(
                    &mut app,
                    &mut reducer,
                    Review::Removed {
                        plan_id: "a".into(),
                    },
                );
            }
            _ => {}
        }
        review(&mut app, &mut reducer, settled(1, None));
        review(&mut app, &mut reducer, settled(1, None));
        let notices = notices(&app);
        assert_eq!(
            notices.len(),
            usize::from(scenario <= 3),
            "scenario {scenario}: {notices:?}"
        );
        if let Some((tone, text)) = notices.first() {
            assert_eq!(*tone, DiagnosticTone::Info);
            assert!(text.contains("fresh, complete Markdown"));
        }
    }
}

#[test]
fn raw_publication_removal_and_replay_preserve_header_correlation() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::Initial, "input"),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::Prompt {
            text: "provider envelope".into(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::Plan {
            plan: plan("a", Some("draft")),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::PlanRemoved {
            plan_id: "a".into(),
        },
    );
    reconcile_agent_report(&mut app, &mut reducer, "answer");
    review(
        &mut app,
        &mut reducer,
        Review::Recovering {
            generation: 1,
            attempt: 2,
        },
    );
    assert_eq!(phase(&app, 1), Some(PromptPhase::Recovering));
    apply_agent_event(&mut app, &mut reducer, AgentRunEvent::ReplayBoundary);
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::Plan {
            plan: plan("a", Some("replayed draft")),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::UserMessage {
            text: "replayed input".into(),
            message_id: None,
        },
    );
    review(&mut app, &mut reducer, settled(1, None));
    assert_eq!(phase(&app, 1), Some(PromptPhase::Succeeded));
    assert_eq!(notices(&app).len(), 1);
    assert!(!blocks(&app).any(|block| block.primary_copy().contains("replayed")));
}

#[test]
fn image_first_mixed_repeated_echoes_keep_one_group_without_queue_reset() {
    let image = PromptImage::from_rgba(1, 1, &[4, 5, 6, 255]).unwrap();
    let prompt = UserPrompt::new(vec![
        PromptBlock::Image(image.clone()),
        PromptBlock::Text("literal\n\n".into()),
        PromptBlock::Image(image.clone()),
    ])
    .unwrap();
    let (mut app, mut reducer) = acp_transcript_app();
    let accepted = input(1, WorkerPromptKind::UserFeedback, prompt);
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: accepted.clone(),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: Some("user".into()),
        },
    );
    // Either duplicate used to reinitialize state before deduplication.
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted { input: accepted },
    );
    for text in ["lit", "eral\n\n"] {
        apply_agent_event(
            &mut app,
            &mut reducer,
            AgentRunEvent::UserMessage {
                text: text.into(),
                message_id: Some("user".into()),
            },
        );
    }
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: Some("user".into()),
        },
    );
    // A third identical image is not an echo; repeated bytes alone cannot dedup it.
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: Some("new".into()),
        },
    );
    let user = blocks(&app)
        .filter(|block| block.role == Some(PresentationRole::User))
        .collect::<Vec<_>>();
    assert_eq!(user.len(), 4);
    assert_eq!(user[0].primary_copy(), image.label(1));
    assert_eq!(user[1].primary_copy(), "literal\n\n");
    assert_eq!(user[2].primary_copy(), image.label(2));
    assert_eq!(user[0].prompt_group, user[2].prompt_group);
    assert_ne!(user[0].prompt_group, user[3].prompt_group);
    assert!(user[0].prompt.is_some());
    assert!(user[1].prompt.is_none());
    let text = buffer_text(&rendered_buffer(&mut app, 120, 40));
    assert_eq!(text.matches("● You").count(), 2);
}

#[test]
fn unmatched_user_segments_group_images_but_keep_distinct_message_headers() {
    let (mut app, mut reducer) = acp_transcript_app();
    let image = PromptImage::from_rgba(1, 1, &[4, 5, 6, 255]).unwrap();
    for id in ["one", "two"] {
        apply_agent_event(
            &mut app,
            &mut reducer,
            AgentRunEvent::UserImage {
                image: image.clone(),
                message_id: Some(id.into()),
            },
        );
        apply_agent_event(
            &mut app,
            &mut reducer,
            AgentRunEvent::UserMessage {
                text: id.into(),
                message_id: Some(id.into()),
            },
        );
    }
    let user = blocks(&app).collect::<Vec<_>>();
    assert_eq!(user[0].prompt_group, user[1].prompt_group);
    assert_eq!(user[2].prompt_group, user[3].prompt_group);
    assert_ne!(user[0].prompt_group, user[2].prompt_group);
    for block in [user[0], user[2]] {
        let annotation = block.prompt.as_ref().unwrap();
        assert_eq!(annotation.origin, PromptOrigin::Initial);
        assert_eq!(annotation.phase, None);
    }
    let text = buffer_text(&rendered_buffer(&mut app, 80, 30));
    assert_eq!(text.matches("● You").count(), 2);
    app.select_for_test(cursor(0, 0));
    app.handle_event(ctrl('d'));
    assert_eq!(app.selection(), cursor(0, 2));
}

#[test]
fn unmatched_review_chunks_do_not_inherit_accepted_input_provenance() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::UserFeedback, "accepted"),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::UserMessage {
            text: "acceptedunmatched".into(),
            message_id: None,
        },
    );
    let unmatched = blocks(&app)
        .find(|block| block.primary_copy() == "unmatched")
        .unwrap();
    assert_ne!(unmatched.prompt_group, header(&app, 1).prompt_group);
    let annotation = unmatched.prompt.as_ref().unwrap();
    assert_eq!(annotation.origin, PromptOrigin::Initial);
    assert_eq!(annotation.phase, None);
    assert_eq!(annotation.generation, None);
}

#[test]
fn failed_initial_and_cancelled_request_never_invent_retained_plan_or_cancellation() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::Initial, "initial"),
        },
    );
    review(
        &mut app,
        &mut reducer,
        Review::CancelRequested { generation: 1 },
    );
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    review(&mut app, &mut reducer, settled(1, Some("provider failed")));
    assert_eq!(phase(&app, 1), Some(PromptPhase::Failed));
    let notice = &notices(&app)[0];
    assert_eq!(notice.0, DiagnosticTone::Error);
    assert!(notice.1.contains("provider failed"));
    assert!(notice.1.contains("Cancellation was requested"));
    assert!(!notice.1.contains("remains available"));
    assert!(!notice.1.contains("cancelled"));
    review(
        &mut app,
        &mut reducer,
        Review::Interrupted { generation: 1 },
    );
    assert_eq!(notices(&app).len(), 1);
    assert_eq!(notices(&app)[0].0, DiagnosticTone::Warning);
    assert!(
        notices(&app)[0]
            .1
            .contains("not incorporated or automatically resent")
    );
}

#[test]
fn snapshots_and_mirrors_upsert_one_outcome_in_either_order_and_keep_independent_errors() {
    for snapshot_first in [true, false] {
        for success in [true, false] {
            let (mut app, mut reducer) = acp_transcript_app();
            let mut state = WorkerReviewState::new(descriptor());
            let accepted = Review::InputAccepted {
                input: input(1, WorkerPromptKind::Initial, "initial"),
            };
            let dispatch = Review::Dispatched {
                generation: 1,
                attempt: 1,
            };
            for event in [accepted, dispatch] {
                state.apply(&event).unwrap();
                review(&mut app, &mut reducer, event);
            }
            let failure = (!success).then(|| "provider down".to_string());
            if !success {
                apply_agent_event(
                    &mut app,
                    &mut reducer,
                    AgentRunEvent::Failure {
                        error: "provider down".into(),
                    },
                );
            }
            let mut evidence = state.evidence.clone();
            evidence.failure = failure.clone();
            let settle = Review::Settled {
                generation: 1,
                failure,
                connected: true,
                evidence: Box::new(evidence),
            };
            state.apply(&settle).unwrap();
            if snapshot_first {
                reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
            }
            review(&mut app, &mut reducer, settle);
            reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
            assert_eq!(
                notices(&app).len(),
                1,
                "snapshot_first={snapshot_first}, success={success}"
            );
            if !success {
                assert!(!notices(&app)[0].1.contains("provider down"));
                assert!(!notices(&app)[0].1.contains("remains available"));
                assert!(blocks(&app).any(|block| matches!(&block.kind, PresentationBlockKind::Error(text) if text == "provider down")));
            }
            state.synthesis_error = Some("payload is too large".into());
            state.diagnostic = Some("connection unavailable".into());
            reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
            reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
            assert_eq!(notices(&app).len(), 3);
        }
    }
}

#[test]
fn status_changes_rebuild_only_the_header_and_appearance_invalidates_geometry() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::Initial, "input"),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: "# Heading\n\n**Markdown**".into(),
            message_id: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::ToolCall {
            id: "tool".into(),
            title: "deliberate external title".into(),
            kind: "execute".into(),
            status: "completed".into(),
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(json!({"command":"unchanged"})),
            raw_output: Some(json!("raw output")),
        },
    );
    let _ = rendered_buffer(&mut app, 100, 30);
    let rebuilt = app.view_cache().block_rebuilds;
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    let _ = rendered_buffer(&mut app, 100, 30);
    assert_eq!(app.view_cache().block_rebuilds, rebuilt + 1);
    let rebuilt = app.view_cache().block_rebuilds;
    review(
        &mut app,
        &mut reducer,
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
    );
    let _ = rendered_buffer(&mut app, 100, 30);
    assert_eq!(app.view_cache().block_rebuilds, rebuilt);
    let mut cache = crate::layout::ConversationCache::default();
    cache.refresh(
        app.history(),
        None,
        40,
        false,
        &crate::app::FoldState::default(),
    );
    let native = cache.block_rebuilds;
    cache.set_appearance(TranscriptAppearance::Acp);
    cache.refresh(
        app.history(),
        None,
        40,
        false,
        &crate::app::FoldState::default(),
    );
    assert!(cache.block_rebuilds > native);
    assert!(
        cache
            .entries()
            .iter()
            .flat_map(|entry| &entry.decorations)
            .any(|decoration| decoration.card)
    );
}

#[test]
fn literal_cards_share_measured_body_and_decoration_rows_at_all_widths() {
    let (mut app, mut reducer) = acp_transcript_app();
    let text = "  indented\n\n```not markdown```\ne\u{301} 👩‍💻 界\n";
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::UserFeedback, text),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: "assistant".into(),
            message_id: None,
        },
    );
    for width in [1, 2, 4, 6, 10, 40, 100] {
        let mut cache = crate::layout::ConversationCache::default();
        cache.set_appearance(TranscriptAppearance::Acp);
        cache.refresh(
            app.history(),
            cursor(0, 0).map(|selection| ActiveSelection {
                selection,
                scope: SelectionScope::Block,
            }),
            width,
            false,
            &crate::app::FoldState::default(),
        );
        let entry = &cache.entries()[0];
        let body = entry.items[0].1;
        let card = &entry.decorations[0];
        assert_eq!(card.rows.end(), body.end());
        assert!(card.rows.start() < body.start());
        assert_eq!(entry.selection, Some(body));
        assert_eq!(entry.items.len(), 2);
        assert!(
            entry
                .lines
                .iter()
                .take(body.end())
                .all(|line| line.width() <= usize::from(width))
        );
        assert!(
            entry
                .decorations
                .iter()
                .filter(|decoration| decoration.role == Some(PresentationRole::Assistant))
                .all(|decoration| !decoration.card && decoration.rows.start() >= body.end())
        );
        assert_eq!(
            entry.lines[body.end() - 1].to_string().trim(),
            "",
            "trailing blank preserved at width {width}"
        );
        assert_eq!(header(&app, 1).primary_copy(), text);
        assert_eq!(
            cache.selection_at_bottom(RowRange::new(card.rows.start(), body.start())),
            None
        );
        assert_eq!(cache.selection_at_bottom(body), cursor(0, 0));
    }
}

#[test]
fn cards_paint_panel_selection_and_bounded_role_accents() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(1, WorkerPromptKind::Initial, "card body\n\nsecond line"),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: "assistant answer".into(),
            message_id: None,
        },
    );
    let original = rendered_buffer(&mut app, 80, 20);
    for selected in [false, true, false] {
        app.select_for_test(if selected { cursor(0, 0) } else { None });
        let buffer = rendered_buffer(&mut app, 80, 20);
        let area = conversation_content_area(&buffer, true);
        let find = |needle: &str| {
            (0..buffer.area.height)
                .find(|&y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap()
        };
        let header_y = find("● You");
        let body_y = find("card body");
        let assistant_y = find("assistant answer");
        assert_conversation_row_background(&buffer, area, header_y, ZEVRIA_DARK.surfaces.panel);
        assert_eq!(buffer[(area.x, header_y)].symbol(), " ", "card inset");
        assert_eq!(buffer[(area.x + 1, header_y)].symbol(), "●");
        assert_eq!(buffer[(area.x, body_y)].symbol(), " ", "body inset");
        assert_eq!(buffer[(area.x + 1, body_y)].symbol(), "c");
        for y in body_y..=find("second line") {
            for x in area.x..area.right() {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.symbol(), original[(x, y)].symbol());
                assert_eq!(cell.modifier, original[(x, y)].modifier);
                assert_eq!(
                    cell.bg,
                    if selected {
                        ZEVRIA_DARK.surfaces.selection_background
                    } else {
                        ZEVRIA_DARK.surfaces.panel
                    }
                );
                assert_eq!(
                    cell.fg,
                    if selected {
                        ZEVRIA_DARK.surfaces.selection_foreground
                    } else {
                        original[(x, y)].fg
                    }
                );
            }
        }
        assert_blank_conversation_row(&buffer, find("● Assistant") - 1);
        assert_conversation_row_background(&buffer, area, assistant_y, ZEVRIA_DARK.surfaces.canvas);
        if !selected {
            assert_eq!(buffer, original, "deselection restores the entire card");
        }
        let gutter = area.x.saturating_sub(1 + crate::chrome::BLOCK_PAD_LEFT);
        assert_eq!(buffer[(gutter, header_y)].fg, ZEVRIA_DARK.roles.you);
        assert_eq!(
            buffer[(gutter, assistant_y)].fg,
            ZEVRIA_DARK.roles.assistant
        );
    }
}

#[test]
fn folded_cards_keep_their_panel_surface_and_insets() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(
                1,
                WorkerPromptKind::Initial,
                "first line\nsecond line\nthird line",
            ),
        },
    );
    let original = rendered_buffer(&mut app, 80, 20);
    app.select_message_for_test(cursor(0, 0));
    app.handle_event(key(KeyCode::Char('z')));
    app.handle_event(key(KeyCode::Char('c')));
    app.select_for_test(None);
    let folded = rendered_buffer(&mut app, 80, 20);
    let area = conversation_content_area(&folded, true);
    assert_eq!(app.view_cache().entries()[0].height, 2);
    assert!(buffer_row_text(&folded, area.y + 1).contains("▸ first line · 2 more rows"));
    assert_eq!(folded[(area.x + 1, area.y)].symbol(), "●");
    assert_eq!(folded[(area.x + 1, area.y + 1)].symbol(), "▸");
    for y in area.y..area.y + 2 {
        assert_eq!(folded[(area.x, y)].symbol(), " ");
        assert_conversation_row_background(&folded, area, y, ZEVRIA_DARK.surfaces.panel);
    }
    assert_blank_conversation_row(&folded, area.y + 2);
    app.handle_event(key(KeyCode::Char('z')));
    app.handle_event(key(KeyCode::Char('R')));
    assert_eq!(rendered_buffer(&mut app, 80, 20), original);
}

#[test]
fn user_message_surfaces_in_acp_do_not_require_card_metadata() {
    use super::selection_tests::plain_block;
    use crate::presentation::{ConversationEntry, PresentedDiagnostic};

    let mut diagnostic = plain_block(1, PresentationRole::User, "");
    diagnostic.role = None;
    diagnostic.visibility = BlockVisibility::Diagnostics;
    diagnostic.kind = PresentationBlockKind::Diagnostic(PresentedDiagnostic {
        label: "note".into(),
        text: "trace".into(),
        tone: DiagnosticTone::Muted,
    });
    let (mut app, _) = acp_transcript_app();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![
            plain_block(
                0,
                PresentationRole::User,
                "non-card user text wraps in narrow panes\n\nlast user line",
            ),
            diagnostic,
            plain_block(2, PresentationRole::User, "next user"),
            plain_block(3, PresentationRole::Assistant, "assistant body"),
        ],
    }));
    for width in [24, 80] {
        for diagnostics in [false, true, false] {
            if app.render_parts().diagnostics_visible != diagnostics {
                app.handle_event(key(KeyCode::Char('d')));
            }
            let buffer = rendered_buffer(&mut app, width, 40);
            let area = conversation_content_area(&buffer, true);
            let find = |needle: &str| {
                (area.y..area.bottom())
                    .find(|&y| buffer_row_text(&buffer, y).contains(needle))
                    .unwrap()
            };
            let assistant_header = find("● Assistant");
            let next_user = find("next user");
            let diagnostic_start = diagnostics.then(|| find("note"));
            assert_eq!(buffer[(area.x, area.y)].symbol(), "●", "no card inset");
            assert_eq!(buffer[(area.x, area.y + 1)].symbol(), "n", "no body inset");
            assert!(
                app.view_cache().entries()[0]
                    .decorations
                    .iter()
                    .all(|d| !d.card)
            );
            for y in area.y..area.bottom() {
                let roleless =
                    diagnostic_start.is_some_and(|start| (start..next_user).contains(&y));
                assert_conversation_row_background(
                    &buffer,
                    area,
                    y,
                    if y < assistant_header - 1 && !roleless {
                        ZEVRIA_DARK.surfaces.panel
                    } else {
                        ZEVRIA_DARK.surfaces.canvas
                    },
                );
            }
            assert_blank_conversation_row(&buffer, assistant_header - 1);
        }
    }
}

#[test]
fn failed_followups_only_offer_a_retained_proposal_when_available() {
    for invalidation in ["none", "removed", "prose", "payload"] {
        let (mut app, mut reducer) = acp_transcript_app();
        review(
            &mut app,
            &mut reducer,
            Review::InputAccepted {
                input: input(1, WorkerPromptKind::Initial, "initial"),
            },
        );
        review(
            &mut app,
            &mut reducer,
            Review::Dispatched {
                generation: 1,
                attempt: 1,
            },
        );
        review(
            &mut app,
            &mut reducer,
            Review::Published {
                generation: 1,
                plan: plan("plan", Some("complete")),
                replay: false,
            },
        );
        review(&mut app, &mut reducer, settled(1, None));
        assert!(notices(&app).is_empty());
        if invalidation == "removed" {
            review(
                &mut app,
                &mut reducer,
                Review::Removed {
                    plan_id: "plan".into(),
                },
            );
        }
        if invalidation == "payload" {
            review(
                &mut app,
                &mut reducer,
                Review::PayloadChecked {
                    error: Some("too large".into()),
                },
            );
        }
        let generation = if invalidation == "prose" {
            review(
                &mut app,
                &mut reducer,
                Review::InputAccepted {
                    input: input(2, WorkerPromptKind::UserFeedback, "discussion"),
                },
            );
            review(
                &mut app,
                &mut reducer,
                Review::Dispatched {
                    generation: 2,
                    attempt: 2,
                },
            );
            review(&mut app, &mut reducer, settled(2, None));
            3
        } else {
            2
        };
        review(
            &mut app,
            &mut reducer,
            Review::InputAccepted {
                input: input(
                    generation,
                    WorkerPromptKind::UserFeedback,
                    "failed feedback",
                ),
            },
        );
        review(
            &mut app,
            &mut reducer,
            Review::Dispatched {
                generation,
                attempt: generation,
            },
        );
        review(&mut app, &mut reducer, settled(generation, Some("failed")));
        let notices = notices(&app);
        let failure = notices
            .iter()
            .find(|(_, text)| text.contains("Feedback was not incorporated"))
            .unwrap();
        assert_eq!(
            failure.1.contains("remains available"),
            invalidation == "none",
            "{invalidation}: {failure:?}"
        );
    }
}

#[test]
fn snapshot_does_not_invent_dispatches_or_overwrite_recovery_attempts() {
    let (mut app, mut reducer) = acp_transcript_app();
    let mut state = WorkerReviewState::new(descriptor());
    let accepted = Review::InputAccepted {
        input: input(1, WorkerPromptKind::Initial, "input"),
    };
    state.apply(&accepted).unwrap();
    review(&mut app, &mut reducer, accepted);
    state
        .apply(&Review::Dispatched {
            generation: 1,
            attempt: 1,
        })
        .unwrap();
    state
        .apply(&Review::Recovering {
            generation: 1,
            attempt: 2,
        })
        .unwrap();
    reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
    for event in [
        Review::Dispatched {
            generation: 1,
            attempt: 1,
        },
        Review::Recovering {
            generation: 1,
            attempt: 2,
        },
    ] {
        review(&mut app, &mut reducer, event);
    }
    assert_eq!(phase(&app, 1), Some(PromptPhase::Recovering));
    let revision = header(&app, 1).revision;
    reducer.apply_review_snapshot(app.conversation_projection_mut(), &state);
    assert_eq!(header(&app, 1).revision, revision);
}

#[test]
fn partial_viewports_clip_card_surfaces_and_accents_using_cached_rows() {
    let (mut app, mut reducer) = acp_transcript_app();
    review(
        &mut app,
        &mut reducer,
        Review::InputAccepted {
            input: input(
                1,
                WorkerPromptKind::Initial,
                "one\ntwo\nthree\nfour\nfive\nsix",
            ),
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::AgentMessage {
            text: "assistant\n\nsecond paragraph".into(),
            message_id: None,
        },
    );
    for diagnostics in [false, true] {
        if diagnostics {
            app.handle_event(key(KeyCode::Char('d')));
        }
        for width in [12, 30, 100] {
            let _ = rendered_buffer(&mut app, width, 30);
            let row = app.view_cache().entries()[0].items[0].1.start() + 1;
            app.set_view_for_test(row, false);
            let buffer = rendered_buffer(&mut app, width, 7);
            let area = conversation_content_area(&buffer, true);
            let top = app.view_scroll();
            let entry = &app.view_cache().entries()[0];
            for local in 0..usize::from(area.height) {
                let physical = top + local;
                let decoration = entry.decorations.iter().find(|decoration| {
                    decoration.rows.start() <= physical && physical < decoration.rows.end()
                });
                let y = area.y + local as u16;
                let expected_bg = if decoration.is_some_and(|decoration| decoration.card) {
                    ZEVRIA_DARK.surfaces.panel
                } else {
                    ZEVRIA_DARK.surfaces.canvas
                };
                assert_eq!(
                    buffer[(area.x, y)].bg,
                    expected_bg,
                    "width {width} row {physical}"
                );
                if let Some(role) = decoration.and_then(|decoration| decoration.role) {
                    let color = match role {
                        PresentationRole::User => ZEVRIA_DARK.roles.you,
                        PresentationRole::Assistant => ZEVRIA_DARK.roles.assistant,
                        PresentationRole::System => ZEVRIA_DARK.roles.system,
                    };
                    assert_eq!(
                        buffer[(area.x - 1 - crate::chrome::BLOCK_PAD_LEFT, y)].fg,
                        color
                    );
                }
            }
        }
    }
    for (width, height) in [(1, 1), (2, 4), (4, 2), (8, 8)] {
        let _ = rendered_buffer(&mut app, width, height);
    }
}

#[test]
fn native_status_spans_share_icons_and_colors_without_cards() {
    for name in ["skill", "command", "write", "generic"] {
        let args = match name {
            "skill" => json!({"skill":"test"}),
            "command" => json!({"command":"echo hi"}),
            "write" => json!({"file_path":"file.txt","content":"text"}),
            _ => json!({}),
        };
        for (status, outcome, glyph, color) in [
            (
                ToolCallStatus::Executing,
                None,
                "◐",
                ZEVRIA_DARK.feedback.info,
            ),
            (
                ToolCallStatus::Interrupted,
                None,
                "◼",
                ZEVRIA_DARK.feedback.warning,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Success),
                "✓",
                ZEVRIA_DARK.feedback.success,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Denied),
                "⊘",
                ZEVRIA_DARK.feedback.error,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Error),
                "✗",
                ZEVRIA_DARK.feedback.error,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Cancelled),
                "◼",
                ZEVRIA_DARK.feedback.warning,
            ),
        ] {
            let message = assistant_message(vec![tool_call("id", None, name, args.clone())]);
            let mut state = crate::app::ToolCallState::new(status, &args);
            state.metadata =
                outcome.map(|outcome| file_metadata("id", None, name, outcome, Vec::new()));
            let mut lines = Vec::new();
            layout_native_message(&message, Some(&[Some(state)]), &mut lines, 100, None);
            assert_eq!(lines[1].spans.last().unwrap().content, glyph);
            assert_eq!(
                lines[1].spans.last().unwrap().style.fg,
                Some(color),
                "{name} {status:?}"
            );
            let entry = HistoryEntry::from_message(message, status).unwrap();
            let mut cache = crate::layout::ConversationCache::default();
            cache.refresh(
                &[entry],
                None,
                100,
                false,
                &crate::app::FoldState::default(),
            );
            assert!(
                cache.entries()[0]
                    .decorations
                    .iter()
                    .all(|decoration| !decoration.card)
            );
        }
    }
}

#[test]
fn semantic_status_colors_cover_acp_and_native_without_changing_copy() {
    use crate::presentation::PresentedToolStatus as Status;
    for (status, color) in [
        (Status::Completed, ZEVRIA_DARK.feedback.success),
        (Status::Running, ZEVRIA_DARK.feedback.info),
        (Status::Failed, ZEVRIA_DARK.feedback.error),
        (Status::Denied, ZEVRIA_DARK.feedback.error),
        (Status::Interrupted, ZEVRIA_DARK.feedback.warning),
        (Status::Pending, ZEVRIA_DARK.text.muted),
        (Status::Unknown, ZEVRIA_DARK.text.muted),
    ] {
        assert_eq!(status.icon().color(), color);
    }
    for (status, glyph, color) in [
        ("completed", "✓", ZEVRIA_DARK.feedback.success),
        ("finished", "✓", ZEVRIA_DARK.feedback.success),
        ("in_progress", "◐", ZEVRIA_DARK.feedback.info),
        ("running", "◐", ZEVRIA_DARK.feedback.info),
        ("failed", "✗", ZEVRIA_DARK.feedback.error),
        ("error", "✗", ZEVRIA_DARK.feedback.error),
        ("denied", "⊘", ZEVRIA_DARK.feedback.error),
        ("rejected", "⊘", ZEVRIA_DARK.feedback.error),
        ("interrupted", "◼", ZEVRIA_DARK.feedback.warning),
        ("cancelled", "◼", ZEVRIA_DARK.feedback.warning),
        ("pending", "○", ZEVRIA_DARK.text.muted),
        ("other", "?", ZEVRIA_DARK.text.muted),
    ] {
        let (mut app, mut reducer) = acp_transcript_app();
        apply_agent_event(
            &mut app,
            &mut reducer,
            AgentRunEvent::ToolCall {
                id: "tool".into(),
                title: "Deliberate title".into(),
                kind: "execute".into(),
                status: status.into(),
                content: vec!["bytes\noutput".into()],
                locations: Vec::new(),
                raw_input: Some(json!("exact params\n")),
                raw_output: None,
            },
        );
        let mut cache = crate::layout::ConversationCache::default();
        cache.refresh(
            app.history(),
            None,
            100,
            false,
            &crate::app::FoldState::default(),
        );
        let line = &cache.entries()[0].lines[1];
        assert_eq!(
            line.to_string(),
            format!("◆ execute · Deliberate title {glyph}")
        );
        assert_eq!(line.spans.last().unwrap().style.fg, Some(color));
        let tool = blocks(&app).next().unwrap();
        assert_eq!(tool.primary_copy(), "exact params\n");
        assert_eq!(tool.secondary_copy().as_deref(), Some("bytes\noutput"));
    }
}
