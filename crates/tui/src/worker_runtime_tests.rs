#[path = "worker_file_reference_tests.rs"]
mod file_reference_tests;

use super::*;
use zevria_foundation::TurnId;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::WorkerControl;
use zevria_workflow::WorkerControlAction;
use zevria_workflow::WorkerControlId;
use zevria_workflow::WorkerControlResult;
use zevria_workflow::WorkerControlTarget;
use zevria_workflow::WorkerReviewEvent;
use zevria_workflow::WorkerReviewState;

fn fixture(seal: bool) -> (EnsembleStart, Vec<TranscriptItem>) {
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "a".into(),
        label: "Worker A".into(),
        safe_mode: "read-only".into(),
    };
    let mut start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan".into(),
        agents: vec![descriptor.clone()],
    };
    let control = WorkerControl {
        request_id: WorkerControlId::new(),
        target: WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: start.run_id.clone(),
            worker_id: descriptor.id.clone(),
        },
        action: WorkerControlAction::Abandon,
    };
    let event = WorkerReviewEvent::Abandoned {
        request_id: control.request_id.clone(),
    };
    let result = WorkerControlResult {
        control,
        accepted: true,
        detail: "excluded".into(),
    };
    let mut middle = Vec::new();
    let record = if seal {
        let survivor = AgentRunDescriptor {
            id: AgentRunId::new(),
            agent: "b".into(),
            label: "Worker B".into(),
            safe_mode: "read-only".into(),
        };
        start.agents.push(survivor.clone());
        let mut surviving = WorkerReviewState::new(survivor.clone());
        for event in [
            WorkerReviewEvent::InputAccepted {
                input: zevria_workflow::WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: zevria_workflow::WorkerPromptKind::Initial,
                    text: start.prompt.clone(),
                },
            },
            WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            },
            WorkerReviewEvent::Published {
                generation: 1,
                plan: zevria_workflow::AgentStructuredPlan {
                    plan_id: None,
                    markdown: Some("# Surviving plan".into()),
                    entries: vec![],
                },
                replay: false,
            },
            WorkerReviewEvent::Settled {
                generation: 1,
                failure: None,
                connected: true,
                evidence: Box::new(surviving.evidence.clone()),
            },
        ] {
            surviving.apply(&event).unwrap();
            middle.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: survivor.id.clone(),
                event: Box::new(event),
                result: None,
            }));
        }
        let receipt = zevria_workflow::WorkerConfirmationReceipt {
            request_id: WorkerControlId::new(),
            target: WorkerControlTarget {
                turn_id: TurnId::new(1),
                run_id: start.run_id.clone(),
                worker_id: survivor.id.clone(),
            },
            revision: surviving.eligible_snapshot().unwrap().revision.clone(),
        };
        let confirmation = WorkerReviewEvent::Confirmed {
            receipt: receipt.clone(),
        };
        surviving.apply(&confirmation).unwrap();
        middle.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: survivor.id,
            event: Box::new(confirmation),
            result: Some(WorkerControlResult {
                control: WorkerControl {
                    request_id: receipt.request_id,
                    target: receipt.target,
                    action: WorkerControlAction::Confirm {
                        expected_revision: receipt.revision,
                    },
                },
                accepted: true,
                detail: "confirmed".into(),
            }),
        }));
        let mut state = WorkerReviewState::new(descriptor);
        state.apply(&event).unwrap();
        EnsembleRecord::WorkersConfirmed {
            run_id: start.run_id.clone(),
            final_confirmation: result,
            outcomes: vec![state.outcome(), surviving.outcome()],
        }
    } else {
        EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: descriptor.id,
            event: Box::new(event),
            result: Some(result),
        }
    };
    let mut items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: zevria_workflow::ENSEMBLE_REVIEW_VERSION,
        }),
    ];
    items.extend(middle);
    items.push(TranscriptItem::Ensemble(record));
    zevria_transcript::validate_ensemble_review_history(&items).unwrap();
    (start, items)
}

#[test]
fn runtime_review_snapshot_and_mirror_share_toned_outcome_notices() {
    use crate::presentation::{
        BlockVisibility, DiagnosticTone, PresentationBlockKind, PromptPhase,
    };
    for snapshot_first in [false, true] {
        let (start, _) = fixture(false);
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        views.apply(SessionEvent::EnsembleStarted {
            turn_id: TurnId::new(1),
            start: start.clone(),
            resumed: false,
        });
        let mut state = WorkerReviewState::new(start.agents[0].clone());
        queue_input(&mut state, WorkerControlId::new(), "literal feedback");
        views.agents[0].apply_transcript_event(AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::InputAccepted {
                input: state.pending[0].clone(),
            }),
        });
        views.agents[0].apply_transcript_event(AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            }),
        });
        publish(&mut views, &start, &state);
        state
            .apply(&WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            })
            .unwrap();
        publish(&mut views, &start, &state);
        let mut evidence = state.evidence.clone();
        evidence.failure = Some("provider unavailable".into());
        let event = WorkerReviewEvent::Settled {
            generation: 1,
            failure: evidence.failure.clone(),
            connected: false,
            evidence: Box::new(evidence),
        };
        state.apply(&event).unwrap();
        if snapshot_first {
            publish(&mut views, &start, &state);
        }
        views.apply(SessionEvent::AgentRunUpdated {
            turn_id: TurnId::new(1),
            ensemble_run_id: start.run_id.clone(),
            agent_run_id: state.descriptor.id.clone(),
            event: AgentRunEvent::Review {
                event: Box::new(event),
            },
        });
        publish(&mut views, &start, &state);
        let blocks = views.agents[0]
            .app
            .history()
            .iter()
            .filter_map(|entry| match entry {
                crate::app::HistoryEntry::Conversation(entry) => Some(&entry.blocks),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        let notices = blocks
            .iter()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::Diagnostic(notice)
                    if block.visibility == BlockVisibility::Always =>
                {
                    Some(notice)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].tone, DiagnosticTone::Error);
        assert!(notices[0].text.contains("provider unavailable"));
        assert!(!notices[0].text.contains("remains available"));
        assert_eq!(
            blocks
                .iter()
                .filter_map(|block| block.prompt.as_ref())
                .next()
                .unwrap()
                .phase,
            Some(PromptPhase::Failed)
        );
        assert!(
            !views.agents[0]
                .app
                .history()
                .iter()
                .any(|entry| matches!(entry, crate::app::HistoryEntry::Error(_)))
        );
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(ratatui::crossterm::event::KeyEvent::new(code, modifiers))
}

fn queue_input(state: &mut WorkerReviewState, request_id: WorkerControlId, text: &str) {
    let generation = state.accepted_generation + 1;
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: zevria_workflow::WorkerInput {
                generation,
                request_id,
                kind: if generation == 1 {
                    zevria_workflow::WorkerPromptKind::Initial
                } else {
                    zevria_workflow::WorkerPromptKind::UserFeedback
                },
                text: text.into(),
            },
        })
        .unwrap();
}

fn settle(state: &mut WorkerReviewState) {
    let generation = state.accepted_generation;
    state
        .apply(&WorkerReviewEvent::Dispatched {
            generation,
            attempt: 1,
        })
        .unwrap();
    state
        .apply(&WorkerReviewEvent::Settled {
            generation,
            failure: None,
            connected: true,
            evidence: Box::new(state.evidence.clone()),
        })
        .unwrap();
}

fn publish(views: &mut SessionViews, start: &EnsembleStart, state: &WorkerReviewState) {
    views.apply(SessionEvent::WorkerReviewUpdated {
        target: WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
        },
        state: Box::new(state.clone()),
    });
}

#[test]
fn visible_selection_uses_workspace_header_geometry_in_root_worker_and_inspect_panes() {
    fn entries() -> Vec<TranscriptItem> {
        (0..8)
            .map(|index| {
                TranscriptItem::Message(rig_core::message::Message::user(format!("item {index}")))
            })
            .collect()
    }
    fn escape_pair(app: &mut App) {
        let now = std::time::Instant::now();
        app.handle_event_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
        app.handle_event_at(
            key(KeyCode::Esc, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(100),
        );
    }
    let mut root = App::new();
    root.restore(entries());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    terminal.draw(|frame| root.render(frame)).unwrap();
    let mut views = SessionViews::new(root, PathBuf::from("."));
    assert_eq!(
        views.root.rendered_selection_window(),
        None,
        "standalone geometry cannot include the workspace header"
    );
    let (start, _) = fixture(false);
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    let mut state = WorkerReviewState::new(start.agents[0].clone());
    queue_input(&mut state, WorkerControlId::new(), "initial");
    settle(&mut state);
    publish(&mut views, &start, &state);
    views.agents[0].app.restore(entries());
    let child_id = SubtaskId::new("selection-inspect");
    views.restore_child(child_id.clone(), None, entries());

    for pane in 0..3 {
        match pane {
            0 => {}
            1 => views.open_agent(&start.agents[0].id),
            2 => views.open_child(&child_id),
            _ => unreachable!(),
        }
        assert_eq!(views.visible_app().rendered_selection_window(), None);
        let before = (
            views.visible_app().view_scroll(),
            views.visible_app().view_follow(),
        );
        escape_pair(views.visible_app_mut());
        assert!(views.visible_app().interaction().is_normal());
        assert_eq!(
            (
                views.visible_app().view_scroll(),
                views.visible_app().view_follow()
            ),
            before
        );
        for (width, height, input, composer_rows) in
            [(80, 10, "", 3), (36, 16, "one\ntwo\nthree", 5)]
        {
            views.visible_app_mut().set_view_for_test(3, false);
            views.visible_app_mut().set_input_for_test(input, 0);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| views.render(frame)).unwrap();
            let inspect = matches!(
                views.visible_app_mut().render_parts().chrome,
                crate::app::ComposerChrome::Inspect
            );
            assert_eq!(inspect, pane == 2);
            let lower = if inspect {
                crate::frame_layout::LowerSurface::None
            } else {
                crate::frame_layout::LowerSurface::Composer {
                    requested_height: composer_rows,
                }
            };
            let layout = crate::frame_layout::FrameLayout::compute(
                terminal.backend().buffer().area,
                inspect,
                lower,
                true,
            );
            assert!(layout.workspace_header_enabled);
            let window = crate::viewport::RowRange::from_start_len(
                3,
                usize::from(layout.conversation_content.height),
            );
            assert_eq!(
                views.visible_app().rendered_selection_window(),
                Some(window)
            );
            escape_pair(views.visible_app_mut());
            let expected = crate::app::Selection {
                history_index: (window.end() - 2) / 3,
                content_index: 0,
            };
            assert_eq!(views.visible_app().selection(), Some(expected));
            assert_eq!(
                views
                    .visible_app_mut()
                    .handle_event(key(KeyCode::Char('y'), KeyModifiers::NONE)),
                Some(UiAction::Copy {
                    text: format!("item {}", expected.history_index)
                })
            );
            terminal.draw(|frame| views.render(frame)).unwrap();
            assert_eq!(views.visible_app().view_scroll(), 3);
            assert!(!views.visible_app().view_follow());
            assert!(!views.visible_app().interaction().selection_reveal());
            views
                .visible_app_mut()
                .handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        }
    }
    views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(
        views.root.rendered_selection_window(),
        None,
        "returning to a previously rendered pane waits for redraw"
    );
    escape_pair(&mut views.root);
    assert!(views.root.interaction().is_normal());
}

#[test]
fn pending_clipboard_completion_targets_the_originating_worker_not_the_visible_pane() {
    let (start, _) = fixture(true);
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    for descriptor in &start.agents {
        let mut state = WorkerReviewState::new(descriptor.clone());
        queue_input(&mut state, WorkerControlId::new(), "initial");
        settle(&mut state);
        publish(&mut views, &start, &state);
    }
    views.open_agent(&start.agents[0].id);
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("first ".into()));
    let Some(UiAction::ReadClipboard { generation, cursor }) =
        views.handle_event(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
    else {
        panic!("paste action");
    };
    let origin = views.clipboard_origin().unwrap();
    views.open_agent(&start.agents[1].id);
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("second".into()));
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    views.clipboard_completed(
        origin.clone(),
        generation,
        cursor,
        crate::clipboard::ClipboardResult::Image(image.clone()),
    );
    assert_eq!(views.visible_app().input(), "second");
    views.open_agent(&start.agents[0].id);
    assert_eq!(views.visible_app().input(), "first [image 1]");
    // A retired pane invalidates a pending result even if its identity still exists.
    let Some(UiAction::ReadClipboard { generation, cursor }) =
        views.handle_event(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
    else {
        panic!("second paste action");
    };
    views.visible_app_mut().freeze_worker();
    views.clipboard_completed(
        origin,
        generation,
        cursor,
        crate::clipboard::ClipboardResult::Image(image),
    );
    assert_eq!(views.visible_app().input(), "first [image 1]");
}

#[test]
fn live_workers_have_independent_admission_and_drafts_through_real_navigation() {
    let (start, _) = fixture(true);
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    let mut states = start
        .agents
        .iter()
        .map(|descriptor| {
            let mut state = WorkerReviewState::new(descriptor.clone());
            queue_input(&mut state, WorkerControlId::new(), "initial");
            settle(&mut state);
            publish(&mut views, &start, &state);
            state
        })
        .collect::<Vec<_>>();
    assert!(views.root.is_busy());
    views.root.set_input_for_test("root draft", 4);

    // Select the first worker from the active root ensemble, without directly
    // setting focus or the visible pane.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| views.render(frame)).unwrap();
    let now = std::time::Instant::now();
    views
        .visible_app_mut()
        .handle_event_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
    views.visible_app_mut().handle_event_at(
        key(KeyCode::Esc, KeyModifiers::NONE),
        now + std::time::Duration::from_millis(100),
    );
    views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    views.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
    views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(views.visible_agent_id(), Some(&start.agents[0].id));
    assert!(views.visible_app().interaction().is_normal());
    views.handle_event(Event::Paste("not in Insert".into()));
    assert!(views.visible_app().input().is_empty());
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("first\nfeedback".into()));
    let Some(UiAction::WorkerControl(first)) =
        views.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("first worker feedback")
    };
    assert_eq!(first.target.worker_id, start.agents[0].id);
    assert!(
        matches!(&first.action, WorkerControlAction::SendFeedback { text } if text == &zevria_content::UserPrompt::from_text("first\nfeedback"))
    );
    assert!(!views.visible_app_mut().render_parts().composer_locked);
    assert!(views.visible_app().interaction().is_normal());
    terminal.draw(|frame| views.render(frame)).unwrap();
    assert!(!terminal.backend().cursor_visible());
    views.handle_event(key(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(views.visible_app().input(), "first\nfeedback");

    views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    views.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
    views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(views.visible_agent_id(), Some(&start.agents[1].id));
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("second draft".into()));
    assert!(!views.visible_app_mut().render_parts().composer_locked);
    queue_input(
        &mut states[1],
        WorkerControlId::new(),
        "restored queued feedback",
    );
    publish(&mut views, &start, &states[1]);
    assert!(views.visible_app().interaction().is_insert());
    assert!(!views.visible_app_mut().render_parts().composer_locked);
    views.handle_event(Event::Paste(" newer".into()));
    views.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(views.visible_app().input(), "second draft");

    // Hidden worker acknowledgements and snapshots do not alter this draft or
    // borrow the active root turn's busy state.
    views.apply(SessionEvent::WorkerControlResult {
        result: WorkerControlResult {
            control: first.clone(),
            accepted: true,
            detail: "accepted".into(),
        },
    });
    assert!(views.agents[0].app.input().is_empty());
    assert!(!views.agents[0].app.render_parts().composer_locked);
    queue_input(&mut states[0], first.request_id, "first\nfeedback");
    publish(&mut views, &start, &states[0]);
    settle(&mut states[0]);
    publish(&mut views, &start, &states[0]);
    assert!(views.agents[0].app.interaction().is_normal());
    assert!(views.visible_app().interaction().is_insert());
    assert_eq!(views.visible_app().input(), "second draft");
    assert_eq!(views.root.input(), "root draft");
    assert!(views.root.is_busy());

    views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    views.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
    views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(views.visible_agent_id(), Some(&start.agents[0].id));
    assert!(views.visible_app().interaction().is_normal());
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert!(views.visible_app().interaction().is_insert());
    views.handle_event(Event::Paste("/re".into()));
    assert!(views.visible_app().command_menu_active());
    assert!(
        views
            .handle_event(key(KeyCode::Tab, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(
        views.visible_app().input(),
        "/retry ",
        "worker completion owns Tab"
    );
    assert!(!views.visible_app_mut().render_parts().composer_locked);
    assert_eq!(views.visible_agent_id(), Some(&start.agents[0].id));

    // The second worker settles while hidden, preserving its draft and chosen Insert focus.
    settle(&mut states[1]);
    publish(&mut views, &start, &states[1]);
    views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    views.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
    views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(views.visible_app().input(), "second draft");
    assert!(views.visible_app().interaction().is_insert());
    assert!(!views.visible_app_mut().render_parts().composer_locked);
    let Some(UiAction::WorkerControl(second)) =
        views.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("second worker feedback")
    };
    assert_eq!(second.target.worker_id, start.agents[1].id);
    assert!(views.visible_app().interaction().is_normal());
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert!(views.visible_app().interaction().is_insert());
    assert!(
        views
            .handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(views.visible_app().input().is_empty());
    let Some(UiAction::WorkerControl(cancel)) =
        views.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
    else {
        panic!("worker cancellation, never root")
    };
    assert_eq!(cancel.target, second.target);
    assert_eq!(cancel.action, WorkerControlAction::CancelPrompt);
    assert!(views.root.is_busy());
    assert_eq!(views.agents[0].app.input(), "/retry ");

    views.apply(SessionEvent::TurnCancelled {
        turn_id: TurnId::new(1),
    });
    assert!(matches!(
        views.visible_app_mut().render_parts().chrome,
        crate::app::ComposerChrome::Inspect
    ));
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("ended run".into()));
    assert!(views.visible_app().input().is_empty());
    assert_eq!(views.agents[0].app.input(), "/retry ");
}

#[test]
fn worker_abandonment_restores_from_root_without_creating_a_missing_sidecar_pane() {
    for seal in [false, true] {
        let (start, items) = fixture(seal);
        let mut root = App::new();
        root.restore(items.clone());
        let mut views = SessionViews::new(root, PathBuf::from("."));
        views.seed_child_creation_order(&items);
        assert!(views.agents.is_empty());
        let crate::app::HistoryEntry::Ensemble(ensemble) = &views.root.history()[0] else {
            panic!("ensemble")
        };
        assert_eq!(ensemble.workers[0].status, AgentRunStatus::Abandoned);
        let mut lines = Vec::new();
        let items = crate::layout::prepare::render_ensemble(
            ensemble,
            &mut lines,
            100,
            crate::layout::EntrySelection::Block(1),
            &crate::app::EntryFolds::none(),
            false,
        );
        assert_eq!(items.len(), ensemble.workers.len() + 1);
        assert!(!items[1].is_empty(), "excluded rows stay selectable");
        assert!(
            items[1].start() > items[0].end(),
            "confirmation summary is excluded"
        );
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(if seal {
            "1/1 workers confirmed · 1 abandoned"
        } else {
            "cancelled · no participating workers"
        }));
        assert!(!text.contains("0/0"));
        assert!(text.contains("abandoned"));
        views.apply(SessionEvent::EnsembleStarted {
            turn_id: TurnId::new(1),
            start: start.clone(),
            resumed: true,
        });
        assert!(
            !views
                .agents
                .iter()
                .any(|pane| pane.id == start.agents[0].id),
            "root restoration does not fabricate an abandoned actor pane"
        );
        views.restore_agent_run(vec![
            AgentRunTranscriptRecord::Header {
                header: zevria_transcript::AgentRunTranscriptHeader {
                    version: zevria_transcript::AGENT_RUN_TRANSCRIPT_VERSION,
                    ensemble_run_id: start.run_id,
                    workflow: start.workflow,
                    prompt: start.prompt,
                    descriptor: start.agents[0].clone(),
                },
            },
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::AgentMessage {
                    text: "archived discussion".into(),
                    message_id: None,
                },
            },
        ]);
        assert_eq!(views.agents.len(), start.agents.len());
        let pane = views
            .agents
            .iter()
            .find(|pane| pane.id == start.agents[0].id)
            .unwrap();
        assert_eq!(pane.status, AgentRunStatus::Abandoned);
        assert!(pane.historical);
        assert!(format!("{:?}", pane.app.history()).contains("archived discussion"));
        assert!(format!("{:?}", pane.app.history()).contains("Permanently abandoned"));
    }
}

#[test]
fn worker_abandonment_freezes_live_rows_and_panes_against_late_telemetry() {
    let (start, _) = fixture(false);
    let turn_id = TurnId::new(1);
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id,
        start: start.clone(),
        resumed: false,
    });
    let mut state = WorkerReviewState::new(start.agents[0].clone());
    state
        .apply(&WorkerReviewEvent::Abandoned {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    views.apply(SessionEvent::WorkerReviewUpdated {
        target: WorkerControlTarget {
            turn_id,
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
        },
        state: Box::new(state.clone()),
    });
    let before = views.agents[0].app.history().len();
    for event in [
        AgentRunEvent::Status {
            status: AgentRunStatus::Failed,
            detail: None,
        },
        AgentRunEvent::Plan {
            plan: zevria_workflow::AgentStructuredPlan {
                plan_id: None,
                markdown: Some("# LATE EXCLUDED".into()),
                entries: vec![],
            },
        },
        AgentRunEvent::AgentMessage {
            text: "LATE EXCLUDED".into(),
            message_id: None,
        },
    ] {
        views.apply(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: start.run_id.clone(),
            agent_run_id: state.descriptor.id.clone(),
            event,
        });
    }
    views.agents[0].apply_transcript_preview(AgentRunEvent::AgentMessage {
        text: "LATE PREVIEW".into(),
        message_id: None,
    });
    let mut outcome = state.evidence.clone();
    outcome.status = AgentRunStatus::Failed;
    outcome.report = "LATE REPORT".into();
    views.apply(SessionEvent::AgentRunFinished {
        turn_id,
        ensemble_run_id: start.run_id,
        outcome,
    });
    assert_eq!(views.agents[0].status, AgentRunStatus::Abandoned);
    assert_eq!(views.agents[0].app.history().len(), before);
    let crate::app::HistoryEntry::Ensemble(ensemble) = &views.root.history()[0] else {
        panic!("ensemble")
    };
    assert_eq!(ensemble.workers[0].status, AgentRunStatus::Abandoned);
}
