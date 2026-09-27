//! Policy orchestration.

use std::fmt;

/// The user-selected workflow mode for one submitted turn.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Build,
    Plan,
}

impl SessionMode {
    pub const ALL: [Self; 2] = [Self::Build, Self::Plan];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        match self {
            Self::Build => 0,
            Self::Plan => 1,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
        }
    }
}

/// Provider-neutral model profile selected for a request.
///
/// A model adapter maps these workflow roles to its concrete configured model
/// identifiers. Keeping the role separate from [`SessionMode`] lets independent
/// children retain nominal Build-mode semantics while using dedicated Explore
/// and Builder models.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Build,
    Plan,
    Review,
    Explore,
    Builder,
}

impl ModelRole {
    pub const ALL: [Self; 5] = [
        Self::Build,
        Self::Plan,
        Self::Review,
        Self::Explore,
        Self::Builder,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        match self {
            Self::Build => 0,
            Self::Plan => 1,
            Self::Review => 2,
            Self::Explore => 3,
            Self::Builder => 4,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Explore => "explore",
            Self::Builder => "builder",
        }
    }
}

impl fmt::Display for ModelRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Session-local identity for one submitted root or child turn.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct TurnId(u64);

impl TurnId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Build => "Build",
            Self::Plan => "Plan",
        })
    }
}

/// Behavioral workspace contract; command execution is not an OS sandbox.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceContract {
    #[default]
    Mutable,
    SourceReadOnlyScratch,
}

/// Absolute roots supplied to an independent Build child.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    pub root: String,
    pub startup: String,
}

/// Immutable model role, instructions, and tool access applied to one complete
/// submission.
///
/// `model_role` selects a provider-configured model profile. `instructions` is
/// the workflow module body in the request-owned instruction set, not persisted
/// conversation input. `allowed_tool_names` uses `None` for every registered tool
/// and opted-in hosted capability and `Some(names)` for an explicit allow-list
/// (including `WEB_SEARCH_TOOL_NAME`); an empty list advertises and permits no tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPolicy {
    pub scope: String,
    pub model_role: ModelRole,
    pub instructions: String,
    pub allowed_tool_names: Option<Vec<String>>,
    /// Explicit capability gate for both new activations and active-skill
    /// request materialization. This is not inferred from a tool name.
    pub skills_enabled: bool,
    /// Static eligibility for explicit root request orchestration. This does not
    /// itself authorize Builder delegation; accepted request behavior does.
    pub orchestration: bool,
    pub contract: WorkspaceContract,
    pub workspace: Option<WorkspaceBinding>,
}

impl TurnPolicy {
    pub fn new(
        instructions: impl Into<String>,
        allowed_tool_names: Option<Vec<String>>,
        model_role: ModelRole,
        skills_enabled: bool,
    ) -> Self {
        Self {
            scope: model_role.name().into(),
            model_role,
            instructions: instructions.into(),
            allowed_tool_names,
            skills_enabled,
            orchestration: false,
            contract: WorkspaceContract::Mutable,
            workspace: None,
        }
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    pub fn with_orchestration(mut self) -> Self {
        self.orchestration = true;
        self
    }

    pub fn with_contract(mut self, contract: WorkspaceContract) -> Self {
        self.contract = contract;
        self
    }

    pub fn with_workspace(mut self, workspace: WorkspaceBinding) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn subtask_kinds(&self) -> Option<&'static [crate::SubtaskKind]> {
        self.allows_tool(crate::LAUNCH_SUBTASKS_TOOL_NAME)
            .then_some(&[crate::SubtaskKind::Explore])
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        if crate::tool_names::SKILL_TOOL_NAMES.contains(&name) && !self.skills_enabled {
            return false;
        }
        self.allowed_tool_names
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LAUNCH_SUBTASKS_TOOL_NAME, SubtaskKind};

    #[test]
    fn subtask_kinds_follow_capabilities_not_nominal_roles() {
        for role in ModelRole::ALL {
            let policy = TurnPolicy::new(
                "role",
                Some(vec![LAUNCH_SUBTASKS_TOOL_NAME.into()]),
                role,
                false,
            );
            assert!(!policy.orchestration);
            assert_eq!(policy.contract, WorkspaceContract::Mutable);
            assert_eq!(policy.workspace, None);
            assert_eq!(policy.subtask_kinds(), Some(&[SubtaskKind::Explore][..]));
            assert_eq!(
                policy.with_orchestration().subtask_kinds(),
                Some(&[SubtaskKind::Explore][..])
            );
            let denied = TurnPolicy::new("role", Some(vec![]), role, false).with_orchestration();
            assert_eq!(denied.subtask_kinds(), None);
        }
        assert_eq!(
            TurnPolicy::new("registered", None, ModelRole::Build, false).subtask_kinds(),
            Some(&[SubtaskKind::Explore][..])
        );
    }
}

/// Stable policies for both root modes, supplied by the composition root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPolicies {
    build: TurnPolicy,
    plan: TurnPolicy,
}

impl SessionPolicies {
    pub fn new(build: TurnPolicy, plan: TurnPolicy) -> Self {
        Self { build, plan }
    }

    pub fn policy_mut(&mut self, mode: SessionMode) -> &mut TurnPolicy {
        match mode {
            SessionMode::Build => &mut self.build,
            SessionMode::Plan => &mut self.plan,
        }
    }

    pub fn policy(&self, mode: SessionMode) -> &TurnPolicy {
        match mode {
            SessionMode::Build => &self.build,
            SessionMode::Plan => &self.plan,
        }
    }
}
