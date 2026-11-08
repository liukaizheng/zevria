//! One function-tool renderer for top-level and inline declarations.
use rig_core::providers::openai::responses_api::ResponsesToolDefinition;
use zevria_model::ToolDeclaration;

#[derive(Debug, Clone, Copy)]
pub struct ToolRendererOptions {
    pub strict_tools: bool,
}
impl Default for ToolRendererOptions {
    fn default() -> Self {
        Self { strict_tools: true }
    }
}

pub fn render(
    tools: &[ToolDeclaration],
    options: ToolRendererOptions,
) -> anyhow::Result<Vec<serde_json::Value>> {
    tools
        .iter()
        .map(|tool| {
            tool.validate()?;
            let definition = ResponsesToolDefinition::function(
                &tool.name,
                &tool.description,
                tool.parameters.clone(),
            );
            let definition = if options.strict_tools {
                definition.with_strict()
            } else {
                definition
            };
            let mut value = serde_json::to_value(definition)?;
            if !options.strict_tools {
                value
                    .as_object_mut()
                    .expect("function tool")
                    .remove("strict");
            }
            Ok(value)
        })
        .collect()
}
