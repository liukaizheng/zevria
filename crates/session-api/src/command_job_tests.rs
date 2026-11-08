use super::*;
fn limits() -> CommandJobLimits {
    CommandJobLimits {
        max_running_background: 8,
        retained_finished: 8,
        capture_bytes: 1024,
        foreground_timeout: Duration::from_secs(30),
        background_timeout: None,
    }
}
fn inspect(jobs: &CommandJobs) -> Vec<CommandJobView> {
    let CommandControlResult::Inspected(views) = jobs.handle_control(CommandControl {
        request_id: "inspect".into(),
        action: CommandControlAction::Inspect { id: None },
    }) else {
        panic!()
    };
    views
}
#[tokio::test]
async fn registration_cancellation_rolls_back_and_terminal_state_precedes_backpressured_events() {
    let (events, mut receiver) = crate::session_event_channel(1);
    let jobs = CommandJobs::new(limits(), Some(events));
    let first = jobs
        .register("first".into(), None, CommandJobMode::Background)
        .await
        .unwrap();
    let task = tokio::spawn({
        let jobs = jobs.clone();
        async move {
            jobs.register("never launched".into(), None, CommandJobMode::Foreground)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while inspect(&jobs).len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(inspect(&jobs).len(), 1);
    let id = first.snapshot().id;
    let task = tokio::spawn(async move {
        first
            .finish(CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            })
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !jobs.snapshot(&id).unwrap().status.is_terminal() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "only frontend publication should be blocked"
    );
    jobs.mark_reported(&id);
    assert!(jobs.take_unreported_notices(None).is_empty());
    receiver.recv().await.unwrap();
    task.await.unwrap();
    assert!(jobs.take_unreported_notices(None).is_empty());
}
#[tokio::test]
async fn unique_ids_survive_eviction_and_private_jobs_are_not_model_visible() {
    let jobs = CommandJobs::private(limits());
    let mut seen = HashSet::new();
    for _ in 0..512 {
        let job = jobs
            .register("private".into(), None, CommandJobMode::Foreground)
            .await
            .unwrap();
        assert!(seen.insert(job.snapshot().id));
        assert!(jobs.list_model_jobs().is_empty());
        job.finish(CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        })
        .await;
    }
    assert!(inspect(&jobs).is_empty());
}
#[tokio::test]
async fn backgrounding_and_turn_cancellation_are_serialized() {
    let jobs = CommandJobs::new(limits(), None);
    let moved = jobs
        .register("move first".into(), None, CommandJobMode::Foreground)
        .await
        .unwrap();
    jobs.move_to_background(&moved.snapshot().id).unwrap();
    assert!(!moved.terminate_foreground(TerminationOrigin::TurnCancelled));
    assert_eq!(*moved.termination_requested().borrow(), None);
    let stopped = jobs
        .register("cancel first".into(), None, CommandJobMode::Foreground)
        .await
        .unwrap();
    assert!(stopped.terminate_foreground(TerminationOrigin::TurnCancelled));
    assert!(jobs.move_to_background(&stopped.snapshot().id).is_err());
    stopped
        .finish(CommandJobStatus::Terminated {
            origin: TerminationOrigin::TurnCancelled,
        })
        .await;
    jobs.shutdown(Duration::ZERO).await;
}

#[test]
fn structured_streams_keep_gaps_and_unread_ranges_exact() {
    assert!(StreamText::default().is_empty());
    assert!(
        !StreamText {
            discarded: 1,
            ..StreamText::default()
        }
        .is_empty()
    );
    let mut buffer = StreamBuffer::new(8);
    buffer.append(b"HEAD123456TAIL");
    assert_eq!(
        buffer.take_unread(false),
        StreamText {
            head: "HEAD".into(),
            discarded: 6,
            tail: "TAIL".into()
        }
    );
    buffer.append(&[b'x'; 100]);
    assert_eq!(buffer.unread_len(), 100);
    assert_eq!(
        buffer.take_unread(false),
        StreamText {
            head: String::new(),
            discarded: 96,
            tail: "xxxx".into()
        }
    );
    assert!(buffer.take_unread(false).is_empty());
    assert_eq!(buffer.unread_len(), 0);
    assert_eq!(
        buffer.retained(),
        StreamText {
            head: "HEAD".into(),
            discarded: 106,
            tail: "xxxx".into()
        }
    );
}

#[test]
fn contiguous_retained_utf8_and_gapped_running_holdback() {
    let mut buffer = StreamBuffer::new(6);
    buffer.append("ab€".as_bytes());
    assert_eq!(
        buffer.retained(),
        StreamText {
            head: "ab€".into(),
            ..StreamText::default()
        }
    );
    assert_eq!(buffer.take_unread(true), buffer.retained());
    let mut buffer = StreamBuffer::new(8);
    buffer.append(b"HEAD123456X\xe2\x82");
    assert_eq!(
        buffer.take_unread(true),
        StreamText {
            head: "HEAD".into(),
            discarded: 5,
            tail: "6X".into()
        }
    );
    assert_eq!(buffer.unread_len(), 2);
    buffer.append(&[0xac]);
    assert_eq!(
        buffer.take_unread(true),
        StreamText {
            head: "€".into(),
            ..StreamText::default()
        }
    );
    assert_eq!(buffer.unread_len(), 0);
}

#[test]
fn incremental_utf8_and_lossy_frontend_tails_stay_bounded() {
    for limit in 32..64 {
        let mut buffer = StreamBuffer::new(limit);
        for byte in "head:你好:tail".bytes() {
            buffer.append(&[byte]);
            let text = buffer.take_unread(true);
            assert_eq!(text.discarded, 0);
            assert!(text.tail.is_empty());
            assert!(
                !text.head.contains('\u{fffd}') && !text.tail.contains('\u{fffd}'),
                "complete retained UTF-8 should never be replaced: {text:?}"
            );
        }
        buffer.take_unread(false);
        assert!(buffer.tail_preview(7).len() <= 7);
    }
    let mut buffer = StreamBuffer::new(32_768);
    buffer.append(&vec![0xff; 32_768]);
    assert!(buffer.tail_preview(8192).len() <= 8192);
}
