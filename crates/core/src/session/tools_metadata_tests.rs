use super::*;

#[test]
fn local_display_diagnostics_leave_model_result_bytes_unchanged() {
    let call = ToolCall::new(
        rig_core::message::ToolCallId::new_or_mint("correlated"),
        rig_core::message::ToolFunction::new(
            "command".into(),
            serde_json::json!({"command":"echo"}),
        ),
    );
    for (slot, outcome, diagnostic, original) in [
        (
            candidate_locked_slot(&call),
            ToolCallOutcome::Denied,
            "a Plan artifact was already accepted for this turn; make no more tool calls and provide a short final confirmation",
            "status: denied\nreason: a Plan artifact was already accepted for this turn; make no more tool calls and provide a short final confirmation",
        ),
        (
            cancelled_slot(&call),
            ToolCallOutcome::Cancelled,
            "the parent turn was cancelled before this call started",
            "status: cancelled\nreason: the parent turn was cancelled before this call started",
        ),
        (
            skill_lifecycle_error_slot(&call, "not accepted"),
            ToolCallOutcome::Error,
            "skill application was rejected: not accepted",
            "status: error\nerror: skill application was rejected: not accepted",
        ),
        (
            ensemble_gate_denied_slot(&call, "not confirmed"),
            ToolCallOutcome::Denied,
            "not confirmed",
            "status: denied\nreason: not confirmed",
        ),
    ] {
        assert_eq!(slot.metadata.outcome, outcome);
        assert_eq!(slot.metadata.diagnostic.as_deref(), Some(diagnostic));
        let UserContent::ToolResult(result) = slot.result else {
            panic!("tool result");
        };
        assert_eq!(result.content, vec![ToolResultContent::text(original)]);
        let wire = serde_json::to_value(result).unwrap();
        assert!(wire.get("diagnostic").is_none());
        assert!(wire.get("outcome").is_none());
    }
}
