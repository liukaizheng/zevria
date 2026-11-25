//! Local adapter for external search. Only bounded answer text is model input.

use std::{collections::HashSet, sync::Arc};

use rig_agent::tool::{Tool, ToolContext};
use rig_core::{
    message::ToolResultContent,
    tool::{ToolExecutionError, ToolOutput},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zevria_content::citations::safe_http_url;
use zevria_foundation::{
    ToolCancelled, ToolResultDetail, WEB_SEARCH_TOOL_NAME, WebSearchDepth,
    web_search::validate_search_domains,
};
use zevria_session_api::{
    TurnContext, WebSearchExecutor, WebSearchOutput, WebSearchProgress, WebSearchRequest,
};

const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_SOURCE_LIST_BYTES: usize = 8 * 1024;
const TRUNCATED: &str = "\n\n[Web search output truncated.]";
const DESCRIPTION: &str = "Search the external web and return a concise answer with cited sources. Write a self-contained query and include relevant versions, API names, or errors in context; the search does not see this conversation. Use quick (the default) for targeted fact, API, or version checks, standard for normal documentation research, and deep only for synthesis across multiple sources. Batch independent queries in one assistant turn for concurrent execution. Prefer direct repository inspection for local codebase facts.";

#[derive(Clone)]
pub struct WebSearchTool {
    executor: Arc<dyn WebSearchExecutor>,
}

impl WebSearchTool {
    pub fn new(executor: Arc<dyn WebSearchExecutor>) -> Self {
        Self { executor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArgs {
    /// Nonblank search question or terms; may include specific web URLs to inspect.
    pub query: String,
    /// Defaults to quick. Standard is for normal research; deep is for multi-source synthesis.
    pub depth: Option<WebSearchDepth>,
    /// Brief context needed to make the query self-contained.
    pub context: Option<String>,
    /// DNS domains to narrow the search to, without schemes, paths, ports, or wildcards.
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebSearchError {
    #[error("{0}")]
    InvalidArgs(anyhow::Error),
    #[error("web search was cancelled")]
    Cancelled,
    #[error("web search failed: {0:#}")]
    Search(anyhow::Error),
}

impl Tool for WebSearchTool {
    const NAME: &'static str = WEB_SEARCH_TOOL_NAME;
    type Args = WebSearchArgs;
    type Error = WebSearchError;
    type Output = ToolOutput;

    fn description(&self) -> String {
        DESCRIPTION.into()
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(WebSearchArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            WebSearchError::InvalidArgs(_) => ToolExecutionError::invalid_args(error.to_string()),
            _ => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err(WebSearchError::InvalidArgs(anyhow::anyhow!(
                "query must be nonblank"
            )));
        }
        if let Some(domains) = &args.allowed_domains {
            if domains.is_empty() {
                return Err(WebSearchError::InvalidArgs(anyhow::anyhow!(
                    "allowed_domains must be nonempty when provided"
                )));
            }
            validate_search_domains(domains, "allowed_domains")
                .map_err(WebSearchError::InvalidArgs)?;
        }
        let request = WebSearchRequest {
            query: query.into(),
            context: args
                .context
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            depth: args.depth.unwrap_or_default(),
            allowed_domains: args.allowed_domains,
        };
        let cancellation = context
            .scope::<TurnContext>()
            .map_or_else(CancellationToken::new, |turn| {
                turn.cancellation().child_token()
            });
        let progress = context
            .scope::<WebSearchProgress>()
            .map(|progress| progress.as_ref().clone());
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(WebSearchError::Cancelled),
            result = self.executor.search(request, progress, cancellation.clone()) => {
                result.map_err(|error| if cancellation.is_cancelled() {
                    WebSearchError::Cancelled
                } else {
                    WebSearchError::Search(error)
                })
            }
        };
        if matches!(result, Err(WebSearchError::Cancelled)) {
            context
                .insert_result(ToolCancelled)
                .expect("cancellation markers serialize");
        }
        let output = result?;
        let text = format_output(&output);
        context
            .insert_result(ToolResultDetail::WebSearch(output.detail))
            .expect("search result detail serializes");
        Ok(ToolOutput::content(vec![ToolResultContent::text(text)]).expect("one text block"))
    }
}

fn format_output(output: &WebSearchOutput) -> String {
    let mut sources = String::new();
    let mut truncated = false;
    let mut seen = HashSet::new();
    for source in &output.detail.sources {
        let Some(url) = safe_http_url(&source.url) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let header = if sources.is_empty() {
            "\n\nSources:\n"
        } else {
            ""
        };
        let line = format!("- <{url}>\n");
        if sources.len() + header.len() + line.len() > MAX_SOURCE_LIST_BYTES {
            truncated = true;
            continue;
        }
        sources.push_str(header);
        sources.push_str(&line);
    }
    let answer = output.answer.trim();
    let mut limit = MAX_OUTPUT_BYTES - sources.len();
    truncated |= answer.len() > limit;
    if truncated {
        limit -= TRUNCATED.len();
    }
    let end = answer.floor_char_boundary(limit.min(answer.len()));
    let mut result = String::from(answer[..end].trim_end());
    result.push_str(sources.trim_end());
    if truncated {
        result.push_str(TRUNCATED);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::Pin};
    use zevria_foundation::{WebSearchResultDetail, WebSearchSourceSummary};

    struct CancelDuringPoll;

    impl WebSearchExecutor for CancelDuringPoll {
        fn search<'a>(
            &'a self,
            _request: WebSearchRequest,
            _progress: Option<WebSearchProgress>,
            cancellation: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<WebSearchOutput>> + Send + 'a>> {
            Box::pin(async move {
                cancellation.cancel();
                Err(anyhow::anyhow!("web search was cancelled"))
            })
        }
    }

    #[tokio::test]
    async fn executor_cancellation_winning_the_select_marks_the_tool_cancelled() {
        let tool = WebSearchTool::new(Arc::new(CancelDuringPoll));
        let mut context = ToolContext::new();
        let result = tool
            .call(
                &mut context,
                WebSearchArgs {
                    query: "Version check".into(),
                    depth: None,
                    context: None,
                    allowed_domains: None,
                },
            )
            .await;
        assert!(matches!(result, Err(WebSearchError::Cancelled)));
        assert!(context.result::<ToolCancelled>().unwrap().is_some());
    }

    #[test]
    fn output_is_utf8_bounded_with_deduplicated_sources() {
        let source = WebSearchSourceSummary {
            title: "Example".into(),
            url: "https://example.com/".into(),
        };
        let output = WebSearchOutput {
            answer: "界".repeat(MAX_OUTPUT_BYTES),
            detail: WebSearchResultDetail {
                provider: "provider".into(),
                model: "model".into(),
                depth: WebSearchDepth::Quick,
                actions: vec![],
                sources: vec![source.clone(), source],
                input_tokens: None,
                cached_tokens: None,
                output_tokens: None,
                total_tokens: None,
            },
        };
        let text = format_output(&output);
        assert!(text.len() <= MAX_OUTPUT_BYTES && text.starts_with('界'));
        assert_eq!(text.matches("https://example.com/").count(), 1);
        assert!(text.ends_with(TRUNCATED));
    }
}
