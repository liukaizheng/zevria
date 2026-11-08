//! Provider-neutral, ordered function declarations. Host-only annotations never
//! enter the model-visible declaration or its durable representation.
use rig_core::completion::ToolDefinition;
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::BTreeSet, ops::Deref, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDeclaration {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.trim().is_empty(),
            "tool declaration name must not be blank"
        );
        anyhow::ensure!(
            self.parameters.is_object(),
            "tool declaration parameters must be an object"
        );
        Ok(())
    }
}

impl From<&ToolDefinition> for ToolDeclaration {
    fn from(tool: &ToolDefinition) -> Self {
        Self {
            name: tool.name.to_string(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        }
    }
}

/// A cheap-to-clone declaration snapshot. Array order and schema key order are
/// preserved through JSONL round trips; neither is canonicalized or sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolDeclarations(Arc<[ToolDeclaration]>);

impl ToolDeclarations {
    pub fn new(tools: Vec<ToolDeclaration>) -> anyhow::Result<Self> {
        let declarations = Self(tools.into());
        declarations.validate()?;
        Ok(declarations)
    }

    pub fn from_definitions(
        definitions: &[Arc<ToolDefinition>],
        mut filter: impl FnMut(&ToolDefinition) -> bool,
    ) -> anyhow::Result<Self> {
        Self::new(
            definitions
                .iter()
                .filter(|tool| filter(tool))
                .map(|tool| ToolDeclaration::from(tool.as_ref()))
                .collect(),
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        for tool in self.iter() {
            tool.validate()?;
            anyhow::ensure!(
                names.insert(&tool.name),
                "duplicate tool declaration name {}",
                tool.name
            );
        }
        Ok(())
    }

    pub fn approximate_tokens(&self) -> u64 {
        crate::compaction::approximate_tokens_from_bytes(
            serde_json::to_vec(self)
                .expect("tool declarations serialize")
                .len(),
        )
    }

    /// Fold later declarations without moving existing names. Used only by
    /// compatibility adapters that cannot render inline tool additions.
    pub fn fold(&self, additions: impl IntoIterator<Item = impl AsRef<[ToolDeclaration]>>) -> Self {
        let mut tools = self.to_vec();
        for addition in additions {
            for tool in addition.as_ref() {
                if let Some(old) = tools.iter_mut().find(|old| old.name == tool.name) {
                    *old = tool.clone();
                } else {
                    tools.push(tool.clone());
                }
            }
        }
        Self(tools.into())
    }
}

impl Deref for ToolDeclarations {
    type Target = [ToolDeclaration];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AsRef<[ToolDeclaration]> for ToolDeclarations {
    fn as_ref(&self) -> &[ToolDeclaration] {
        self
    }
}
impl Serialize for ToolDeclarations {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_ref().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ToolDeclarations {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        Self::new(Vec::<ToolDeclaration>::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_names_schemas_and_preserves_wire_order() {
        for input in [
            r#"[{"name":" ","description":"","parameters":{}}]"#,
            r#"[{"name":"t","description":"","parameters":null}]"#,
            r#"[{"name":"t","description":"","parameters":{}},{"name":"t","description":"","parameters":{}}]"#,
            r#"[{"name":"t","description":"","parameters":{},"annotations":{}}]"#,
        ] {
            assert!(
                serde_json::from_str::<ToolDeclarations>(input).is_err(),
                "{input}"
            );
        }
        let json = r#"[{"name":"z","description":"last first","parameters":{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"number"}}}},{"name":"a","description":"","parameters":{}}]"#;
        let tools: ToolDeclarations = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&tools).unwrap(), json);
        assert_eq!(tools.clone(), tools);
    }
}
