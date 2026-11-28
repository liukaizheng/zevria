//! Transport-neutral identities and management contracts for MCP clients.
use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct McpServerName(String);

impl McpServerName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for McpServerName {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if (1..=48).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
            && !value.contains("__")
            && !value.starts_with(['-', '_'])
            && !value.ends_with(['-', '_'])
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(
                "MCP server names must be 1–48 lowercase ASCII letters, digits, hyphens or underscores, with no '__' or edge punctuation",
            )
        }
    }
}

impl TryFrom<String> for McpServerName {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<McpServerName> for String {
    fn from(value: McpServerName) -> Self {
        value.0
    }
}

impl fmt::Display for McpServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolAccess {
    #[default]
    None,
    PlanScoped,
    /// Plan policy intersected with non-destructive MCP annotations.
    ReadOnlyPlanScoped,
    AllEnabled,
}

impl McpToolAccess {
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMcpPolicy {
    None,
    #[default]
    ReadOnly,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerOrigin {
    Config,
    Acp,
}

#[derive(Clone, PartialEq, Eq)]
pub enum AcpMcpServerSpec {
    Stdio {
        name: McpServerName,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        name: McpServerName,
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for AcpMcpServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { name, .. } => f
                .debug_struct("Stdio")
                .field("name", name)
                .finish_non_exhaustive(),
            Self::Http { name, .. } => f
                .debug_struct("Http")
                .field("name", name)
                .finish_non_exhaustive(),
        }
    }
}

pub const MCP_TOOL_PREFIX: &str = "mcp__";

pub fn is_mcp_external_tool_name(name: &str) -> bool {
    name.starts_with(MCP_TOOL_PREFIX)
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    is_mcp_external_tool_name(name) || crate::MCP_RESOURCE_TOOL_NAMES.contains(&name)
}

pub fn parse_mcp_tool_prefix(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(MCP_TOOL_PREFIX)?.split_once("__")
}

pub fn mcp_wire_tool_name(server: &McpServerName, original_name: &str) -> String {
    mcp_wire_tool_name_with_disambiguator(server, original_name, 0)
}

/// Reserve digest-shaped suffixes for names that need mangling. Without this,
/// a later legal MCP name could take over an already advertised tool identity.
/// The disambiguator is only for an actual collision in the short digest.
pub fn mcp_wire_tool_name_with_disambiguator(
    server: &McpServerName,
    original_name: &str,
    disambiguator: u32,
) -> String {
    let sanitized: String = original_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut result = format!("{MCP_TOOL_PREFIX}{server}__{sanitized}");
    let digest_suffix = sanitized
        .as_bytes()
        .last_chunk::<9>()
        .is_some_and(|suffix| suffix[0] == b'_' && suffix[1..].iter().all(u8::is_ascii_hexdigit));
    if disambiguator != 0
        || result.len() > 64
        || sanitized != original_name
        || digest_suffix
        || sanitized.is_empty()
    {
        let mut digest = Sha256::new();
        digest.update(server.as_str());
        digest.update([0]);
        digest.update(original_name);
        if disambiguator != 0 {
            digest.update([0]);
            digest.update(disambiguator.to_be_bytes());
        }
        result.truncate(55);
        let hash = digest.finalize();
        use fmt::Write as _;
        write!(
            &mut result,
            "_{:08x}",
            u32::from_be_bytes(hash[..4].try_into().unwrap())
        )
        .unwrap();
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    Connected,
    Connecting,
    Disabled,
    Disconnected,
    Failed {
        error: String,
        stderr_tail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransportSummary {
    Stdio { command: String, args: Vec<String> },
    Http { redacted_url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub wire_name: String,
    pub description: Option<String>,
    pub read_only: bool,
    pub enabled: bool,
    pub plan_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceTemplateInfo {
    pub uri_template: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgumentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgumentInfo {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSummary {
    pub name: McpServerName,
    pub origin: McpServerOrigin,
    pub enabled: bool,
    pub status: McpServerStatus,
    pub transport: McpTransportSummary,
    pub plan_mode: PlanMcpPolicy,
    pub tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
    pub connect_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDetail {
    pub summary: McpServerSummary,
    pub server_info: Option<String>,
    pub instructions: Option<String>,
    pub tools: Vec<McpToolInfo>,
    pub resources: Vec<McpResourceInfo>,
    pub templates: Vec<McpResourceTemplateInfo>,
    pub prompts: Vec<McpPromptInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpManagementView {
    pub revision: String,
    pub enabled: bool,
    pub servers: Vec<McpServerSummary>,
    pub selected: Option<McpServerDetail>,
    pub diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<(McpServerName, McpPromptInfo)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpManagementRequest {
    List,
    Inspect {
        server: McpServerName,
    },
    Reload,
    Reconnect {
        server: McpServerName,
    },
    SetEnabled {
        expected_revision: String,
        server: McpServerName,
        enabled: bool,
    },
}

impl McpManagementRequest {
    pub fn is_query(&self) -> bool {
        matches!(self, Self::List | Self::Inspect { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpManagementResult {
    View {
        view: McpManagementView,
    },
    Changed {
        revision: String,
        connected: usize,
        total: usize,
        tool_count: usize,
        unchanged: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

impl McpManagementResult {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub const MAX_MCP_INSTRUCTIONS_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInstruction {
    pub server: McpServerName,
    pub digest: String,
    pub body: String,
}

impl McpServerInstruction {
    pub fn from_body(server: McpServerName, body: &str) -> Option<Self> {
        let body = body.trim();
        let body = body[..body.floor_char_boundary(MAX_MCP_INSTRUCTIONS_BYTES)].trim_end();
        (!body.is_empty()).then(|| Self {
            server,
            digest: Sha256::digest(body.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            body: body.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallDetail {
    pub server: McpServerName,
    pub tool: String,
    pub duration_ms: u64,
    pub content_blocks: usize,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated_and_mangling_is_stable_and_unambiguous() {
        for name in ["", "-name", "name_", "a__b", "A", "中文"] {
            assert!(name.parse::<McpServerName>().is_err());
        }
        let server = "my_server".parse().unwrap();
        assert_eq!(
            mcp_wire_tool_name(&server, "sample"),
            "mcp__my_server__sample"
        );
        assert_ne!(
            mcp_wire_tool_name(&server, "a.b"),
            mcp_wire_tool_name(&server, "a_b")
        );
        let long = mcp_wire_tool_name(&server, &"ä".repeat(100));
        assert_eq!(long.len(), 64);
        assert!(long.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        assert_eq!(parse_mcp_tool_prefix(&long).unwrap().0, "my_server");
    }
}
