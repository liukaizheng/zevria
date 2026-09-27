/// Opt-in inspection of real histories, never provider work or writable open.
/// Set ZEVRIA_REPLAY_WORKSPACE and ZEVRIA_REPLAY_SESSION, then run this ignored
/// test with --ignored --nocapture. No private fixture or payload is committed.
#[test]
#[ignore = "requires an explicitly supplied local root history; read-only"]
fn existing_root_history_passes_read_only_replay_and_preflight() -> anyhow::Result<()> {
    use zevria_transcript::transcript::{self, TranscriptItem};
    use zevria_workflow::EnsembleRecord;

    let workspace = PathBuf::from(std::env::var("ZEVRIA_REPLAY_WORKSPACE")?);
    let session = std::env::var("ZEVRIA_REPLAY_SESSION")?;
    let path = transcript::sessions_dir(&workspace).join(format!("{session}.jsonl"));
    let logs = zevria_transcript::agent_runs_dir(&workspace, &session);
    let mut originals = vec![(path.clone(), std::fs::read(&path)?)];
    let result = (|| -> anyhow::Result<()> {
        let items = transcript::load(&path)?;
        zevria_transcript::SessionReplayError::validate(&items)?;
        zevria_transcript::project_worker_reviews(&items).map_err(anyhow::Error::msg)?;
        let starts = items
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Ensemble(EnsembleRecord::Started { start }) => Some(start),
                _ => None,
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !starts.is_empty(),
            "root history references no ensemble runs"
        );

        // Deliberately do not call new(): even its create-only ignore guard is
        // a write. recover_review uses only agent_runs_root, readers and reducers.
        let supervisor = EnsembleSupervisor {
            config: EnsembleConfig::default(),
            workspace: workspace.clone(),
            #[cfg(windows)]
            acp_helper: std::env::current_exe()?,
            agent_runs_root: logs.clone(),
            questions: test_questions(),
            question_gate: Arc::new(tokio::sync::Mutex::new(())),
        };
        let mut workers = 0;
        let mut records = 0;
        let mut terminal_displays = 0;
        for start in starts {
            for descriptor in &start.agents {
                let worker = agent_run_path(&logs, &start.run_id, &descriptor.id);
                if !worker.try_exists()? {
                    continue;
                }
                originals.push((worker.clone(), std::fs::read(&worker)?));
                let mut streamed = AgentRunProjection::default();
                let mut worker_records = 0;
                let mut worker_displays = 0;
                for record in zevria_transcript::AgentRunTranscriptReader::open(&worker)? {
                    let record = record?;
                    if matches!(&record, AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::ResponseDisplay { display },
                    } if !display.attempt.terminal.is_empty())
                    {
                        worker_displays += 1;
                    }
                    streamed.apply(&record);
                    worker_records += 1;
                }
                let projection = load_agent_run_projection(&worker)?;
                anyhow::ensure!(
                    projection == streamed,
                    "streaming/projection mismatch at {}",
                    worker.display()
                );
                eprintln!(
                    "read-only worker {}: {worker_records} records, {worker_displays} terminal-evidence displays",
                    worker.display()
                );
                workers += 1;
                records += worker_records;
                terminal_displays += worker_displays;
            }
            preflight_existing_logs(&logs, start)?;
            let sealed = items.iter().any(|item| matches!(item,
                TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { run_id, .. }) if run_id == &start.run_id
            ));
            if start.workflow == EnsembleWorkflow::Plan && !sealed {
                let history = items
                    .iter()
                    .filter_map(|item| match item {
                        TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                            run_id,
                            worker_id,
                            event,
                            ..
                        }) if run_id == &start.run_id => Some((worker_id.clone(), *event.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let recovered = supervisor.recover_review(start, &history)?;
                eprintln!(
                    "read-only reconciliation: {} recoverable updates (not committed)",
                    recovered.len()
                );
            }
        }
        eprintln!(
            "read-only root: {} items; {workers} workers, {records} worker records, {terminal_displays} terminal-evidence displays; replay and preflight passed",
            items.len()
        );
        Ok(())
    })();
    // Check on failure too. Do not print byte arrays if an external writer ran.
    for (path, bytes) in originals {
        anyhow::ensure!(
            std::fs::read(&path)? == bytes,
            "history bytes changed during inspection: {}",
            path.display()
        );
    }
    result
}
