//! Read-only dependency restoration. Each load validates once on the blocking
//! pool; all jobs settle before errors are selected in original input order.
//! No writable repair or engine/supervisor spawn precedes this barrier.
use super::{RestoredAgentRun, RestoredSubtask};
use anyhow::Context as _;
use std::path::{Path, PathBuf};
use zevria_foundation::{SubtaskId, SubtaskLaunchMetadata};
use zevria_transcript::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunTranscriptHeader, AgentRunTranscriptReader,
    AgentRunTranscriptRecord, TranscriptItem, agent_run_path, subtask_launch_metadata, transcript,
};

enum Load {
    Child {
        id: String,
        path: PathBuf,
        launch: Option<SubtaskLaunchMetadata>,
    },
    Agent {
        path: PathBuf,
        expected: AgentRunTranscriptHeader,
    },
}
enum Loaded {
    Child(RestoredSubtask),
    Agent(RestoredAgentRun),
    Missing,
}
impl Load {
    fn read(self) -> anyhow::Result<Loaded> {
        match self {
            Self::Child { id, path, launch } => {
                let loaded = transcript::load_report(&path)?;
                Ok(Loaded::Child(RestoredSubtask {
                    id: SubtaskId::new(id),
                    launch,
                    items: loaded.into_items(),
                }))
            }
            Self::Agent { path, expected } => {
                match std::fs::metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Loaded::Missing);
                    }
                    Err(error) => return Err(error.into()),
                    Ok(_) => {}
                }
                let records =
                    AgentRunTranscriptReader::open(&path)?.collect::<anyhow::Result<Vec<_>>>()?;
                let header = records.first().and_then(|record| match record {
                    AgentRunTranscriptRecord::Header { header } => Some(header),
                    _ => None,
                });
                anyhow::ensure!(
                    header == Some(&expected),
                    transcript::UnsupportedHistory::new(
                        &path,
                        Some(1),
                        "worker identity matching the root ensemble start",
                    )
                );
                Ok(Loaded::Agent(RestoredAgentRun {
                    run_id: expected.ensemble_run_id,
                    descriptor: expected.descriptor,
                    path,
                    records,
                }))
            }
        }
    }
}

pub(super) async fn preflight(
    children: &Path,
    agents: &Path,
    items: &[TranscriptItem],
) -> anyhow::Result<(Vec<RestoredSubtask>, Vec<RestoredAgentRun>)> {
    // The caller owns the authoritative root load report. Revalidating it (or
    // children after load_report) would duplicate the same replay work.
    let children = children.to_path_buf();
    let paths = tokio::task::spawn_blocking(move || transcript::subsession_files(&children))
        .await
        .context("subsession discovery task failed")??;
    let launches = subtask_launch_metadata(items);
    let mut loads = paths
        .into_iter()
        .map(|(id, path)| {
            let launch = launches.get(&id).cloned();
            Load::Child { id, path, launch }
        })
        .collect::<Vec<_>>();
    for item in items {
        if let TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::Started { start }) = item {
            for descriptor in &start.agents {
                loads.push(Load::Agent {
                    path: agent_run_path(agents, &start.run_id, &descriptor.id),
                    expected: AgentRunTranscriptHeader {
                        version: AGENT_RUN_TRANSCRIPT_VERSION,
                        ensemble_run_id: start.run_id.clone(),
                        workflow: start.workflow,
                        descriptor: descriptor.clone(),
                        prompt: start.prompt.clone(),
                    },
                });
            }
        }
    }
    let loaded = join_loads(loads.into_iter().map(|load| move || load.read()).collect()).await?;
    let mut subtasks = Vec::new();
    let mut agent_runs = Vec::new();
    for loaded in loaded {
        match loaded {
            Loaded::Child(child) => subtasks.push(child),
            Loaded::Agent(agent) => agent_runs.push(agent),
            Loaded::Missing => {}
        }
    }
    Ok((subtasks, agent_runs))
}

async fn join_loads<T, F>(loads: Vec<F>) -> anyhow::Result<Vec<T>>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    // Spawn the entire batch before awaiting any handle. Awaiting in input
    // order does not serialize execution, and stabilizes error precedence.
    let jobs = loads
        .into_iter()
        .map(tokio::task::spawn_blocking)
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(jobs.len());
    for job in jobs {
        results.push(job.await);
    }
    // Only inspect errors after every read-only job has finished. Writable
    // repair and engine/service spawning happen after the caller gets success.
    results
        .into_iter()
        .map(|result| result.context("session restoration loader failed")?)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[tokio::test]
    async fn blocking_dependency_loads_overlap_but_results_keep_input_order() {
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let loads = (0..2)
            .map(|index| {
                let release = release.clone();
                let wait = wait.clone();
                move || {
                    if index == 0 {
                        wait.lock().unwrap().recv_timeout(Duration::from_secs(5))?;
                    } else {
                        release.send(())?;
                    }
                    Ok(index)
                }
            })
            .collect();
        assert_eq!(join_loads(loads).await.unwrap(), [0, 1]);
    }

    #[tokio::test]
    async fn all_dependencies_finish_before_the_first_ordered_error_is_returned() {
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let (started, mut blocked) = tokio::sync::mpsc::unbounded_channel();
        let completed = Arc::new(AtomicUsize::new(0));
        let loads = (0..3)
            .map(|index| {
                let wait = wait.clone();
                let started = started.clone();
                let completed = completed.clone();
                move || {
                    if index == 2 {
                        started.send(())?;
                        wait.lock().unwrap().recv_timeout(Duration::from_secs(5))?;
                    }
                    completed.fetch_add(1, Ordering::SeqCst);
                    match index {
                        0 => anyhow::bail!("first ordered failure"),
                        1 => anyhow::bail!("later failure"),
                        _ => Ok(index),
                    }
                }
            })
            .collect();
        let work = join_loads(loads);
        tokio::pin!(work);
        tokio::select! {
            _ = blocked.recv() => {},
            result = &mut work => panic!("returned before the blocked dependency finished: {result:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut work)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(work.await.unwrap_err().to_string(), "first ordered failure");
        assert_eq!(completed.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn failed_dependency_barrier_never_repairs_a_recoverable_child() {
        let directory = tempfile::tempdir().unwrap();
        let children = directory.path().join("children");
        std::fs::create_dir(&children).unwrap();
        let recoverable = children.join("a.jsonl");
        let mut bytes = serde_json::to_vec(&TranscriptItem::Message(
            rig_core::message::Message::user("saved"),
        ))
        .unwrap();
        bytes.extend_from_slice(b"\n{\"partial\":");
        std::fs::write(&recoverable, &bytes).unwrap();
        assert_eq!(
            transcript::load_report(&recoverable)
                .unwrap()
                .recoverable_lines,
            1
        );
        std::fs::write(children.join("b.jsonl"), b"{\"zevria_unknown\":{}}\n").unwrap();
        assert!(
            preflight(&children, &directory.path().join("agents"), &[])
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&recoverable).unwrap(), bytes);
    }

    #[tokio::test]
    async fn restoration_does_not_revalidate_the_authoritative_root_load() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = transcript::TranscriptWriter::create(directory.path()).unwrap();
        writer
            .append(&TranscriptItem::Message(rig_core::message::Message::user(
                "root",
            )))
            .unwrap();
        let loaded = transcript::load_report(writer.path()).unwrap();
        let (restored, counts) =
            zevria_transcript::replay_probe::measure_instruction_replay_async(preflight(
                &directory.path().join("children"),
                &directory.path().join("agents"),
                loaded.items(),
            ))
            .await;
        assert!(restored.unwrap().0.is_empty());
        assert_eq!(counts.loads, 0);
        assert_eq!(counts.full_replays, 0);
        assert_eq!(counts.validations, 0);
    }

    #[test]
    fn child_load_retains_only_the_load_reports_one_authoritative_validation() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = transcript::TranscriptWriter::create(directory.path()).unwrap();
        writer
            .append(&TranscriptItem::Message(rig_core::message::Message::user(
                "child",
            )))
            .unwrap();
        let (loaded, counts) = zevria_transcript::replay_probe::measure_instruction_replay(|| {
            Load::Child {
                id: "child".into(),
                path: writer.path().to_path_buf(),
                launch: None,
            }
            .read()
            .unwrap()
        });
        assert!(matches!(loaded, Loaded::Child(_)));
        assert_eq!(counts.loads, 1);
        assert_eq!(counts.full_replays, 1);
        assert_eq!(counts.validations, 1);
    }

    #[test]
    fn worker_dependency_validates_and_captures_records_in_one_reader_scan() {
        use zevria_workflow::{
            AgentRunDescriptor, AgentRunEvent, AgentRunId, EnsembleRunId, EnsembleWorkflow,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.jsonl");
        let expected = AgentRunTranscriptHeader {
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
        };
        let mut writer =
            zevria_transcript::AgentRunTranscriptWriter::create(path.clone(), expected.clone())
                .unwrap();
        writer
            .append(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::AgentMessage {
                    text: "report".into(),
                    message_id: None,
                },
            })
            .unwrap();
        let (loaded, counts) = zevria_transcript::replay_probe::measure_instruction_replay(|| {
            Load::Agent { path, expected }.read().unwrap()
        });
        let Loaded::Agent(agent) = loaded else {
            panic!("agent records")
        };
        assert_eq!(agent.records.len(), 2);
        assert_eq!(counts.worker_loads, 1);
        assert_eq!(counts.worker_records, 2);
    }
}
