use super::*;

#[test]
fn local_display_diagnostics_leave_model_result_bytes_unchanged() {
    let call = ToolCall::new(
        rig_core::message::CallId::from_wire("correlated"),
        rig_core::message::ToolFunction::new(
            rig_core::message::ToolName::new("command").expect("nonempty tool name"),
            serde_json::json!({"command":"echo"}),
        ),
    );
    let call = EngineCall::new(&call);
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

#[test]
fn batch_plan_retains_call_order_and_resolves_policy_once() {
    for (names, expected) in [
        (vec!["skill", "skill"], BatchPolicy::SkillOnly),
        (vec!["skill", "command"], BatchPolicy::MixedRejected),
        (
            vec!["create_plan", "read_plan", "submit_plan"],
            BatchPolicy::Serialized,
        ),
        (
            vec!["command", "launch_subtasks", "other"],
            BatchPolicy::Default,
        ),
    ] {
        let calls = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                ToolCall::new(
                    rig_core::message::CallId::from_wire(index.to_string()),
                    rig_core::message::ToolFunction::new(
                        rig_core::message::ToolName::new(*name).unwrap(),
                        serde_json::json!({}),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let plan = BatchPlan::new(&calls, &PlanSubmissionGate::inert(None));
        assert!(plan.policy == expected);
        assert_eq!(
            plan.calls
                .iter()
                .map(|call| call.function.name.as_str())
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(
            plan.calls.iter().map(|call| call.tool).collect::<Vec<_>>(),
            names
                .iter()
                .map(|name| EngineTool::resolve(name))
                .collect::<Vec<_>>()
        );
    }
}
