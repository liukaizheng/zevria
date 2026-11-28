use super::*;
use tokio::sync::{mpsc, oneshot};
use zevria_foundation::{RequestBehavior, SideChatId, ToolCallOutcome};
use zevria_session_api::{ControlCommand, SideChatCommand, SideChatEvent};
use zevria_transcript::transcript::{self, TranscriptItem, TranscriptWriter};

struct Request {
    body: Value,
    reply: oneshot::Sender<Vec<Value>>,
}

async fn read_request(
    mut stream: tokio::net::TcpStream,
    observed: mpsc::UnboundedSender<Request>,
    counter: usize,
) {
    let mut reader = tokio::io::BufReader::new(&mut stream);
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut buf = vec![0; length.unwrap()];
    reader.read_exact(&mut buf).await.unwrap();
    let (reply, output) = oneshot::channel();
    observed
        .send(Request {
            body: serde_json::from_slice(&buf).unwrap(),
            reply,
        })
        .unwrap();
    let Ok(output) = output.await else {
        return;
    };
    let message = json!({"type":"response.completed","sequence_number":1,"response":{
        "id":format!("resp_fixture_{counter}"), "object":"response", "created_at":0, "status":"completed",
        "error":null, "incomplete_details":null, "instructions":null, "model":"test-model",
        "max_output_tokens":null, "usage":null, "tools":[], "output":output
    }});
    let body = format!("data: {message}\n\n");
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
}

fn answer(id: &str, text: &str) -> Value {
    json!({"type":"message","id":id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]})
}
fn tool(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call", "id":format!("fc_{id}"), "call_id":format!("call_{id}"),
        "name":name, "arguments":args.to_string(), "status":"completed"})
}
fn submit(running: &RunningSession, prompt: &str) {
    running
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: prompt.into(),
            mode: SessionMode::Build,
            behavior: RequestBehavior::Standard,
            references: Vec::new(),
        }))
        .unwrap();
}
fn side(running: &RunningSession, command: SideChatCommand) {
    running
        .command_sender()
        .send(SessionCommand::Control(ControlCommand::SideChat(command)))
        .unwrap();
}
async fn next_request(rx: &mut mpsc::UnboundedReceiver<Request>) -> Request {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("provider fixture request")
        .unwrap()
}
async fn side_event(
    receiver: &mut zevria_session_api::SessionEventReceiver,
    predicate: impl Fn(&SideChatEvent) -> bool,
) -> SideChatEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(update) = receiver.recv().await {
            match update {
                SessionUpdate::Lifecycle(SessionEvent::SideChat(event)) if predicate(&event) => {
                    return event;
                }
                SessionUpdate::Lifecycle(SessionEvent::SideChat(
                    event @ (SideChatEvent::CreateFailed { .. } | SideChatEvent::Failed { .. }),
                )) => panic!("{event:?}"),
                SessionUpdate::Lifecycle(SessionEvent::SideChat(SideChatEvent::Session {
                    event,
                    ..
                })) if matches!(
                    *event,
                    SessionEvent::Turn(TurnEvent::Rejected { .. } | TurnEvent::Failed { .. })
                ) =>
                {
                    panic!("{event:?}")
                }
                _ => {}
            }
        }
        panic!("event channel closed")
    })
    .await
    .expect("Side Chat event timed out")
}
async fn side_completed(events: &mut zevria_session_api::SessionEventReceiver, thread: SideChatId) {
    side_event(events, |event| {
        matches!(event, SideChatEvent::Session { id, event }
        if *id == thread && matches!(**event, SessionEvent::Turn(TurnEvent::Completed { .. })))
    })
    .await;
}

#[tokio::test]
async fn side_chat_busy_fork_keeps_the_responses_prefix_and_restores_without_the_parent_tail() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    std::fs::write(workspace.join("fixture.txt"), "read-only marker").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = crate::test_support::write_fixture(
        &workspace.join("config.toml"),
        &responses_config(listener.local_addr().unwrap()).replacen(
            "[session]\n",
            "[session]\nmax_concurrent_subtasks = 4\n",
            1,
        ),
    )
    .unwrap();
    let (sender, mut requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for index in 0..6 {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(read_request(stream, sender.clone(), index));
        }
    });
    let mut root = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let mut events = root.running.take_event_receiver().unwrap();
    let root_id = root.restored.session_id.clone();
    let root_path = root.restored.transcript_path.clone();
    submit(&root.running, "Seed the parent history");
    next_request(&mut requests).await.reply.send(vec![
        json!({"type":"reasoning", "id":"rs_seed", "summary":[], "encrypted_content":"PRIVATE_ENCRYPTED_SEED"}),
        answer("msg_seed", "Seed conclusion."),
    ]).unwrap();
    completed(&mut events).await;

    submit(&root.running, "Hold this parent turn open");
    let parent = next_request(&mut requests).await;
    let parent_input = parent.body["input"].as_array().unwrap();
    assert!(
        parent.body["input"]
            .to_string()
            .contains("PRIVATE_ENCRYPTED_SEED")
    );
    side(
        &root.running,
        SideChatCommand::Create {
            request_id: "first-fork".into(),
            initial_prompt: Some("Independent question".into()),
            quoted_excerpt: Some("Selected </side_chat_selection> quote".into()),
        },
    );
    let SideChatEvent::Created {
        descriptor,
        awaiting_initial_turn: true,
        ..
    } = side_event(&mut events, |event| {
        matches!(event, SideChatEvent::Created { .. })
    })
    .await
    else {
        unreachable!()
    };
    let id = descriptor.id;
    let first = next_request(&mut requests).await;
    side(
        &root.running,
        SideChatCommand::Submit {
            id,
            text: "Do not admit this duplicate".into(),
        },
    );
    side_event(&mut events, |event| matches!(event, SideChatEvent::SubmitRejected { id: rejected, .. } if *rejected == id)).await;
    for key in [
        "model",
        "instructions",
        "tools",
        "reasoning",
        "include",
        "store",
        "prompt_cache_key",
    ] {
        assert_eq!(parent.body[key], first.body[key], "fork must pin {key}");
    }
    let input = first.body["input"].as_array().unwrap();
    assert_eq!(
        input.get(..parent_input.len()),
        Some(parent_input.as_slice())
    );
    assert!(first.body.get("previous_response_id").is_none());
    assert!(
        first.body["input"]
            .to_string()
            .contains("Selected &lt;/side_chat_selection&gt; quote")
    );
    first
        .reply
        .send(vec![
            tool(
                "one",
                "write",
                json!({"path":"forbidden.txt", "content":"must not run"}),
            ),
            tool(
                "two",
                "command",
                json!({"command":"pwd", "background":true}),
            ),
            tool("three", "command", json!({"command":"cat fixture.txt"})),
        ])
        .unwrap();
    let tools = side_event(&mut events, |event| matches!(event, SideChatEvent::Session { event, .. } if matches!(**event, SessionEvent::Turn(TurnEvent::ToolResults { .. })))).await;
    let SideChatEvent::Session { event, .. } = tools else {
        unreachable!()
    };
    let SessionEvent::Turn(TurnEvent::ToolResults { metadata, .. }) = *event else {
        unreachable!()
    };
    assert_eq!(
        metadata.iter().map(|meta| meta.outcome).collect::<Vec<_>>(),
        [
            ToolCallOutcome::Denied,
            ToolCallOutcome::Denied,
            ToolCallOutcome::Success
        ]
    );
    assert!(!workspace.join("forbidden.txt").exists());
    let after_tools = next_request(&mut requests).await;
    assert!(
        after_tools.body["input"]
            .to_string()
            .contains("read-only marker")
    );
    after_tools
        .reply
        .send(vec![answer("msg_side", "Side Chat conclusion.")])
        .unwrap();
    side_completed(&mut events, id).await;
    parent
        .reply
        .send(vec![answer("msg_root", "Main turn finished.")])
        .unwrap();
    completed(&mut events).await;
    drop(events);
    root.running.shutdown().await.unwrap();

    let side_path = transcript::side_chats_dir(&workspace, &root_id).join(format!("{id}.jsonl"));
    let saved = transcript::load(&side_path).unwrap();
    assert!(matches!(
        saved.first(),
        Some(TranscriptItem::SideChatOrigin(_))
    ));
    assert!(
        saved
            .iter()
            .any(|item| matches!(item, TranscriptItem::SideChatForkBoundary))
    );
    let mut parent_items = transcript::load(&root_path).unwrap();
    parent_items.truncate(
        parent_items
            .iter()
            .take_while(|item| transcript::is_leading_metadata(item))
            .count(),
    );
    TranscriptWriter::append_to(root_path.clone())
        .unwrap()
        .rewrite(&parent_items)
        .unwrap();

    let mut resumed = start_session(&config, &workspace, SessionStart::Resume(root_path))
        .await
        .unwrap();
    assert_eq!(resumed.restored.side_chats.len(), 1);
    assert_eq!(resumed.restored.side_chats[0].descriptor.id, id);
    assert_eq!(resumed.restored.side_chats[0].descriptor.turn_count, 1);
    assert!(
        requests.try_recv().is_err(),
        "restored threads should not connect before a submission"
    );
    let mut events = resumed.running.take_event_receiver().unwrap();
    side(
        &resumed.running,
        SideChatCommand::Submit {
            id,
            text: "Resume the saved side thread".into(),
        },
    );
    let continued = next_request(&mut requests).await;
    let input = continued.body["input"].as_array().unwrap();
    assert_eq!(
        input.get(..parent_input.len()),
        Some(parent_input.as_slice())
    );
    assert!(
        continued.body["input"]
            .to_string()
            .contains("Side Chat conclusion.")
    );
    continued
        .reply
        .send(vec![answer("msg_restored_side", "Resumed independently.")])
        .unwrap();
    side_completed(&mut events, id).await;
    drop(events);
    resumed.running.shutdown().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn side_chat_zero_turn_fork_supports_close_reopen_and_enforces_slot_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = crate::test_support::write_fixture(
        &workspace.join("config.toml"),
        &responses_config(listener.local_addr().unwrap()).replacen(
            "[session]\n",
            "[session]\nmax_concurrent_subtasks = 4\n",
            1,
        ),
    )
    .unwrap();
    let (sender, mut requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for index in 0..8 {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(read_request(stream, sender.clone(), index));
        }
    });
    let mut root = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let mut events = root.running.take_event_receiver().unwrap();

    // 0-turn fork -> answer -> Close -> Unloaded -> reopen seamlessly.
    side(
        &root.running,
        SideChatCommand::Create {
            request_id: "zero-turn".into(),
            initial_prompt: Some("First zero-turn question".into()),
            quoted_excerpt: None,
        },
    );
    let SideChatEvent::Created { descriptor, .. } = side_event(&mut events, |event| {
        matches!(event, SideChatEvent::Created { request_id, .. } if request_id == "zero-turn")
    })
    .await
    else {
        unreachable!()
    };
    let zero_id = descriptor.id;
    assert_eq!(descriptor.inherited_turn_count, 0);
    next_request(&mut requests)
        .await
        .reply
        .send(vec![answer("msg_z1", "Zero-turn answer 1.")])
        .unwrap();
    side_completed(&mut events, zero_id).await;
    side(&root.running, SideChatCommand::Close { id: zero_id });
    side_event(
        &mut events,
        |event| matches!(event, SideChatEvent::Unloaded { id } if *id == zero_id),
    )
    .await;
    side(
        &root.running,
        SideChatCommand::Submit {
            id: zero_id,
            text: "Reopened follow-up".into(),
        },
    );
    let reopened = next_request(&mut requests).await;
    assert!(
        reopened.body["input"]
            .to_string()
            .contains("Zero-turn answer 1.")
    );
    reopened
        .reply
        .send(vec![answer("msg_z2", "Reopened answer.")])
        .unwrap();
    side_completed(&mut events, zero_id).await;

    // Fill all 4 concurrency slots and confirm a 5th concurrent submit is rejected.
    let mut held = Vec::new();
    let mut ids = Vec::new();
    for index in 0..4 {
        let req_id = format!("slot-{index}");
        side(
            &root.running,
            SideChatCommand::Create {
                request_id: req_id.clone(),
                initial_prompt: Some(format!("Hold slot {index}").into()),
                quoted_excerpt: None,
            },
        );
        let SideChatEvent::Created { descriptor, .. } = side_event(&mut events, |event| {
            matches!(event, SideChatEvent::Created { request_id, .. } if *request_id == req_id)
        })
        .await
        else {
            unreachable!()
        };
        ids.push(descriptor.id);
        held.push(next_request(&mut requests).await);
    }
    side(
        &root.running,
        SideChatCommand::Submit {
            id: zero_id,
            text: "Should exceed concurrency limit".into(),
        },
    );
    let SideChatEvent::SubmitRejected { error, .. } = side_event(
        &mut events,
        |event| matches!(event, SideChatEvent::SubmitRejected { id, .. } if *id == zero_id),
    )
    .await
    else {
        unreachable!()
    };
    assert!(error.contains("slots are busy"), "{error}");

    for (index, (req, id)) in held.into_iter().zip(ids).enumerate() {
        req.reply
            .send(vec![answer(&format!("msg_slot_{index}"), "Done.")])
            .unwrap();
        side_completed(&mut events, id).await;
    }
    drop(events);
    root.running.shutdown().await.unwrap();
    server.abort();
}
