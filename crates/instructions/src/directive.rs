//! Versioned, engine-owned skill and MCP directives. Transcript records persist their
//! semantic payloads at exact historical positions and re-render immutable model
//! input on load. Historical skill activation pins remain lifecycle authority.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zevria_foundation::{
    MAX_MCP_INSTRUCTIONS_BYTES, McpServerInstruction, McpServerName, McpToolAccess,
    WorkspaceBinding, WorkspaceContract,
};

use crate::{
    TurnPolicy,
    skill::{ActiveSkills, SkillDigest, SkillName, SkillSnapshot},
};

pub const INSTRUCTION_VERSION: u32 = 1;

pub(crate) fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Full effective policy, not just the enum identifying a mode in this binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectivePolicy {
    pub scope: String,
    pub instructions: String,
    pub skills_enabled: bool,
    pub allowed_tool_names: Option<Vec<String>>,
    pub orchestration: bool,
    pub contract: WorkspaceContract,
    pub workspace: Option<WorkspaceBinding>,
    #[serde(default, skip_serializing_if = "McpToolAccess::is_none")]
    pub mcp_access: McpToolAccess,
}

/// Field order here is part of the deterministic model-facing format.
#[derive(Serialize)]
struct PolicyDeclaration<'a> {
    scope: &'a str,
    tools: ToolDeclaration<'a>,
    skills: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtasks: Option<&'static [zevria_foundation::SubtaskKind]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    orchestration: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a WorkspaceBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inspection: Option<WorkspaceContract>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp: Option<McpToolAccess>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ToolDeclaration<'a> {
    Allowed(&'a [String]),
    Registered(&'static str),
}
impl DirectivePolicy {
    pub fn allows_tool(&self, name: &str) -> bool {
        if zevria_foundation::SKILL_TOOL_NAMES.contains(&name) && !self.skills_enabled {
            return false;
        }
        self.allowed_tool_names
            .as_ref()
            .is_none_or(|names| names.iter().any(|candidate| candidate == name))
    }

    pub fn allows_skill_activation(&self) -> bool {
        self.allows_tool(zevria_foundation::SKILL_TOOL_NAME)
    }

    pub(crate) fn declaration(&self) -> String {
        let subtasks = self
            .allows_tool(zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME)
            .then_some(&[zevria_foundation::SubtaskKind::Explore][..]);
        serde_json::to_string(&PolicyDeclaration {
            scope: &self.scope,
            tools: self.allowed_tool_names.as_deref().map_or(
                ToolDeclaration::Registered("registered"),
                ToolDeclaration::Allowed,
            ),
            skills: self.skills_enabled,
            subtasks,
            orchestration: self
                .orchestration
                .then_some("explicit_request_only: explore, build; concurrent_batch_required"),
            workspace: self.workspace.as_ref(),
            inspection: (self.contract == WorkspaceContract::SourceReadOnlyScratch)
                .then_some(self.contract),
            mcp: (!self.mcp_access.is_none()).then_some(self.mcp_access),
        })
        .expect("policy declaration serializes")
    }

    pub fn new(scope: impl Into<String>, policy: &TurnPolicy) -> Self {
        Self {
            scope: scope.into(),
            instructions: policy.instructions.clone(),
            skills_enabled: policy.skills_enabled,
            allowed_tool_names: policy.allowed_tool_names.clone(),
            orchestration: policy.orchestration,
            contract: policy.contract,
            workspace: policy.workspace.clone(),
            mcp_access: policy.mcp_access,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectivePayload {
    SkillBody {
        name: SkillName,
        digest: SkillDigest,
        body: String,
    },
    SkillRevocation {
        name: SkillName,
        reason: String,
    },
    McpInstructions {
        server: McpServerName,
        digest: String,
        body: String,
    },
    McpInstructionsRevocation {
        server: McpServerName,
        reason: String,
    },
}
impl DirectivePayload {
    pub fn key(&self) -> String {
        match self {
            Self::SkillBody { name, .. } | Self::SkillRevocation { name, .. } => {
                format!("skill:{name}")
            }
            Self::McpInstructions { server, .. }
            | Self::McpInstructionsRevocation { server, .. } => format!("mcp:{server}"),
        }
    }

    fn is_revocation(&self) -> bool {
        matches!(
            self,
            Self::SkillRevocation { .. } | Self::McpInstructionsRevocation { .. }
        )
    }

    pub(crate) fn render(&self) -> String {
        match self {
            Self::SkillBody { name, digest, body } => format!(
                "Skill directive: enable \"{name}\" (snapshot {digest}).\n{body}\nSkill directive: end \"{name}\"."
            ),
            Self::SkillRevocation { name, reason } => {
                format!("Skill directive: revoke \"{name}\": {reason}.")
            }
            Self::McpInstructions {
                server,
                digest,
                body,
            } => format!(
                "MCP server instructions: enable \"{server}\" (snapshot {digest}).\n{body}\nMCP server instructions: end \"{server}\"."
            ),
            Self::McpInstructionsRevocation { server, reason } => {
                format!("MCP server instructions: revoke \"{server}\": {reason}.")
            }
        }
    }
}

/// Semantic payload and its verbatim, validated wire representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveContent {
    pub version: u32,
    pub payload: DirectivePayload,
    pub text: String,
}
impl DirectiveContent {
    pub fn new(payload: DirectivePayload) -> anyhow::Result<Self> {
        let content = Self {
            version: INSTRUCTION_VERSION,
            text: payload.render(),
            payload,
        };
        content.validate()?;
        Ok(content)
    }
    pub fn skill(snapshot: &SkillSnapshot) -> Self {
        Self::new(DirectivePayload::SkillBody {
            name: snapshot.name().clone(),
            digest: snapshot.digest(),
            body: snapshot.body().into(),
        })
        .expect("validated skill snapshot")
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == INSTRUCTION_VERSION,
            "unsupported directive version; start a fresh session"
        );
        match &self.payload {
            DirectivePayload::SkillBody { body, .. } => anyhow::ensure!(
                !body.is_empty()
                    && body == body.trim()
                    && body.len() as u64 <= crate::skill::MAX_SKILL_BYTES,
                "skill directive body must be canonical bounded text"
            ),
            DirectivePayload::SkillRevocation { reason, .. } => {
                anyhow::ensure!(!reason.trim().is_empty(), "empty skill revocation reason")
            }
            DirectivePayload::McpInstructions {
                body,
                digest: expected,
                ..
            } => anyhow::ensure!(
                !body.is_empty()
                    && body == body.trim()
                    && body.len() <= MAX_MCP_INSTRUCTIONS_BYTES
                    && digest(body) == *expected,
                "MCP instructions must be canonical bounded text with a matching digest"
            ),
            DirectivePayload::McpInstructionsRevocation { reason, .. } => anyhow::ensure!(
                !reason.is_empty()
                    && reason == reason.trim()
                    && reason.len() <= 1024
                    && !reason.contains(['\n', '\r']),
                "invalid MCP instructions revocation reason"
            ),
        }
        anyhow::ensure!(
            self.text == self.payload.render(),
            "directive text is inconsistent with its supported semantic format"
        );
        Ok(())
    }
}

/// Transient effective skill bodies for validation and accounting.
/// Never persisted in a compaction checkpoint or inferred from summary prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveSnapshot {
    pub version: u32,
    pub directives: Vec<DirectiveContent>,
}
impl Default for DirectiveSnapshot {
    fn default() -> Self {
        Self {
            version: INSTRUCTION_VERSION,
            directives: Vec::new(),
        }
    }
}
impl DirectiveSnapshot {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == INSTRUCTION_VERSION,
            "unsupported instruction snapshot version"
        );
        let mut previous = None;
        for directive in &self.directives {
            directive.validate()?;
            anyhow::ensure!(
                !directive.payload.is_revocation(),
                "instruction snapshots contain only effective components"
            );
            let key = directive.payload.key();
            anyhow::ensure!(
                previous.as_ref().is_none_or(|previous| previous < &key),
                "effective directives must be uniquely ordered"
            );
            previous = Some(key);
        }
        Ok(())
    }
}

/// Borrow effective directives at a live or resumed projection boundary. Uses
/// the same keys and revocation semantics as `DirectiveState`, without copying
/// bodies. Checkpoints never reset this state; loaded histories retain the
/// original directive positions.
pub fn effective_directives<'a>(
    directives: impl IntoIterator<Item = &'a DirectiveContent>,
) -> Vec<&'a DirectiveContent> {
    let mut state = BTreeMap::new();
    for directive in directives {
        let key = directive.payload.key();
        if directive.payload.is_revocation() {
            state.remove(&key);
        } else {
            state.insert(key, directive);
        }
    }
    state.into_values().collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectiveState(BTreeMap<String, DirectiveContent>);
impl DirectiveState {
    pub fn from_snapshot(snapshot: &DirectiveSnapshot) -> anyhow::Result<Self> {
        snapshot.validate()?;
        let mut state = Self::default();
        for directive in &snapshot.directives {
            state.apply(directive)?;
        }
        Ok(state)
    }
    pub fn snapshot(&self) -> DirectiveSnapshot {
        DirectiveSnapshot {
            version: INSTRUCTION_VERSION,
            directives: self.0.values().cloned().collect(),
        }
    }
    pub fn apply(&mut self, directive: &DirectiveContent) -> anyhow::Result<()> {
        directive.validate()?;
        self.apply_validated(directive);
        Ok(())
    }

    /// Install content whose local fields were already checked while decoding.
    #[doc(hidden)]
    pub fn apply_validated(&mut self, directive: &DirectiveContent) {
        let key = directive.payload.key();
        if directive.payload.is_revocation() {
            self.0.remove(&key);
        } else {
            self.0.insert(key, directive.clone());
        }
    }

    pub fn reconcile(
        &self,
        active: &ActiveSkills,
        enabled: impl Fn(&SkillName) -> bool,
    ) -> Vec<DirectiveContent> {
        let mut changes = Vec::new();
        for current in self.0.values() {
            if let DirectivePayload::SkillBody { name, .. } = &current.payload
                && (!enabled(name) || !active.contains(name))
            {
                changes.push(
                    DirectiveContent::new(DirectivePayload::SkillRevocation {
                        name: name.clone(),
                        reason: "disabled by the current skill or workflow policy".into(),
                    })
                    .expect("valid skill revocation"),
                );
            }
        }
        for snapshot in active
            .snapshots()
            .filter(|snapshot| enabled(snapshot.name()))
        {
            let directive = DirectiveContent::skill(snapshot);
            if self.0.get(&directive.payload.key()) != Some(&directive) {
                changes.push(directive);
            }
        }
        changes.sort_by_key(|directive| directive.payload.key());
        changes
    }

    pub fn reconcile_mcp(
        &self,
        desired: &[McpServerInstruction],
    ) -> anyhow::Result<Vec<DirectiveContent>> {
        let mut changes = BTreeMap::new();
        let mut active = std::collections::BTreeSet::new();
        for McpServerInstruction {
            server,
            digest,
            body,
        } in desired
        {
            anyhow::ensure!(
                active.insert(server),
                "duplicate MCP instructions for {server}"
            );
            let directive = DirectiveContent::new(DirectivePayload::McpInstructions {
                server: server.clone(),
                digest: digest.clone(),
                body: body.clone(),
            })?;
            let key = directive.payload.key();
            if self.0.get(&key) != Some(&directive) {
                changes.insert(key, directive);
            }
        }
        for (key, old) in &self.0 {
            if let DirectivePayload::McpInstructions { server, .. } = &old.payload
                && !active.contains(server)
            {
                changes.insert(
                    key.clone(),
                    DirectiveContent::new(DirectivePayload::McpInstructionsRevocation {
                        server: server.clone(),
                        reason: "server unavailable or disabled by the current workflow policy"
                            .into(),
                    })?,
                );
            }
        }
        Ok(changes.into_values().collect())
    }
}
