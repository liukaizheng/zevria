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
            let definition = if options.strict_tools && tool.strict.unwrap_or(true) {
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
            } else if tool.strict == Some(false) {
                value["strict"] = serde_json::Value::Bool(false);
            }
            Ok(value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_opt_out_keeps_optional_and_map_schemas_untouched() {
        let parameters = json!({"type":"object","properties":{"opts":{"type":"object","additionalProperties":true},"tag":{"type":"string"}},"required":["tag"]});
        let mcp = ToolDeclaration {
            name: "mcp__fixture__search".into(),
            description: "fixture".into(),
            parameters: parameters.clone(),
            strict: Some(false),
        };
        let built_in = ToolDeclaration {
            name: "native".into(),
            strict: None,
            ..mcp.clone()
        };
        let rendered = render(&[built_in, mcp.clone()], ToolRendererOptions::default()).unwrap();
        assert_eq!(rendered[0]["strict"], true);
        assert_eq!(rendered[1]["strict"], false);
        assert_eq!(rendered[1]["parameters"], parameters);
        let compatibility = render(
            &[mcp],
            ToolRendererOptions {
                strict_tools: false,
            },
        )
        .unwrap();
        assert!(compatibility[0].get("strict").is_none());
        assert_eq!(compatibility[0]["parameters"], parameters);
    }
}
