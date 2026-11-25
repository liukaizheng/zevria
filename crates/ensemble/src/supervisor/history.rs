//! Read-only log preflight and validation against the root identity.

use std::path::Path;
use zevria_transcript::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunProjection, agent_run_path, load_agent_run_projection,
};
use zevria_workflow::AgentRunDescriptor;

/// Batch preflight runs before concurrent workers or frontend restoration.
/// A missing log is a valid interruption before that worker was started.
pub fn preflight_existing_logs(
    root: &Path,
    start: &zevria_workflow::EnsembleStart,
) -> anyhow::Result<()> {
    for descriptor in &start.agents {
        load_existing_worker(root, start, descriptor)?;
    }
    Ok(())
}

pub(super) fn load_existing_worker(
    root: &Path,
    start: &zevria_workflow::EnsembleStart,
    descriptor: &AgentRunDescriptor,
) -> anyhow::Result<Option<AgentRunProjection>> {
    let path = agent_run_path(root, &start.run_id, &descriptor.id);
    if !path.try_exists()? {
        return Ok(None);
    }
    let projection = load_agent_run_projection(&path)?;
    if !projection.header.as_ref().is_some_and(|header| {
        header.version == AGENT_RUN_TRANSCRIPT_VERSION
            && header.ensemble_run_id == start.run_id
            && header.workflow == start.workflow
            && &header.descriptor == descriptor
            && header.prompt == start.prompt
    }) {
        return Err(zevria_transcript::transcript::UnsupportedHistory::new(
            &path,
            Some(1),
            "worker identity matching the root ensemble start",
        )
        .into());
    }
    Ok(Some(projection))
}
