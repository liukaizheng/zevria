//! Version 1 Zevria-specific ACP extensions (ordered text/image arguments). Exact typed SDK dispatch avoids
//! the pinned stable-v1 generic extension parser's underscore stripping.
use agent_client_protocol::schema::v1::{SessionId, StopReason};
use agent_client_protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use zevria_instructions::skill::SkillCatalogCounts;
use zevria_instructions::skill::SkillManagementResult;
use zevria_instructions::skill::SkillName;

pub(crate) const SKILLS_EXTENSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub struct SkillsResponse {
    pub version: u32,
    pub result: SkillManagementResult,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvokeResponse {
    pub version: u32,
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_zevria/skills/list", response = SkillsResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsListRequest {
    pub version: u32,
    pub session_id: SessionId,
    #[serde(default)]
    pub query: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_zevria/skills/inspect", response = SkillsResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsInspectRequest {
    pub version: u32,
    pub session_id: SessionId,
    pub name: SkillName,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_zevria/skills/reload", response = SkillsResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsReloadRequest {
    pub version: u32,
    pub session_id: SessionId,
    pub expected_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_zevria/skills/config/write", response = SkillsResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsConfigWriteRequest {
    pub version: u32,
    pub session_id: SessionId,
    pub expected_revision: String,
    pub name: SkillName,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_zevria/skills/invoke", response = SkillInvokeResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillInvokeRequest {
    pub version: u32,
    pub session_id: SessionId,
    pub name: SkillName,
    #[serde(default)]
    pub args: Vec<agent_client_protocol::schema::v1::ContentBlock>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_zevria/skills/changed")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsChangedNotification {
    pub version: u32,
    pub session_id: SessionId,
    pub revision: String,
    pub counts: SkillCatalogCounts,
}

pub(crate) fn check_version(version: u32) -> Result<(), agent_client_protocol::schema::v1::Error> {
    if version == SKILLS_EXTENSION_VERSION {
        Ok(())
    } else {
        Err(
            agent_client_protocol::schema::v1::Error::invalid_params().data(format!(
                "unsupported Zevria skills extension version; expected {SKILLS_EXTENSION_VERSION}"
            )),
        )
    }
}
