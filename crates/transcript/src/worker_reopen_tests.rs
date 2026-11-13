use super::*;

fn header() -> AgentRunTranscriptHeader {
    AgentRunTranscriptHeader {
        version: AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        descriptor: AgentRunDescriptor {
            id: AgentRunId::new(),
            agent: "reviewer".into(),
            label: "Reviewer".into(),
            safe_mode: "read-only".into(),
        },
        prompt: "Review the change".into(),
    }
}

#[test]
fn reopening_projects_one_scan_and_retains_exhausted_append_validation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker.jsonl");
    let header = header();
    let outcome = AgentRunOutcome {
        descriptor: header.descriptor.clone(),
        status: AgentRunStatus::Completed,
        report: "report".into(),
        plan: None,
        partial: false,
        failure: None,
        usage: None,
        acp_session_id: None,
        user_decisions: Vec::new(),
        decision_ids: Vec::new(),
        unavailable_decisions: Vec::new(),
        confirmation: None,
    };
    let mut writer = AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap();
    for record in [
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::AgentMessage {
                text: "report".into(),
                message_id: Some("message".into()),
            },
        },
        AgentRunTranscriptRecord::Outcome {
            outcome: outcome.clone(),
        },
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Completed,
                detail: None,
            },
        },
    ] {
        writer.append(&record).unwrap();
    }
    drop(writer);
    let length = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    #[cfg(feature = "test-support")]
    let ((mut writer, projection), counts) =
        crate::replay_probe::measure_instruction_replay(|| {
            AgentRunTranscriptWriter::reopen(path.clone(), &header).unwrap()
        });
    #[cfg(not(feature = "test-support"))]
    let (mut writer, projection) = AgentRunTranscriptWriter::reopen(path.clone(), &header).unwrap();
    #[cfg(feature = "test-support")]
    {
        assert_eq!(counts.worker_loads, 1);
        assert_eq!(counts.worker_records, 4);
    }
    assert_eq!(projection.header.as_ref(), Some(&header));
    assert_eq!(projection.report, "report");
    assert_eq!(projection.recoverable_outcome(), Some(outcome));
    assert_eq!(
        writer.validation.outcome_status,
        Some(AgentRunStatus::Completed)
    );
    assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
    writer
        .append(&AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Completed,
                detail: None,
            },
        })
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        writer
            .append(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Status {
                    status: AgentRunStatus::Failed,
                    detail: None
                },
            })
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn mismatched_expected_header_never_repairs_a_recoverable_tail() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker.jsonl");
    let header = header();
    drop(AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap());
    let prefix = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"record\":\"event\",\"event\":")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut wrong = header.clone();
    wrong.prompt = "Different task".into();
    assert!(AgentRunTranscriptWriter::reopen(path.clone(), &wrong).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    AgentRunTranscriptWriter::reopen(path.clone(), &header).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), prefix);
}

#[test]
fn reopen_validates_journal_order_instead_of_trusting_display_projection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker.jsonl");
    let header = header();
    drop(AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap());
    let record = AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::Status {
            status: AgentRunStatus::Completed,
            detail: None,
        },
    };
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(AgentRunTranscriptWriter::reopen(path.clone(), &header).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
