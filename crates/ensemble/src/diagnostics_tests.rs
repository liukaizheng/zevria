use super::*;
use crate::supervisor::diagnostics::image_safe_protocol_diagnostic;

async fn diagnostic_log() -> (tempfile::TempDir, RunLog) {
    let directory = tempfile::tempdir().unwrap();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "diagnostics".into(),
        label: "Diagnostics fixture".into(),
        safe_mode: "read-only".into(),
    };
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "diagnostics fixture".into(),
        agents: vec![descriptor.clone()],
    };
    let (events, _receiver) = session_event_channel(8);
    let (log, _) = RunLog::open(
        directory.path().join("diagnostics.jsonl"),
        &start,
        &descriptor,
        false,
        events,
        TurnContext::new(
            TurnId::new(42),
            SessionMode::Build,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    (directory, log)
}

async fn finish_diagnostics(
    sink: RawDiagnosticSink,
    log: &RunLog,
    barrier: bool,
) -> Vec<AgentRunTranscriptRecord> {
    tokio::time::timeout(Duration::from_secs(5), async {
        if barrier {
            sink.barrier().await;
        }
        sink.finish().await;
        log.barrier().await.unwrap();
    })
    .await
    .expect("truncation must not hang a diagnostic barrier or finish");
    assert!(
        log.failure().is_none(),
        "truncation is not a worker failure"
    );
    let records = load_agent_run(log.path()).unwrap();
    assert_eq!(records.iter().filter(|record| matches!(record,
        AgentRunTranscriptRecord::Event { event: AgentRunEvent::Unsupported { context, placeholder } }
        if context == "ACP raw diagnostics" && placeholder.contains("truncated")
    )).count(), 1, "exactly one durable truncation marker");
    records
}

#[tokio::test(flavor = "current_thread")]
async fn raw_diagnostic_line_limit_preserves_the_boundary_and_marks_drops_on_barrier_or_finish() {
    for barrier in [false, true] {
        let (_directory, log) = diagnostic_log().await;
        let sink = RawDiagnosticSink::spawn(&log);
        let boundary = "x ".repeat(RAW_DIAGNOSTIC_MAX_LINE_BYTES / 2);
        assert_eq!(boundary.len(), RAW_DIAGNOSTIC_MAX_LINE_BYTES);
        sink.sender().record(&boundary, LineDirection::Stderr);
        sink.barrier().await;
        log.barrier().await.unwrap();
        assert!(load_agent_run(log.path()).unwrap().iter().any(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Stderr { text } } if text == &boundary
        )));
        // A rejected line queues no message at all. Both shutdown and a barrier
        // must still observe the drop counter and persist its marker.
        sink.sender()
            .record(&format!("{boundary}x"), LineDirection::Stderr);
        assert_eq!(sink.sender().dropped_lines.load(Ordering::Acquire), 1);
        let records = finish_diagnostics(sink, &log, barrier).await;
        assert!(records.iter().all(|record| !matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Stderr { text } }
            if text.len() > RAW_DIAGNOSTIC_MAX_LINE_BYTES
        )));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_diagnostic_queue_overflow_marks_drops_on_barrier_or_finish() {
    for barrier in [false, true] {
        let (_directory, log) = diagnostic_log().await;
        let sink = RawDiagnosticSink::spawn(&log);
        // No await on a current-thread runtime: the consumer cannot run until
        // the queue has filled and the extra line has deterministically dropped.
        for _ in 0..DEBUG_LOG_QUEUE_CAPACITY {
            sink.sender()
                .record("queued diagnostic", LineDirection::Stderr);
        }
        assert_eq!(sink.sender().dropped_lines.load(Ordering::Acquire), 0);
        sink.sender()
            .record("overflow diagnostic", LineDirection::Stderr);
        assert_eq!(sink.sender().dropped_lines.load(Ordering::Acquire), 1);
        let records = finish_diagnostics(sink, &log, barrier).await;
        assert!(records.iter().all(|record| !matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Stderr { text } }
            if text == "overflow diagnostic"
        )));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_diagnostic_run_limit_counts_encoded_redacted_bytes_without_queue_drops() {
    for barrier in [false, true] {
        let (_directory, log) = diagnostic_log().await;
        let sink = RawDiagnosticSink::spawn(&log);
        let empty_bytes = serde_json::to_vec(&diagnostic_record("")).unwrap().len();
        let opaque = "x".repeat(RAW_DIAGNOSTIC_MAX_LINE_BYTES);
        let redacted = image_safe_protocol_diagnostic(&opaque);
        assert!(redacted.len() < opaque.len());
        sink.sender().record(&opaque, LineDirection::Stderr);
        sink.barrier().await;
        let mut retained = serde_json::to_vec(&diagnostic_record(&redacted))
            .unwrap()
            .len();
        while retained < RAW_DIAGNOSTIC_MAX_RUN_BYTES {
            let bytes_left = RAW_DIAGNOSTIC_MAX_RUN_BYTES - retained;
            assert!(bytes_left >= empty_bytes);
            let payload_bytes = (bytes_left - empty_bytes).min(RAW_DIAGNOSTIC_MAX_LINE_BYTES);
            // Spaces prevent the image-safe diagnostic copy from redacting the
            // payload as one opaque base64 token (the old integration fixture).
            let mut line = "x ".repeat(payload_bytes.div_ceil(2));
            line.truncate(payload_bytes);
            assert_eq!(image_safe_protocol_diagnostic(&line), line);
            let encoded_bytes = serde_json::to_vec(&diagnostic_record(&line)).unwrap().len();
            sink.sender().record(&line, LineDirection::Stderr);
            sink.barrier().await;
            retained += encoded_bytes;
        }
        assert_eq!(retained, RAW_DIAGNOSTIC_MAX_RUN_BYTES);
        assert_eq!(sink.sender().dropped_lines.load(Ordering::Acquire), 0);
        // Even an empty line has an encoded transcript envelope, so this crosses
        // the exact budget without crossing the per-line or queue limits.
        sink.sender().record("", LineDirection::Stderr);
        let records = finish_diagnostics(sink, &log, barrier).await;
        let retained_bytes = records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::Stderr { .. }
                    }
                )
            })
            .map(|record| serde_json::to_vec(record).unwrap().len())
            .sum::<usize>();
        assert_eq!(retained_bytes, RAW_DIAGNOSTIC_MAX_RUN_BYTES);
        assert!(records.iter().any(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Unsupported { placeholder, .. } }
            if placeholder.contains("per-run limit")
        )));
    }
}
