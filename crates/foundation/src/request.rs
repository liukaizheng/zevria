//! Explicit, request-local behavior. Conversation prose never grants authorization.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestBehavior {
    #[default]
    Standard,
    Orchestrate,
}

/// Versioned intent on the owning user prompt. A fresh submission/edit always
/// receives a fresh identity; accepted launches are deliberately not persisted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RequestMetadataRecord")]
pub struct RequestMetadata {
    pub version: u32,
    pub id: String,
    pub behavior: RequestBehavior,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestMetadataRecord {
    version: u32,
    id: String,
    behavior: RequestBehavior,
}

impl TryFrom<RequestMetadataRecord> for RequestMetadata {
    type Error = anyhow::Error;
    fn try_from(record: RequestMetadataRecord) -> Result<Self, Self::Error> {
        let value = Self {
            version: record.version,
            id: record.id,
            behavior: record.behavior,
        };
        value.validate()?;
        Ok(value)
    }
}

impl RequestMetadata {
    pub fn new(behavior: RequestBehavior) -> Self {
        Self {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            behavior,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported request metadata version; expected 1"
        );
        anyhow::ensure!(
            uuid::Uuid::parse_str(&self.id).is_ok(),
            "invalid request identity"
        );
        Ok(())
    }
}
