//! Names for the ACP v1 enums stored in worker transcripts.

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, PlanEntryPriority, PlanEntryStatus, StopReason, ToolCallStatus, ToolKind,
};

pub(super) const fn permission_name(value: &PermissionOptionKind) -> &'static str {
    match value {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "unknown",
    }
}

pub(super) const fn plan_priority_name(value: &PlanEntryPriority) -> &'static str {
    match value {
        PlanEntryPriority::High => "high",
        PlanEntryPriority::Medium => "medium",
        PlanEntryPriority::Low => "low",
        _ => "unknown",
    }
}

pub(super) const fn plan_status_name(value: &PlanEntryStatus) -> &'static str {
    match value {
        PlanEntryStatus::Pending => "pending",
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "unknown",
    }
}

pub(super) const fn stop_reason_name(value: &StopReason) -> &'static str {
    match value {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
}

pub(super) const fn tool_status_name(value: &ToolCallStatus) -> &'static str {
    match value {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "unknown",
    }
}

pub(super) const fn tool_kind_name(value: &ToolKind) -> &'static str {
    match value {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "unknown",
    }
}
