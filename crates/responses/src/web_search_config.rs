//! Stable Responses hosted-search controls. The guide (not the older SDK
//! filter type) documents blocked_domains and return_token_budget as well.

use rig_core::providers::openai::responses_api::ResponsesToolDefinition;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub external_web_access: bool,
    pub search_context_size: SearchContextSize,
    pub return_token_budget: SearchReturnTokenBudget,
    pub filters: Option<WebSearchFilters>,
    pub user_location: Option<WebSearchLocation>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            external_web_access: true,
            search_context_size: SearchContextSize::Medium,
            return_token_budget: SearchReturnTokenBudget::Default,
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
    pub fn validate(&self, provider: &str) -> anyhow::Result<()> {
        let prefix = format!("providers.{provider}.web_search");
        if let Some(filters) = &self.filters {
            for (field, domains) in [
                ("allowed_domains", &filters.allowed_domains),
                ("blocked_domains", &filters.blocked_domains),
            ] {
                if let Some(domains) = domains {
                    anyhow::ensure!(
                        domains.len() <= 100,
                        "{prefix}.filters.{field} permits at most 100 domains"
                    );
                    for (index, domain) in domains.iter().enumerate() {
                        // A DNS name, not a URL, wildcard, IP, port, or path.
                        let valid = domain.len() <= 253
                            && domain.contains('.')
                            && domain.parse::<std::net::IpAddr>().is_err()
                            && domain.split('.').all(|label| {
                                !label.is_empty()
                                    && label.len() <= 63
                                    && !label.starts_with('-')
                                    && !label.ends_with('-')
                                    && label
                                        .bytes()
                                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                            });
                        anyhow::ensure!(
                            valid,
                            "{prefix}.filters.{field}[{index}] must be a domain name without a scheme, port, wildcard, or path"
                        );
                    }
                }
            }
        }
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
                        "{prefix}.user_location.{field} must be nonblank and contain no control characters"
                    );
                }
            }
            if let Some(country) = &location.country {
                anyhow::ensure!(
                    country.len() == 2 && country.bytes().all(|b| b.is_ascii_uppercase()),
                    "{prefix}.user_location.country must be a two-letter uppercase ISO country code"
                );
            }
        }
        Ok(())
    }

    pub fn tool(&self) -> anyhow::Result<ResponsesToolDefinition> {
        let mut tool = ResponsesToolDefinition::web_search()
            .with_config("external_web_access", self.external_web_access.into())
            .with_config(
                "search_context_size",
                serde_json::to_value(self.search_context_size)?,
            )
            .with_config(
                "return_token_budget",
                serde_json::to_value(self.return_token_budget)?,
            );
        if let Some(filters) = &self.filters {
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
        assert!(!default.enabled);
        assert_eq!(
            serde_json::to_value(default.tool().unwrap().with_strict()).unwrap(),
            json!({"type":"web_search", "external_web_access":true, "search_context_size":"medium", "return_token_budget":"default"})
        );
        let config: WebSearchConfig = serde_json::from_value(json!({"enabled":true,"external_web_access":false,"search_context_size":"high","return_token_budget":"unlimited","filters":{"allowed_domains":["openai.com"],"blocked_domains":["example.org"]},"user_location":{"country":"GB","city":"London","region":"London","timezone":"Europe/London"}})).unwrap();
        config.validate("test").unwrap();
        let wire = serde_json::to_value(config.tool().unwrap()).unwrap();
        assert_eq!(wire["user_location"]["type"], "approximate");
        assert_eq!(wire["filters"]["blocked_domains"], json!(["example.org"]));
        for invalid in [
            json!({"search_context_size":"huge"}),
            json!({"return_token_budget":null}),
            json!({"return_token_budget":12}),
            json!({"return_token_budget":"large"}),
            json!({"unknown":true}),
            json!({"filters":{"unknown":[]}}),
            json!({"user_location":{"type":"exact"}}),
        ] {
            assert!(serde_json::from_value::<WebSearchConfig>(invalid).is_err());
        }
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
        ] {
            let config: WebSearchConfig =
                serde_json::from_value(json!({"filters":{"allowed_domains":[domain]}})).unwrap();
            assert!(
                config
                    .validate("test")
                    .unwrap_err()
                    .to_string()
                    .contains("providers.test.web_search.filters.allowed_domains[0]")
            );
        }
        for field in ["allowed_domains", "blocked_domains"] {
            for size in [100, 101] {
                let config: WebSearchConfig =
                    serde_json::from_value(json!({"filters":{field:vec!["example.com";size]}}))
                        .unwrap();
                assert_eq!(config.validate("test").is_ok(), size == 100);
            }
        }
    }
}
