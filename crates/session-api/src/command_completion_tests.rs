use super::*;

fn jobs() -> CommandJobs {
    CommandJobs::new(
        CommandJobLimits {
            max_running_background: 8,
            retained_finished: 8,
            capture_bytes: 1024,
            foreground_timeout: Duration::from_secs(30),
            background_timeout: None,
        },
        None,
    )
}
fn exited() -> CommandJobStatus {
    CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    }
}

#[tokio::test]
async fn pending_completions_pin_finish_order_origin_and_unread_output_until_ack() {
    let jobs = jobs();
    let mut changes = jobs.completions_changed();
    let mut ids = Vec::new();
    for i in 1..=12 {
        let job = jobs
            .register(
                format!("job {i}"),
                None,
                CommandJobMode::Background,
                Some(TurnId::new(i)),
            )
            .await
            .unwrap();
        job.append(CommandStream::Stdout, b"unread");
        job.append(CommandStream::Stderr, b"error");
        job.finish(exited()).await;
        job.finish(exited()).await;
        ids.push(job.snapshot().id);
    }
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), 12);
    let pending = jobs.pending_completions();
    assert_eq!(pending.len(), 12);
    for (i, completion) in pending.iter().enumerate() {
        assert_eq!(completion.snapshot.id, ids[i]);
        assert_eq!(completion.origin, Some(TurnId::new(i as u64 + 1)));
        assert_eq!(completion.unread_bytes(), (6, 5));
        assert_eq!(completion.peek_unread(), completion.peek_unread());
        assert_eq!(completion.unread_bytes(), (6, 5));
        assert!(jobs.snapshot(&ids[i]).is_ok());
    }
    jobs.acknowledge_completions(&[(ids[0].clone(), true)]);
    assert_eq!(pending[0].unread_bytes(), (0, 0));
    assert_eq!(jobs.pending_completions().len(), 11);
    // Skipped output stays readable even when the finished-job retention is full.
    jobs.acknowledge_completions(
        &pending[1..]
            .iter()
            .map(|c| (c.snapshot.id.clone(), false))
            .collect::<Vec<_>>(),
    );
    assert!(jobs.pending_completions().is_empty());
    for id in &ids[1..] {
        assert_eq!(jobs.take_unread(id).unwrap().0.head, "unread");
    }
    for id in &ids[1..] {
        jobs.mark_reported(id);
    }
    assert_eq!(jobs.list_model_jobs().len(), 8);
}

#[tokio::test]
async fn explicit_report_and_launch_failure_do_not_leave_completion_work() {
    let jobs = jobs();
    let job = jobs
        .register("done".into(), None, CommandJobMode::Background, None)
        .await
        .unwrap();
    job.finish(exited()).await;
    jobs.mark_reported(&job.snapshot().id);
    assert!(jobs.pending_completions().is_empty());
    let mut changes = jobs.completions_changed();
    changes.borrow_and_update();
    let failed = jobs
        .register(
            "not launched".into(),
            None,
            CommandJobMode::Background,
            None,
        )
        .await
        .unwrap();
    failed.launch_failed("spawn failed".into()).await;
    assert!(jobs.pending_completions().is_empty());
    assert!(!changes.has_changed().unwrap());
    assert!(jobs.snapshot(&failed.snapshot().id).is_err());
    jobs.close();
    let job = jobs.job(&job.snapshot().id).unwrap();
    job.finish(exited()).await;
    assert!(jobs.pending_completions().is_empty());
}

#[test]
fn peek_and_consume_preserve_utf8_and_gap_semantics() {
    let mut buffer = StreamBuffer::new(8);
    buffer.append(b"HEAD123456TAIL");
    assert_eq!(buffer.peek_unread(false), buffer.retained());
    assert_eq!(buffer.unread_len(), 14);
    buffer.consume_all();
    assert!(buffer.peek_unread(false).is_empty());
    buffer.append(&[0xe2, 0x82]);
    assert!(buffer.peek_unread(true).is_empty());
    buffer.append(&[0xac]);
    assert_eq!(buffer.peek_unread(true).head, "€");
}
