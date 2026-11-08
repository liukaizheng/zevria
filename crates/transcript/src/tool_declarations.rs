//! Durable append-only function declarations, scoped to a compaction window.
use crate::TranscriptItem;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use zevria_model::{ToolDeclaration, ToolDeclarations};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDeclarationPlacement {
    Initial,
    Additional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolDeclarationRecord {
    pub version: u32,
    pub placement: ToolDeclarationPlacement,
    pub tools: ToolDeclarations,
}
impl ToolDeclarationRecord {
    pub fn new(
        placement: ToolDeclarationPlacement,
        tools: ToolDeclarations,
    ) -> anyhow::Result<Self> {
        let record = Self {
            version: 1,
            placement,
            tools,
        };
        record.validate()?;
        Ok(record)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported tool declarations version {}; expected 1",
            self.version
        );
        self.tools.validate()?;
        anyhow::ensure!(
            self.placement != ToolDeclarationPlacement::Additional || !self.tools.is_empty(),
            "additional tool declarations must not be empty"
        );
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ToolDeclarationRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Record {
            version: u32,
            placement: ToolDeclarationPlacement,
            tools: ToolDeclarations,
        }
        let Record {
            version,
            placement,
            tools,
        } = Record::deserialize(deserializer)?;
        let record = Self {
            version,
            placement,
            tools,
        };
        record.validate().map_err(D::Error::custom)?;
        Ok(record)
    }
}

#[derive(Debug, Default)]
pub struct ToolWindow<'a> {
    pub initial: Option<&'a ToolDeclarations>,
    pub latest: BTreeMap<&'a str, &'a ToolDeclaration>,
}

/// The current window begins just after the latest checkpoint. Validation of
/// record sequencing is performed by the instruction replay reducer on load.
pub fn tool_window(items: &[TranscriptItem]) -> ToolWindow<'_> {
    let start = items
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::Compaction(_)))
        .map_or(0, |index| index + 1);
    let mut window = ToolWindow::default();
    for item in &items[start..] {
        if let TranscriptItem::ToolDeclarations(record) = item {
            if record.placement == ToolDeclarationPlacement::Initial {
                window.initial = Some(&record.tools);
            }
            for tool in record.tools.iter() {
                window.latest.insert(&tool.name, tool);
            }
        }
    }
    window
}
