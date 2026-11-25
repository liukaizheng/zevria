use super::*;
use crate::runtime::{dispatch_turn_action, drain_update_burst};
use zevria_host::{SessionStart, runtime::start_session, test_support::BlockedPromptCount};
use zevria_presentation::PREPARING_PROMPT;
use zevria_session_api::TurnEvent;
use zevria_session_api::{SessionCommand, SessionUpdate};

const PROMPT: &str = "Show the draft before durable admission";

fn submit(views: &mut SessionViews, text: &str) -> UiAction {
    views
        .workspace
        .root
        .set_focus_for_test(crate::app::FocusState::Insert);
    views.workspace.root.set_input_for_test(text, text.len());
    views
        .handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("valid prompt")
}

fn frame(views: &mut SessionViews) -> String {
    let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
    terminal.draw(|frame| views.render(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(90)
        .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn first_frame_echoes_the_prompt_while_provider_count_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let mut provider = BlockedPromptCount::start().await;
    let mut running = start_session(
        &provider.config(&workspace),
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let path = running.restored.transcript_path.clone();
    let commands = running.running.command_sender();
    let mut updates = running.running.take_event_receiver().unwrap();
    let mut views = SessionViews::new(App::new(), workspace);
    let mut done = false;
    let action = submit(&mut views, PROMPT);
    dispatch_turn_action(&mut views, &commands, &mut done, action).unwrap();
    let deadline = Duration::from_secs(15);
    tokio::time::timeout(deadline, &mut provider.count_started)
        .await
        .unwrap()
        .unwrap();
    drain_update_burst(&mut views, &mut updates, &mut done).unwrap();
    assert!(!done);
    let pending = frame(&mut views);
    assert!(
        pending.contains(PREPARING_PROMPT) && pending.contains(PROMPT),
        "{pending}"
    );
    assert!(!pending.contains("You · #1"));
    assert_eq!(views.workspace.root.input(), "");
    assert!(views.workspace.root.history().is_empty());
    assert!(
        !path.exists(),
        "the pending transcript is still unmaterialized"
    );
    assert!(matches!(
        provider.completion_started.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    provider.release_count.take().unwrap().send(()).unwrap();
    tokio::time::timeout(deadline, async {
        loop {
            if let SessionUpdate::Lifecycle(event) = updates.recv().await.unwrap() {
                let started = matches!(event, SessionEvent::Turn(TurnEvent::Started { .. }));
                views.apply(event);
                if started {
                    break;
                }
            }
        }
        (&mut provider.completion_started).await.unwrap();
    })
    .await
    .unwrap();
    let accepted = frame(&mut views);
    assert!(!accepted.contains(PREPARING_PROMPT));
    assert!(accepted.contains("You · #1"));
    assert_eq!(accepted.matches(PROMPT).count(), 1);
    assert!(matches!(
        views.workspace.root.history(),
        [HistoryEntry::Conversation(_)]
    ));
    assert_eq!(
        zevria_transcript::transcript::load(&path)
            .unwrap()
            .iter()
            .filter(|item| item.display_message() == Some(Message::user(PROMPT)))
            .count(),
        1
    );
    running.running.shutdown().await.unwrap();
}

#[test]
fn transport_loss_settles_unaccepted_prompts_through_draft_recovery() {
    for failed_send in [false, true] {
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel::<SessionCommand>();
        let mut done = false;
        let action = submit(&mut views, PROMPT);
        if failed_send {
            drop(receiver);
            dispatch_turn_action(&mut views, &commands, &mut done, action).unwrap();
            assert_eq!(views.workspace.root.input(), PROMPT);
        } else {
            dispatch_turn_action(&mut views, &commands, &mut done, action).unwrap();
            assert!(frame(&mut views).contains(PREPARING_PROMPT));
            views.workspace.root.set_input_for_test("newer draft", 11);
            let (events, mut updates) = zevria_session_api::session_event_channel(4);
            drop(events);
            drain_update_burst(&mut views, &mut updates, &mut done).unwrap();
            assert_eq!(views.workspace.root.input(), "newer draft");
            views.workspace.root.set_input_for_test("", 0);
            views.workspace.root.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
            )));
            assert_eq!(views.workspace.root.input(), PROMPT);
        }
        assert!(done && !views.workspace.root.is_busy());
        assert!(!frame(&mut views).contains(PREPARING_PROMPT));
        assert!(
            matches!(views.workspace.root.history(), [HistoryEntry::Error(error)] if error.contains("session engine stopped"))
        );
    }
}
