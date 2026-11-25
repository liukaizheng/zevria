//! Responses hosted-search wire controls. Session activation and model
//! selection are owned by the host, not by the provider's tool renderer.

use rig_core::providers::openai::responses_api::ResponsesToolDefinition;
use serde::{Deserialize, Serialize};
use zevria_foundation::{WebSearchDepth, web_search::validate_search_domains};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub external_web_access: bool,
    pub search_context_size: SearchContextSize,
    pub return_token_budget: SearchReturnTokenBudget,
    pub max_tool_calls: Option<u32>,
    pub filters: Option<WebSearchFilters>,
    pub user_location: Option<WebSearchLocation>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            external_web_access: true,
            search_context_size: SearchContextSize::Medium,
            return_token_budget: SearchReturnTokenBudget::Default,
            max_tool_calls: None,
            filters: None,
            user_location: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchReturnTokenBudget {
    Default,
    Unlimited,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl WebSearchConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_tool_calls != Some(0),
            "web_search.max_tool_calls must be positive"
        );
        self.filters_for_call(None)?;
        if let Some(location) = &self.user_location {
            for (field, value) in [
                ("country", &location.country),
                ("city", &location.city),
                ("region", &location.region),
                ("timezone", &location.timezone),
            ] {
                if let Some(value) = value {
                    anyhow::ensure!(
                        !value.trim().is_empty() && !value.chars().any(char::is_control),
                        "web_search.user_location.{field} must be nonblank and contain no control characters"
                    );
                }
            }
            if let Some(country) = &location.country {
                anyhow::ensure!(
                    country.len() == 2 && country.bytes().all(|b| b.is_ascii_uppercase()),
                    "web_search.user_location.country must be a two-letter uppercase ISO country code"
                );
            }
        }
        Ok(())
    }

    pub fn resolve_for_depth(&self, depth: WebSearchDepth) -> (SearchContextSize, Option<u32>) {
        match depth {
            WebSearchDepth::Quick => (
                SearchContextSize::Low,
                self.max_tool_calls.map(|cap| cap.min(3)),
            ),
            WebSearchDepth::Standard => (self.search_context_size, self.max_tool_calls),
            WebSearchDepth::Deep => (SearchContextSize::High, self.max_tool_calls),
        }
    }

    /// Return only the intersection of configured and requested DNS trees.
    /// Keep blocked descendants of an allowed parent on the wire; removing just
    /// the explicitly blocked entries from the allowlist cannot express that.
    fn filters_for_call(&self, requested: Option<&[String]>) -> anyhow::Result<WebSearchFilters> {
        fn normalized(domains: &[String], field: &str) -> anyhow::Result<Vec<String>> {
            validate_search_domains(domains, field)?;
            let mut domains: Vec<_> = domains
                .iter()
                .map(|domain| domain.to_ascii_lowercase())
                .collect();
            domains.sort();
            domains.dedup();
            Ok(domains)
        }
        fn within(domain: &str, parent: &str) -> bool {
            domain == parent
                || domain
                    .strip_suffix(parent)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        }

        let mut filters = self.filters.clone().unwrap_or_default();
        let configured = filters
            .allowed_domains
            .as_deref()
            .map(|domains| normalized(domains, "web_search.filters.allowed_domains"))
            .transpose()?;
        filters.blocked_domains = filters
            .blocked_domains
            .as_deref()
            .map(|domains| normalized(domains, "web_search.filters.blocked_domains"))
            .transpose()?
            .filter(|domains| !domains.is_empty());
        let requested = requested
            .map(|domains| normalized(domains, "allowed_domains"))
            .transpose()?;
        filters.allowed_domains = match (configured, requested) {
            (Some(configured), Some(requested)) => {
                let mut intersection = Vec::new();
                for domain in &requested {
                    for allowed in &configured {
                        if within(domain, allowed) {
                            intersection.push(domain.clone());
                        } else if within(allowed, domain) {
                            intersection.push(allowed.clone());
                        }
                    }
                }
                Some(intersection)
            }
            (domains @ Some(_), None) | (None, domains) => domains,
        };
        if let Some(allowed) = &mut filters.allowed_domains {
            if let Some(blocked) = &filters.blocked_domains {
                allowed.retain(|domain| !blocked.iter().any(|block| within(domain, block)));
            }
            allowed.sort();
            allowed.dedup();
            // Never translate an empty intersection to an unrestricted request.
            anyhow::ensure!(
                !allowed.is_empty(),
                "web_search domain filters leave no allowed domains"
            );
            validate_search_domains(allowed, "effective allowed_domains")?;
        }
        Ok(filters)
    }

    pub fn tool_for_call(
        &self,
        context_size: SearchContextSize,
        requested_allowed_domains: Option<&[String]>,
    ) -> anyhow::Result<ResponsesToolDefinition> {
        let mut tool = ResponsesToolDefinition::web_search()
            .with_config("external_web_access", self.external_web_access.into())
            .with_config("search_context_size", serde_json::to_value(context_size)?)
            .with_config(
                "return_token_budget",
                serde_json::to_value(self.return_token_budget)?,
            );
        let filters = self.filters_for_call(requested_allowed_domains)?;
        if filters != WebSearchFilters::default() {
            tool = tool.with_config("filters", serde_json::to_value(filters)?);
        }
        if let Some(location) = &self.user_location {
            let mut location = serde_json::to_value(location)?;
            location["type"] = "approximate".into();
            tool = tool.with_config("user_location", location);
        }
        Ok(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_controls_and_strictness() {
        let default: WebSearchConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            serde_json::to_value(
                default
                    .tool_for_call(SearchContextSize::Medium, None)
                    .unwrap()
                    .with_strict()
            )
            .unwrap(),
            json!({"type":"web_search", "external_web_access":true, "search_context_size":"medium", "return_token_budget":"default"})
        );
        let config: WebSearchConfig = serde_json::from_value(json!({"external_web_access":false,"search_context_size":"high","return_token_budget":"unlimited","filters":{"allowed_domains":["openai.com"],"blocked_domains":["example.org"]},"user_location":{"country":"GB","city":"London","region":"London","timezone":"Europe/London"}})).unwrap();
        config.validate().unwrap();
        let wire =
            serde_json::to_value(config.tool_for_call(SearchContextSize::High, None).unwrap())
                .unwrap();
        assert_eq!(wire["user_location"]["type"], "approximate");
        assert_eq!(wire["filters"]["blocked_domains"], json!(["example.org"]));
        for invalid in [
            json!({"search_context_size":"huge"}),
            json!({"return_token_budget":null}),
            json!({"return_token_budget":12}),
            json!({"return_token_budget":"large"}),
            json!({"enabled":true}),
            json!({"filters":{"unknown":[]}}),
            json!({"user_location":{"type":"exact"}}),
        ] {
            assert!(serde_json::from_value::<WebSearchConfig>(invalid).is_err());
        }
    }

    #[test]
    fn depth_varies_context_and_clamps_only_an_explicit_step_budget() {
        let options = WebSearchConfig::default();
        assert_eq!(
            options.resolve_for_depth(WebSearchDepth::Quick),
            (SearchContextSize::Low, None)
        );
        assert_eq!(
            options.resolve_for_depth(WebSearchDepth::Standard),
            (SearchContextSize::Medium, None)
        );
        assert_eq!(
            options.resolve_for_depth(WebSearchDepth::Deep),
            (SearchContextSize::High, None)
        );
        for cap in [1, 2, 3, 8] {
            let options = WebSearchConfig {
                max_tool_calls: Some(cap),
                search_context_size: SearchContextSize::Low,
                ..options.clone()
            };
            assert_eq!(
                options.resolve_for_depth(WebSearchDepth::Quick),
                (SearchContextSize::Low, Some(cap.min(3)))
            );
            assert_eq!(
                options.resolve_for_depth(WebSearchDepth::Standard),
                (SearchContextSize::Low, Some(cap))
            );
            assert_eq!(
                options.resolve_for_depth(WebSearchDepth::Deep),
                (SearchContextSize::High, Some(cap))
            );
        }
        assert!(
            WebSearchConfig {
                max_tool_calls: Some(0),
                ..options
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn per_call_filters_only_narrow_and_fail_closed() {
        let options: WebSearchConfig = serde_json::from_value(json!({"filters":{
            "allowed_domains":["example.com", "docs.example.org"],
            "blocked_domains":["private.example.com", "old.docs.example.org"]
        }}))
        .unwrap();
        let requested = [
            "NEWS.example.com",
            "example.org",
            "private.example.com",
            "badexample.com",
            "news.example.com",
        ]
        .map(str::to_owned);
        let wire = serde_json::to_value(
            options
                .tool_for_call(SearchContextSize::Low, Some(&requested))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            wire["filters"]["allowed_domains"],
            json!(["docs.example.org", "news.example.com"])
        );
        assert_eq!(
            wire["filters"]["blocked_domains"],
            json!(["old.docs.example.org", "private.example.com"])
        );
        for domain in [
            "x.private.example.com",
            "badexample.com",
            "other.net",
            "https://example.com",
        ] {
            assert!(
                options
                    .tool_for_call(SearchContextSize::Low, Some(&[domain.into()]))
                    .is_err()
            );
        }
        assert!(
            options
                .tool_for_call(SearchContextSize::Low, Some(&[]))
                .is_err()
        );
    }

    #[test]
    fn qualified_domain_validation_and_limits() {
        for domain in [
            "",
            " ",
            "https://example.com",
            "example.com/path",
            "a..com",
            "-a.com",
            "a-.com",
            "*.example.com",
            "example.com:80",
            "127.0.0.1",
            "127.1",
            "0x7f.0x1",
        ] {
            let config: WebSearchConfig =
                serde_json::from_value(json!({"filters":{"allowed_domains":[domain]}})).unwrap();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("web_search.filters.allowed_domains[0]")
            );
        }
        for field in ["allowed_domains", "blocked_domains"] {
            for size in [100, 101] {
                let config: WebSearchConfig =
                    serde_json::from_value(json!({"filters":{field:vec!["example.com";size]}}))
                        .unwrap();
                assert_eq!(config.validate().is_ok(), size == 100);
            }
        }
    }
}
