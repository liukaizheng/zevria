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
        .register("first".into(), None, CommandJobMode::Background, None)
        .await
        .unwrap();
    let task = tokio::spawn({
        let jobs = jobs.clone();
        async move {
            jobs.register(
                "never launched".into(),
                None,
                CommandJobMode::Foreground,
                None,
            )
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
    assert!(jobs.pending_completions().is_empty());
    receiver.recv().await.unwrap();
    task.await.unwrap();
    assert!(jobs.pending_completions().is_empty());
}
#[tokio::test]
async fn unique_ids_survive_eviction_and_private_jobs_are_not_model_visible() {
    let jobs = CommandJobs::private(limits());
    let mut seen = HashSet::new();
    for _ in 0..512 {
        let job = jobs
            .register("private".into(), None, CommandJobMode::Foreground, None)
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
        .register("move first".into(), None, CommandJobMode::Foreground, None)
        .await
        .unwrap();
    jobs.move_to_background(&moved.snapshot().id).unwrap();
    assert!(!moved.terminate_foreground(TerminationOrigin::TurnCancelled));
    assert_eq!(*moved.termination_requested().borrow(), None);
    let stopped = jobs
        .register(
            "cancel first".into(),
            None,
            CommandJobMode::Foreground,
            None,
        )
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

// The pre-optimization algorithms serve as a byte-for-byte reference for bulk
// retention, cursor advancement, and preview trimming (including lossy UTF-8).
#[derive(Clone)]
struct ByteReference(StreamBuffer);
impl ByteReference {
    fn append(&mut self, bytes: &[u8]) {
        let b = &mut self.0;
        for &byte in bytes {
            b.total += 1;
            if b.head.len() < b.limit / 2 {
                b.head.push(byte);
            } else if b.limit - b.limit / 2 > 0 {
                if b.tail.len() == b.limit - b.limit / 2 {
                    b.tail.pop_front();
                }
                b.tail.push_back(byte);
            }
        }
    }
    fn take_unread(&mut self, running: bool) -> StreamText {
        let b = &mut self.0;
        let tail_start = b.total - b.tail.len();
        let mut bytes = Vec::new();
        if b.cursor < b.head.len() {
            bytes.extend_from_slice(&b.head[b.cursor..]);
        }
        let gap_start = b.cursor.max(b.head.len());
        if tail_start > gap_start {
            let head = String::from_utf8_lossy(&bytes).into_owned();
            bytes = b.tail.iter().copied().collect();
            let end = utf8_end(&bytes, running);
            b.cursor = tail_start + end;
            return StreamText {
                head,
                discarded: tail_start - gap_start,
                tail: String::from_utf8_lossy(&bytes[..end]).into_owned(),
            };
        }
        bytes.extend(
            b.tail
                .iter()
                .skip(b.cursor.saturating_sub(tail_start))
                .copied(),
        );
        let end = utf8_end(&bytes, running);
        b.cursor += end;
        StreamText {
            head: String::from_utf8_lossy(&bytes[..end]).into_owned(),
            ..StreamText::default()
        }
    }
    fn tail_preview(&self, max: usize) -> String {
        let b = &self.0;
        let mut bytes: Vec<u8> = if b.total > b.head.len() + b.tail.len() {
            b.tail.iter().copied().collect()
        } else {
            b.head.iter().chain(b.tail.iter()).copied().collect()
        };
        if bytes.len() > max {
            bytes.drain(..bytes.len() - max);
        }
        let mut start = 0;
        while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
            start += 1;
        }
        let text = String::from_utf8_lossy(&bytes[start..]);
        let mut start = text.len().saturating_sub(max);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].to_owned()
    }
}

#[test]
fn stream_buffer_bulk_paths_match_byte_reference() {
    let chunks: &[&[u8]] = &[
        b"",
        b"a",
        b"bc",
        b"\xe2",
        b"\x82",
        b"\xac",
        "你好🐚".as_bytes(),
        b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        b"\xff\x80z\xe2\x82",
        b"\xac!",
        b"tail",
        b"",
        b"\xf0\x9f",
        b"\x90\x9a",
    ];
    for limit in [0, 1, 2, 7, 8, 64] {
        for consume_running in [false, true] {
            for consume_every in [1, 3, usize::MAX] {
                let mut buffer = StreamBuffer::new(limit);
                let mut reference = ByteReference(StreamBuffer::new(limit));
                for (index, chunk) in chunks.iter().cycle().take(chunks.len() * 3).enumerate() {
                    buffer.append(chunk);
                    reference.append(chunk);
                    assert_eq!(buffer.retained(), reference.0.retained());
                    assert_eq!(buffer.total_bytes(), reference.0.total_bytes());
                    assert_eq!(buffer.unread_len(), reference.0.unread_len());
                    for running in [false, true] {
                        let expected = reference.clone().take_unread(running);
                        assert_eq!(buffer.peek_unread(running), expected);
                        let mut reader = buffer.clone();
                        assert_eq!(reader.take_unread(running), expected);
                        let mut reader_reference = reference.clone();
                        reader_reference.take_unread(running);
                        assert_eq!(reader.unread_len(), reader_reference.0.unread_len());
                    }
                    for max in [0, 1, 2, 3, 7, 8, 32, 64, 8192] {
                        assert_eq!(buffer.tail_preview(max), reference.tail_preview(max));
                    }
                    if index % consume_every == 0 {
                        assert_eq!(
                            buffer.take_unread(consume_running),
                            reference.take_unread(consume_running)
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn listing_keeps_unread_completions_pending_and_pinned() {
    let jobs = CommandJobs::new(limits(), None);
    let unread = jobs
        .register("unread".into(), None, CommandJobMode::Background, None)
        .await
        .unwrap();
    let empty = jobs
        .register("empty".into(), None, CommandJobMode::Background, None)
        .await
        .unwrap();
    jobs.mark_listed(&CommandJobId::new("unknown"));
    jobs.mark_listed(&empty.snapshot().id);
    assert_eq!(
        jobs.snapshot(&empty.snapshot().id).unwrap().status,
        CommandJobStatus::Running
    );
    unread.append(CommandStream::Stdout, b"still unread");
    assert!(!unread.output_detached());
    unread.mark_output_detached();
    for job in [&unread, &empty] {
        job.finish(CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        })
        .await;
        jobs.mark_listed(&job.snapshot().id);
    }
    let pending = jobs.pending_completions();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].snapshot.id, unread.snapshot().id);
    assert!(pending[0].output_detached());
    jobs.acknowledge_completions(&[(unread.snapshot().id, false)]);
    jobs.mark_listed(&unread.snapshot().id);
    for _ in 0..=jobs.limits().retained_finished {
        let job = jobs
            .register("later".into(), None, CommandJobMode::Background, None)
            .await
            .unwrap();
        job.finish(CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        })
        .await;
        jobs.mark_listed(&job.snapshot().id);
    }
    assert!(jobs.pending_completions().is_empty());
    assert!(jobs.snapshot(&unread.snapshot().id).is_ok());
    assert_eq!(
        jobs.take_unread(&unread.snapshot().id).unwrap().0.head,
        "still unread"
    );
    jobs.mark_listed(&unread.snapshot().id);
    // Releasing the pin makes this oldest job evictable at the next overflow.
    let next = jobs
        .register("overflow".into(), None, CommandJobMode::Background, None)
        .await
        .unwrap();
    next.finish(CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    })
    .await;
    assert!(jobs.snapshot(&unread.snapshot().id).is_err());
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
