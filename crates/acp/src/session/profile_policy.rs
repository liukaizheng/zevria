//! Capabilities of interactive and report-only ACP sessions.

use super::{ClientState, ExecutionProfile};

#[derive(Clone, Copy)]
pub(super) struct ProfilePolicy {
    pub(super) execution: ExecutionProfile,
    pub(super) form_elicitation: bool,
    pub(super) plan_operations: bool,
}
impl ProfilePolicy {
    pub(super) fn new(execution: ExecutionProfile, client: &ClientState) -> Self {
        Self {
            execution,
            form_elicitation: client.form_elicitation,
            plan_operations: client.plan_operations,
        }
    }
    pub(super) fn is_worker(self) -> bool {
        self.execution.is_worker()
    }
}
