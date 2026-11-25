//! Bounded, best-effort raw ACP diagnostics.

use super::logging::RunLog;
use agent_client_protocol::LineDirection;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use tokio::sync::{mpsc, oneshot};
use zevria_workflow::{AgentProtocolDirection, AgentRunEvent};

pub(super) const DEBUG_LOG_QUEUE_CAPACITY: usize = 64;
pub(super) const RAW_DIAGNOSTIC_MAX_LINE_BYTES: usize = 256 * 1_024;
pub(super) const RAW_DIAGNOSTIC_MAX_RUN_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct RawDiagnosticSender {
    sender: mpsc::Sender<RawDiagnosticMessage>,
    pub(super) dropped_lines: Arc<AtomicU64>,
}

enum RawDiagnosticMessage {
    Line(String, LineDirection),
    Barrier(oneshot::Sender<()>),
}

pub(super) struct RawDiagnosticSink {
    sender: RawDiagnosticSender,
    task: tokio::task::JoinHandle<()>,
    log: RunLog,
}

impl RawDiagnosticSender {
    pub(super) fn record(&self, line: &str, direction: LineDirection) {
        if line.len() > RAW_DIAGNOSTIC_MAX_LINE_BYTES {
            self.dropped_lines.fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }
        match self
            .sender
            .try_send(RawDiagnosticMessage::Line(line.to_string(), direction))
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_lines.fetch_add(1, AtomicOrdering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Only the optional diagnostic copy is redacted. Typed accepted inputs and
/// actual ACP transport retain their exact bytes and identities.
pub(super) fn image_safe_protocol_diagnostic(line: &str) -> String {
    zevria_content::image_diagnostics::text_copy(line)
}

impl RawDiagnosticSink {
    pub(super) fn spawn(log: &RunLog) -> Self {
        let (sender, mut receiver) = mpsc::channel(DEBUG_LOG_QUEUE_CAPACITY);
        let dropped_lines = Arc::new(AtomicU64::new(0));
        let task_drops = dropped_lines.clone();
        let task_log = log.clone();
        let task = tokio::spawn(async move {
            let mut retained_bytes = 0usize;
            let mut truncated = false;
            while let Some(message) = receiver.recv().await {
                let dropped = task_drops.swap(0, AtomicOrdering::AcqRel);
                if dropped > 0 {
                    record_debug_truncation(
                        &task_log,
                        format!("the bounded diagnostic queue dropped at least {dropped} line(s)"),
                    )
                    .await;
                    truncated = true;
                    break;
                }
                let (line, direction) = match message {
                    RawDiagnosticMessage::Line(line, direction) => (line, direction),
                    RawDiagnosticMessage::Barrier(acknowledgement) => {
                        let _ = acknowledgement.send(());
                        continue;
                    }
                };
                let line = image_safe_protocol_diagnostic(&line);
                let event = match direction {
                    LineDirection::Stdin => AgentRunEvent::Protocol {
                        direction: AgentProtocolDirection::ClientToAgent,
                        json: line,
                    },
                    LineDirection::Stdout => AgentRunEvent::Protocol {
                        direction: AgentProtocolDirection::AgentToClient,
                        json: line,
                    },
                    LineDirection::Stderr => AgentRunEvent::Stderr { text: line },
                };
                #[derive(serde::Serialize)]
                struct EventRecord<'a> {
                    record: &'static str,
                    event: &'a AgentRunEvent,
                }
                let mut count = zevria_foundation::BoundedCountingWriter::new(
                    RAW_DIAGNOSTIC_MAX_RUN_BYTES - retained_bytes,
                );
                if serde_json::to_writer(
                    &mut count,
                    &EventRecord {
                        record: "event",
                        event: &event,
                    },
                )
                .is_err()
                {
                    record_debug_truncation(
                        &task_log,
                        format!(
                            "raw ACP diagnostics reached the {RAW_DIAGNOSTIC_MAX_RUN_BYTES}-byte per-run limit"
                        ),
                    )
                    .await;
                    truncated = true;
                    break;
                }
                if let Err(error) = task_log.emit(event).await {
                    task_log.set_failure(error.to_string());
                    break;
                }
                retained_bytes += count.written();
            }
            let dropped = task_drops.swap(0, AtomicOrdering::AcqRel);
            if !truncated && dropped > 0 {
                record_debug_truncation(
                    &task_log,
                    format!("the bounded diagnostic queue dropped at least {dropped} line(s)"),
                )
                .await;
            }
        });
        Self {
            sender: RawDiagnosticSender {
                sender,
                dropped_lines,
            },
            task,
            log: log.clone(),
        }
    }

    pub(super) fn sender(&self) -> RawDiagnosticSender {
        self.sender.clone()
    }

    pub(super) async fn barrier(&self) {
        let (acknowledgement, receiver) = oneshot::channel();
        if self
            .sender
            .sender
            .send(RawDiagnosticMessage::Barrier(acknowledgement))
            .await
            .is_ok()
        {
            let _ = receiver.await;
        }
    }

    pub(super) async fn finish(self) {
        drop(self.sender);
        if let Err(error) = self.task.await {
            self.log
                .set_failure(format!("raw ACP diagnostic task failed: {error}"));
        }
    }
}

pub(super) async fn record_debug_truncation(log: &RunLog, reason: String) {
    if let Err(error) = log
        .emit(AgentRunEvent::Unsupported {
            context: "ACP raw diagnostics".to_string(),
            placeholder: format!("[diagnostics truncated: {reason}]"),
        })
        .await
    {
        log.set_failure(error.to_string());
    }
}
