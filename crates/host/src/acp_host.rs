//! Production adapter from `zevria-acp` runtime contracts to the extracted
//! Zevria engine runtime.

use std::{path::PathBuf, pin::Pin, sync::Arc};

use anyhow::Context as _;
use zevria_acp::{
    ExecutionProfile, RuntimeExit, SessionDescriptor, SessionRuntimeFactory,
    SessionRuntimeLifecycle, SessionStart, StartSessionRequest, StartedSession,
};
use zevria_transcript::transcript;

use crate::{config::Config, runtime};

#[derive(Clone)]
pub struct AcpHostFactory {
    config: Arc<Config>,
    profile: ExecutionProfile,
}

impl AcpHostFactory {
    pub fn new(config: Arc<Config>) -> Self {
        Self::with_profile(config, ExecutionProfile::Interactive)
    }

    pub fn with_profile(config: Arc<Config>, profile: ExecutionProfile) -> Self {
        Self { config, profile }
    }
}

impl SessionRuntimeFactory for AcpHostFactory {
    fn profile(&self) -> ExecutionProfile {
        self.profile
    }

    fn start(
        &self,
        request: StartSessionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<StartedSession>> + Send + '_>>
    {
        Box::pin(async move {
            let profile = self.profile;
            let (workspace, start) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let workspace = runtime::canonical_workspace(&request.workspace)?;
                let start = match request.start {
                    SessionStart::New => runtime::SessionStart::New {
                        inherited_models: None,
                    },
                    SessionStart::Existing { session_id } => {
                        let directory = runtime::sessions_dir(&workspace, profile);
                        // Match picker visibility without reading any other root.
                        let path = transcript::find_session_file(&directory, &session_id)?
                            .filter(|path| !transcript::is_abandoned_root(path))
                            .with_context(|| {
                                format!("no persisted session matches ID {session_id:?}")
                            })?;
                        runtime::SessionStart::Resume(path)
                    }
                };
                Ok((workspace, start))
            })
            .await
            .context("ACP session lookup task failed")??;
            let runtime::StartedSession {
                mut running,
                restored,
            } = runtime::start_acp_session(
                &self.config,
                &workspace,
                start,
                self.profile,
                request.mcp_servers,
            )
            .await?;
            let commands = running.command_sender();
            let events = running.take_event_receiver()?;
            let mut exits = running.take_exit_receiver()?;
            let runtime::RestoredSession {
                session_id,
                transcript_items,
                plan_state,
                selected_mode,
                startup_notices,
                ..
            } = restored;
            let background_exit = Box::pin(async move {
                match exits.recv().await {
                    Some(exit) => RuntimeExit {
                        component: exit.component.to_string(),
                        error: exit.error,
                    },
                    None => RuntimeExit::failed(
                        "runtime monitor",
                        "background-task monitor closed unexpectedly",
                    ),
                }
            });
            Ok(StartedSession {
                session_id,
                workspace,
                transcript_items,
                plan_state,
                selected_mode,
                startup_notices,
                commands,
                events,
                background_exit,
                lifecycle: Box::new(AcpRuntimeLifecycle {
                    running: Some(running),
                }),
            })
        })
    }

    fn list(
        &self,
        workspace: PathBuf,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Vec<SessionDescriptor>>> + Send + '_>,
    > {
        Box::pin(async move {
            let profile = self.profile;
            let (workspace, summaries) =
                tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                    let workspace = runtime::canonical_workspace(&workspace)?;
                    let summaries =
                        transcript::list_sessions(&runtime::sessions_dir(&workspace, profile))?;
                    Ok((workspace, summaries))
                })
                .await
                .context("ACP session discovery task failed")??;
            Ok(summaries
                .into_iter()
                .map(|summary| SessionDescriptor {
                    id: summary.id,
                    workspace: workspace.clone(),
                    modified: summary.modified,
                    preview: summary.preview,
                })
                .collect())
        })
    }
}

struct AcpRuntimeLifecycle {
    running: Option<runtime::RunningSession>,
}

impl SessionRuntimeLifecycle for AcpRuntimeLifecycle {
    fn shutdown(
        mut self: Box<Self>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
        let running = self.running.take();
        Box::pin(async move {
            match running {
                Some(running) => running.shutdown().await,
                None => Ok(()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::Message;
    use zevria_acp::{SessionStart, StartSessionRequest};
    use zevria_transcript::transcript::TranscriptItem;
    use zevria_transcript::transcript::TranscriptWriter;
    use zevria_transcript::transcript::sessions_dir;

    use super::*;

    #[tokio::test]
    async fn worker_restoration_is_exact_and_never_falls_back_to_roots_or_projects_markdown() {
        use zevria_foundation::ModelProfileRef;
        use zevria_foundation::TurnId;
        use zevria_workflow::PlanArtifact;
        use zevria_workflow::PlanId;
        use zevria_workflow::PlanRecord;
        use zevria_workflow::PlanVersion;
        let workspace = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        let worker_dir = runtime::sessions_dir(&canonical, ExecutionProfile::EnsembleWorker);
        let root_dir = sessions_dir(&canonical);
        let models = zevria_model::models::SessionModels::new(
            zevria_model::models::ModelSelection::new(
                ModelProfileRef::new("test", "test-model"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            zevria_model::models::ModelSelection::new(
                ModelProfileRef::new("test", "test-model"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
        let artifact = PlanArtifact {
            version: PlanVersion { id: PlanId::new(), revision: 1 },
            title: "Inspect Worker Session Storage".into(),
            markdown: "# Inspect Worker Session Storage\n\n## Goal\nIsolate.\n## Decisions\nSeparate.\n## Implementation\nInspect.\n## Validation\nTest.\n## Risks\nNone.\n".into(),
            source_turn_id: TurnId::new(1),
        };
        for (directory, id) in [
            (&worker_dir, "saved"),
            (&root_dir, "saved"),
            (&root_dir, "root-only"),
        ] {
            let mut writer = TranscriptWriter::create_with_id(directory, id).unwrap();
            writer
                .rewrite(&[
                    TranscriptItem::SessionModels(Box::new(models.clone())),
                    TranscriptItem::Message(Message::user("persisted")),
                    TranscriptItem::Plan(Box::new(PlanRecord::Started {
                        id: artifact.version.id,
                    })),
                    TranscriptItem::Plan(Box::new(PlanRecord::Ready {
                        artifact: artifact.clone(),
                    })),
                ])
                .unwrap();
        }
        let root_latest = transcript::latest_session_file(&root_dir).unwrap();
        let factory = AcpHostFactory::with_profile(
            Arc::new(crate::config::test_config()),
            ExecutionProfile::EnsembleWorker,
        );
        assert_eq!(factory.profile(), ExecutionProfile::EnsembleWorker);
        let listed = factory.list(canonical.clone()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "saved");
        for id in ["root-only", "../saved"] {
            assert!(
                factory
                    .start(StartSessionRequest {
                        mcp_servers: Vec::new(),
                        workspace: canonical.clone(),
                        start: SessionStart::Existing {
                            session_id: id.into()
                        },
                    })
                    .await
                    .is_err()
            );
        }
        for _ in 0..2 {
            let mut started = factory
                .start(StartSessionRequest {
                    mcp_servers: Vec::new(),
                    workspace: canonical.clone(),
                    start: SessionStart::Existing {
                        session_id: "saved".into(),
                    },
                })
                .await
                .unwrap();
            assert_eq!(
                started.plan_state,
                zevria_workflow::PlanWorkflowState::Ready {
                    artifact: artifact.clone()
                }
            );
            assert_eq!(started.selected_mode, zevria_foundation::SessionMode::Plan);
            assert!(
                crate::session_lease::RootSessionLease::acquire(&worker_dir.join("saved.jsonl"))
                    .is_err()
            );
            assert!(
                crate::session_lease::RootSessionLease::acquire(&root_dir.join("saved.jsonl"))
                    .is_ok()
            );
            drop(started.commands);
            drop(started.background_exit);
            started.lifecycle.shutdown().await.unwrap();
            assert!(
                started.events.try_recv().is_err(),
                "worker restoration and shutdown emit no startup lifecycle"
            );
            assert!(!worker_dir.join("saved.jsonl.lock").exists());
            assert!(
                crate::session_lease::RootSessionLease::acquire(&worker_dir.join("saved.jsonl"))
                    .is_ok()
            );
        }
        assert_eq!(
            transcript::latest_session_file(&root_dir).unwrap(),
            root_latest
        );
        assert_eq!(transcript::list_sessions(&root_dir).unwrap().len(), 2);
        assert!(!transcript::plans_dir(&canonical).exists());
    }

    #[tokio::test]
    async fn root_restoration_seeds_canonical_ready_without_repairing_edited_or_deleted_markdown() {
        use zevria_foundation::ModelProfileRef;
        use zevria_foundation::TurnId;
        use zevria_workflow::PlanArtifact;
        use zevria_workflow::PlanId;
        use zevria_workflow::PlanRecord;
        use zevria_workflow::PlanVersion;
        use zevria_workflow::PlanWorkflowState;
        let workspace = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        let directory = sessions_dir(&canonical);
        let artifact = PlanArtifact {
            version: PlanVersion { id: PlanId::new(), revision: 1 },
            title: "Restore Root Plan Snapshot".into(),
            markdown: "# Restore Root Plan Snapshot\n\n## Goal\nRestore.\n## Decisions\nSeed.\n## Implementation\nKeep canonical.\n## Validation\nTest.\n## Risks\nNone.\n".into(),
            source_turn_id: TurnId::new(1),
        };
        let mut writer = TranscriptWriter::create_with_id(&directory, "saved-plan").unwrap();
        let items = vec![
            TranscriptItem::SessionModels(Box::new(
                zevria_model::models::SessionModels::new(
                    zevria_model::models::ModelSelection::new(
                        ModelProfileRef::new("test", "test-model"),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    zevria_model::models::ModelSelection::new(
                        ModelProfileRef::new("test", "test-model"),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                )
                .unwrap(),
            )),
            TranscriptItem::Message(Message::user("plan it")),
            TranscriptItem::Plan(Box::new(PlanRecord::Started {
                id: artifact.version.id,
            })),
            TranscriptItem::Plan(Box::new(PlanRecord::Ready {
                artifact: artifact.clone(),
            })),
        ];
        writer.rewrite(&items).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        let projection = transcript::plans_dir(&canonical)
            .join("saved-plan")
            .join(format!(
                "{}-restore-root-plan-snapshot.md",
                artifact.version.id
            ));
        std::fs::create_dir_all(projection.parent().unwrap()).unwrap();
        std::fs::write(&projection, "manual edit").unwrap();
        let mut config = crate::config::test_config();
        config.source_path = Some(canonical.join("config.toml"));
        std::fs::write(
            config.source_path.as_ref().unwrap(),
            toml::to_string(&std::collections::BTreeMap::from([(
                "modes",
                &config.modes,
            )]))
            .unwrap(),
        )
        .unwrap();
        let models_path =
            zevria_foundation::config::models_path_for(config.source_path.as_ref().unwrap());
        std::fs::write(
            &models_path,
            serde_json::to_string(&serde_json::json!({
                "providers": config.providers,
            }))
            .unwrap(),
        )
        .unwrap();
        config.models_path = Some(models_path);
        let factory = AcpHostFactory::new(Arc::new(config));
        for missing in [false, true] {
            if missing {
                std::fs::remove_file(&projection).unwrap();
            }
            let mut started = factory
                .start(StartSessionRequest {
                    mcp_servers: Vec::new(),
                    workspace: canonical.clone(),
                    start: SessionStart::Existing {
                        session_id: "saved-plan".into(),
                    },
                })
                .await
                .unwrap();
            assert_eq!(
                started.plan_state,
                PlanWorkflowState::Ready {
                    artifact: artifact.clone()
                }
            );
            assert_eq!(started.selected_mode, zevria_foundation::SessionMode::Plan);
            assert_eq!(started.transcript_items, items);
            started.lifecycle.shutdown().await.unwrap();
            assert!(
                started.events.try_recv().is_err(),
                "restoration must not publish snapshots, warnings, or turns"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            if missing {
                assert!(!projection.exists());
            } else {
                assert_eq!(std::fs::read_to_string(&projection).unwrap(), "manual edit");
            }
        }
    }

    #[tokio::test]
    async fn existing_session_ids_must_be_exact_filename_components() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = sessions_dir(workspace.path());
        let mut writer =
            TranscriptWriter::create_with_id(&sessions, "safe-session").expect("create session");
        writer
            .append(&TranscriptItem::Message(Message::user("safe")))
            .expect("append session");

        let factory = AcpHostFactory::new(Arc::new(crate::config::test_config()));
        let error = match factory
            .start(StartSessionRequest {
                mcp_servers: Vec::new(),
                workspace: workspace.path().to_path_buf(),
                start: SessionStart::Existing {
                    session_id: "../safe-session".to_string(),
                },
            })
            .await
        {
            Ok(_) => panic!("path-like IDs must never be joined into the sessions directory"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("no persisted session matches ID")
        );
    }

    #[tokio::test]
    async fn existing_abandoned_roots_are_not_resumable_by_id() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = sessions_dir(workspace.path());
        let config = crate::config::test_config();
        let models = zevria_model::models::SessionModels::new(
            config.modes.build.selection(),
            config.modes.plan.selection(),
        )
        .unwrap();
        let factory = AcpHostFactory::new(Arc::new(config));
        for (id, item) in [
            ("empty", None),
            (
                "models-only",
                Some(TranscriptItem::SessionModels(Box::new(models))),
            ),
        ] {
            let mut writer = TranscriptWriter::create_with_id(&sessions, id).unwrap();
            if let Some(item) = item {
                writer.append(&item).unwrap();
            }
            let path = writer.path().to_owned();
            drop(writer);
            let before = std::fs::read(&path).unwrap();
            let Err(error) = factory
                .start(StartSessionRequest {
                    workspace: workspace.path().to_owned(),
                    start: SessionStart::Existing {
                        session_id: id.into(),
                    },
                    mcp_servers: Vec::new(),
                })
                .await
            else {
                panic!("ACP resumed an abandoned root: {id}");
            };
            assert!(
                error
                    .to_string()
                    .contains("no persisted session matches ID")
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
            assert!(!sessions.join(".leases.lock").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_session_lookup_preserves_filesystem_errors() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = sessions_dir(workspace.path());
        std::fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        std::fs::write(&sessions, "not a directory").unwrap();
        let factory = AcpHostFactory::new(Arc::new(crate::config::test_config()));
        let Err(error) = factory
            .start(StartSessionRequest {
                workspace: workspace.path().to_owned(),
                start: SessionStart::Existing {
                    session_id: "session".into(),
                },
                mcp_servers: Vec::new(),
            })
            .await
        else {
            panic!("ACP accepted an invalid sessions directory");
        };
        assert!(error.to_string().contains("failed to inspect session file"));
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotADirectory,
        );
    }
}
