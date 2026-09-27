//! Historical per-session pins, their single reducer, and owning transcript applications.
use super::{SkillName, SkillSnapshot};
use rig_core::message::{Message, UserContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::Error as _};
use std::collections::{BTreeMap, BTreeSet};
use zevria_foundation::SKILL_TOOL_NAME;

// The first-use variant deliberately owns the full validated snapshot; reapply
// carries only a name. Keep this persisted transition shape explicit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillApplication {
    Activate(SkillSnapshot),
    Reapply(SkillName),
}
impl SkillApplication {
    pub fn name(&self) -> &SkillName {
        match self {
            Self::Activate(snapshot) => snapshot.name(),
            Self::Reapply(name) => name,
        }
    }
    pub fn acknowledgement(&self, request: &SkillRequest) -> String {
        let status = match self {
            Self::Activate(_) => "activated",
            Self::Reapply(_) => "already_active",
        };
        let application = if request.arguments().is_empty() {
            "the current request"
        } else {
            request.arguments()
        };
        format!(
            "status: {status}\nskill: {}\napplication: {application}",
            self.name()
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInvocationOrigin {
    Explicit,
    Model,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillToolApplication {
    pub call_id: String,
    pub application: SkillApplication,
}

/// Contains all historical pins, including disabled skills. Only projection filters them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveSkills {
    by_name: BTreeMap<SkillName, SkillSnapshot>,
}
impl ActiveSkills {
    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = SkillSnapshot>,
    ) -> anyhow::Result<Self> {
        let mut active = Self::default();
        for snapshot in snapshots {
            active.apply(&SkillApplication::Activate(snapshot))?;
        }
        Ok(active)
    }
    pub fn apply(&mut self, application: &SkillApplication) -> anyhow::Result<()> {
        match application {
            SkillApplication::Activate(snapshot) => {
                snapshot.validate()?;
                anyhow::ensure!(
                    !self.contains(snapshot.name()),
                    "skill {} is already pinned",
                    snapshot.name()
                );
                self.by_name
                    .insert(snapshot.name().clone(), snapshot.clone());
            }
            SkillApplication::Reapply(name) => anyhow::ensure!(
                self.contains(name),
                "skill {name} cannot be reapplied without a historical pin"
            ),
        }
        Ok(())
    }
    pub fn prepare(&self, snapshot: SkillSnapshot) -> anyhow::Result<(SkillApplication, Self)> {
        // Resolution must choose the pin first. Refuse accidental replacement, not just duplicate activation.
        let application = if let Some(pin) = self.get(snapshot.name()) {
            anyhow::ensure!(
                pin == &snapshot,
                "skill {} retains a different pinned snapshot",
                snapshot.name()
            );
            SkillApplication::Reapply(snapshot.name().clone())
        } else {
            SkillApplication::Activate(snapshot)
        };
        let prospective = self.with_application(&application)?;
        Ok((application, prospective))
    }
    pub fn with_application(&self, application: &SkillApplication) -> anyhow::Result<Self> {
        let mut prospective = self.clone();
        prospective.apply(application)?;
        Ok(prospective)
    }
    pub fn contains(&self, name: &SkillName) -> bool {
        self.by_name.contains_key(name)
    }
    pub fn get(&self, name: &SkillName) -> Option<&SkillSnapshot> {
        self.by_name.get(name)
    }
    pub fn snapshots(&self) -> impl Iterator<Item = &SkillSnapshot> {
        self.by_name.values()
    }
    pub fn len(&self) -> usize {
        self.by_name.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
    pub fn render_context(&self) -> Option<String> {
        (!self.is_empty()).then(|| {
            self.snapshots()
                .map(|s| crate::DirectiveContent::skill(s).text)
                .collect::<Vec<_>>()
                .join("\n\n")
        })
    }
}

/// Validate the batch locally and reduce accepted applications in actual result order.
/// Provider correlation remains part of ordinary messages and metadata, not this sidecar.
pub fn apply_tool_applications(
    active: &mut ActiveSkills,
    message: &Message,
    metadata: &[crate::ToolResultMetadata],
    applications: &[SkillToolApplication],
) -> anyhow::Result<()> {
    zevria_foundation::tool_result::validate_tool_result_message(message)?;
    let mut by_id = BTreeMap::new();
    for accepted in applications {
        anyhow::ensure!(
            !accepted.call_id.is_empty()
                && by_id.insert(accepted.call_id.as_str(), accepted).is_none(),
            "duplicate or empty skill application id {}",
            accepted.call_id
        );
    }
    let mut meta = BTreeMap::new();
    for entry in metadata {
        anyhow::ensure!(
            meta.insert(entry.id.as_str(), entry).is_none(),
            "duplicate tool result metadata id {}",
            entry.id
        );
    }
    let mut seen = BTreeSet::new();
    if let Message::User { content } = message {
        for block in content {
            let UserContent::ToolResult(result) = block else {
                continue;
            };
            let id = result.call.as_str();
            anyhow::ensure!(seen.insert(id), "duplicate tool result id {id}");
            let entry = meta.remove(id);
            if result.name != SKILL_TOOL_NAME {
                anyhow::ensure!(
                    entry.is_none_or(|e| e.tool_name != SKILL_TOOL_NAME),
                    "skill metadata has a non-skill result"
                );
                anyhow::ensure!(
                    !by_id.contains_key(id),
                    "skill application has a non-skill result"
                );
                continue;
            }
            let entry =
                entry.ok_or_else(|| anyhow::anyhow!("skill result {id:?} has no metadata"))?;
            anyhow::ensure!(
                entry.tool_name == SKILL_TOOL_NAME,
                "skill result metadata name mismatch"
            );
            let accepted = by_id.remove(id);
            if entry.outcome.is_success() {
                let accepted = accepted.ok_or_else(|| {
                    anyhow::anyhow!(
                        "successful skill result {id:?} is missing its typed application"
                    )
                })?;
                active.apply(&accepted.application)?;
            } else {
                anyhow::ensure!(
                    accepted.is_none(),
                    "failed, denied or cancelled skill call carries an accepted application"
                );
            }
        }
    }
    anyhow::ensure!(
        by_id.is_empty(),
        "skill application has no correlated successful result"
    );
    anyhow::ensure!(
        meta.values()
            .all(|entry| entry.tool_name != SKILL_TOOL_NAME),
        "skill metadata has no correlated result"
    );
    Ok(())
}

/// The message cache is derived, never serialized or accepted from clients.
#[derive(Debug, Clone)]
pub struct SkillInvocation {
    name: SkillName,
    arguments: crate::UserPrompt,
    application: SkillApplication,
    model_message: Message,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvocationFields {
    name: SkillName,
    arguments: crate::UserPrompt,
    application: SkillApplication,
}
impl PartialEq for SkillInvocation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.arguments == other.arguments
            && self.application == other.application
    }
}
impl Eq for SkillInvocation {}
impl Serialize for SkillInvocation {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct BorrowedFields<'a> {
            name: &'a SkillName,
            arguments: &'a crate::UserPrompt,
            application: &'a SkillApplication,
        }
        BorrowedFields {
            name: &self.name,
            arguments: &self.arguments,
            application: &self.application,
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for SkillInvocation {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = InvocationFields::deserialize(d)?;
        if fields.arguments != fields.arguments.clone().trimmed() {
            return Err(D::Error::custom(
                "skill invocation arguments must use their trimmed canonical form",
            ));
        }
        if &fields.name != fields.application.name() {
            return Err(D::Error::custom(
                "skill invocation application name mismatch",
            ));
        }
        Ok(Self::new(fields.name, fields.arguments, fields.application))
    }
}
impl SkillInvocation {
    pub fn new(
        name: SkillName,
        arguments: impl Into<crate::UserPrompt>,
        application: SkillApplication,
    ) -> Self {
        let arguments = arguments.into().trimmed();
        let prefix = if arguments.is_blank() {
            format!(
                "Apply the active skill {:?} to this request.",
                name.as_str()
            )
        } else {
            format!(
                "Apply the active skill {:?} to this request:\n\n",
                name.as_str()
            )
        };
        let model_message = arguments.with_prefix(prefix).to_message();
        Self {
            name,
            arguments,
            application,
            model_message,
        }
    }
    pub fn name(&self) -> &SkillName {
        &self.name
    }
    pub fn arguments(&self) -> &crate::UserPrompt {
        &self.arguments
    }
    pub fn application(&self) -> &SkillApplication {
        &self.application
    }
    pub fn model_message(&self) -> &Message {
        &self.model_message
    }
    pub fn model_text(&self) -> String {
        let args = self.arguments.text_projection();
        if args.is_empty() {
            format!(
                "Apply the active skill {:?} to this request.",
                self.name.as_str()
            )
        } else {
            format!(
                "Apply the active skill {:?} to this request:\n\n{args}",
                self.name.as_str()
            )
        }
    }
    pub fn display(&self) -> String {
        skill_invocation_display(self.name.as_str(), &self.arguments.display_projection())
    }
    pub fn display_message(&self) -> Message {
        self.arguments
            .with_prefix(format!("${} ", self.name))
            .trimmed()
            .to_message()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillRequest {
    /// Exact validated skill name.
    pub skill: SkillName,
    /// Optional application-specific arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}
impl SkillRequest {
    pub fn arguments(&self) -> &str {
        self.args.as_deref().unwrap_or("").trim()
    }
}
pub fn skill_invocation_display(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("${name}")
    } else {
        format!("${name} {args}")
    }
}
