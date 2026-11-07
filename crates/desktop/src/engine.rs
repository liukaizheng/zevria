//! Tokio owns engine execution; GPUI foreground tasks await its handles directly.
use std::{future::Future, sync::Arc};

pub const LOG_FILE: &str = "zevria-desktop.log";

#[derive(Clone)]
pub struct EngineRuntime(pub Arc<tokio::runtime::Runtime>);
impl gpui_kit::Global for EngineRuntime {}
impl EngineRuntime {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("zevria-engine")
                .build()?,
        )))
    }
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.0.spawn(future)
    }
}
/// macOS does not return from run(): explicitly flush before calling quit.
pub struct LogGuard(pub Option<tracing_appender::non_blocking::WorkerGuard>);
impl gpui_kit::Global for LogGuard {}
