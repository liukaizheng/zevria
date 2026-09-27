//! The complete request-owned instruction set, independent of conversation history.
use crate::{DirectivePolicy, directive::digest, prompts::*, skill::SkillPromptCatalog};
use zevria_foundation::{LAUNCH_SUBTASKS_TOOL_NAME, ModelRole, TurnPolicy, WorkspaceContract};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSet {
    pub application: String,
    pub system: Vec<(String, String)>,
    pub workflow: DirectivePolicy,
    pub catalog: Option<SkillPromptCatalog>,
}

const CAPABILITY_MODULES: [(&str, &str); 3] = [
    ("Command conventions", COMMAND_CONVENTIONS_INSTRUCTIONS),
    ("Hosted search", HOSTED_SEARCH_INSTRUCTIONS),
    (
        "Inspection and scratch policy",
        INSPECTION_POLICY_INSTRUCTIONS,
    ),
];

pub(crate) fn capability_sections(policy: &DirectivePolicy) -> Vec<(&'static str, &'static str)> {
    CAPABILITY_MODULES
        .into_iter()
        .zip([
            policy.allows_tool("command"),
            policy.allows_tool(crate::WEB_SEARCH_TOOL_NAME),
            policy.contract == WorkspaceContract::SourceReadOnlyScratch,
        ])
        .filter_map(|(section, enabled)| enabled.then_some(section))
        .collect()
}

impl InstructionSet {
    pub fn render(&self) -> String {
        let mut sections = vec![ENGINE_PROTOCOL_INSTRUCTIONS.to_string()];
        if !self.application.is_empty() {
            sections.push(format!("## Application guidance\n{}", self.application));
        }
        // Component identity, not arrival order, defines the stable prefix.
        let mut system = self
            .system
            .iter()
            .filter(|(_, text)| !text.is_empty())
            .collect::<Vec<_>>();
        system.sort_by(|(left, _), (right, _)| left.cmp(right));
        if !system.is_empty() {
            sections.push(format!(
                "## File guidance\n{}",
                system
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            ));
        }
        sections.push(format!(
            "## Workflow policy: {}\n{}\n{}",
            self.workflow.scope,
            self.workflow.declaration(),
            self.workflow.instructions
        ));
        for (title, body) in capability_sections(&self.workflow) {
            sections.push(format!("## {title}\n{body}"));
        }
        sections.push(format!(
            "## Eligible skills\n{}",
            self.catalog.as_ref().map_or_else(
                || SKILL_SELECTION_UNAVAILABLE.into(),
                SkillPromptCatalog::render
            )
        ));
        sections.join("\n\n")
    }

    pub fn identity(&self) -> String {
        digest(&self.render())
    }

    pub fn maintenance<'a>(
        application: &str,
        system: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        Self {
            application: application.into(),
            system: system
                .into_iter()
                .map(|(key, text)| (key.into(), text.into()))
                .collect(),
            workflow: DirectivePolicy::new(
                "maintenance",
                &TurnPolicy::new(
                    MAINTENANCE_INSTRUCTIONS,
                    Some(Vec::new()),
                    ModelRole::Build,
                    false,
                ),
            ),
            catalog: None,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let policy = &self.workflow;
        anyhow::ensure!(!policy.scope.trim().is_empty(), "empty policy scope");
        if let Some(names) = &policy.allowed_tool_names {
            let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
            anyhow::ensure!(
                unique.len() == names.len() && names.iter().all(|name| !name.trim().is_empty()),
                "invalid policy tool restrictions"
            );
        }
        anyhow::ensure!(
            !policy.orchestration || policy.allows_tool(LAUNCH_SUBTASKS_TOOL_NAME),
            "orchestration eligibility requires launch_subtasks"
        );
        anyhow::ensure!(
            policy.contract != WorkspaceContract::SourceReadOnlyScratch
                || !["edit", "write", "delete"]
                    .into_iter()
                    .any(|name| policy.allows_tool(name)),
            "source-read-only inspection cannot permit mutation tools"
        );
        if let Some(workspace) = &policy.workspace {
            anyhow::ensure!(
                !workspace.root.trim().is_empty() && !workspace.startup.trim().is_empty(),
                "empty workspace binding path"
            );
        }
        for text in std::iter::once(&self.application)
            .chain(std::iter::once(&policy.instructions))
            .chain(self.system.iter().map(|(_, text)| text))
        {
            for (title, body) in CAPABILITY_MODULES {
                anyhow::ensure!(
                    !text.contains(body.trim()),
                    "embedded capability module: {title}"
                );
            }
        }
        if let Some(catalog) = &self.catalog {
            catalog.validate()?;
        }
        Ok(())
    }
}
