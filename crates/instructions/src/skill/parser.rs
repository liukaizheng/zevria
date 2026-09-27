//! Private YAML adapter. Main instructions are eagerly parsed, complete, and
//! bounded. Sidecars can restrict invocation but never grant execution authority.

use serde_json::{Map, Value};

use super::{
    MAX_SKILL_BYTES, SkillDependency, SkillInvocationPolicy, SkillLayout, SkillMetadata, SkillName,
    SkillRelativePath,
};

pub(crate) struct ParsedSkill {
    pub name: SkillName,
    pub metadata: SkillMetadata,
    pub body: String,
    pub warnings: Vec<String>,
    pub invalid_policy: bool,
}

fn yaml(source: &str, limit: usize) -> anyhow::Result<Map<String, Value>> {
    anyhow::ensure!(
        source.len() <= limit,
        "YAML source exceeds its {limit}-byte cap"
    );
    // Keep the decoder's recursion/expansion defenses, tighten them for small
    // local metadata, and disallow YAML includes, tags, merges and aliases.
    let mut options = serde_saphyr::Options::default();
    let mut budget = serde_saphyr::Budget::default();
    budget.max_depth = 32;
    budget.flow_nesting_limit = 32;
    budget.max_events = 65_536;
    budget.max_nodes = 32_768;
    budget.max_total_scalar_bytes = limit;
    budget.max_documents = 1;
    budget.max_aliases = 0;
    budget.max_anchors = 0;
    budget.max_recorded_anchor_events = 0;
    budget.max_recorded_anchor_bytes = 0;
    options.budget = Some(budget);
    options.merge_keys = serde_saphyr::MergeKeyPolicy::Error;
    options.reject_unsupported_tags = true;
    options.emit_comments = false;
    options.with_snippet = false;
    let value: Value = serde_saphyr::from_str_with_options(source, options)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("skill YAML must be a mapping"))
}

fn split_frontmatter(source: &str) -> anyhow::Result<(&str, &str)> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let rest = source
        .strip_prefix("---\r\n")
        .or_else(|| source.strip_prefix("---\n"))
        .ok_or_else(|| anyhow::anyhow!("the file must start with a `---` frontmatter fence"))?;
    let mut consumed = 0;
    for line in rest.split_inclusive('\n') {
        let start = consumed;
        consumed += line.len();
        if line.trim_end_matches(['\r', '\n']).trim() == "---" {
            return Ok((&rest[..start], &rest[consumed..]));
        }
    }
    anyhow::bail!("the frontmatter has no closing `---` fence")
}

pub(crate) fn parse_document(
    source: &str,
    derived_name: &str,
    layout: SkillLayout,
) -> anyhow::Result<ParsedSkill> {
    anyhow::ensure!(
        source.len() as u64 <= MAX_SKILL_BYTES,
        "main skill source exceeds its 64 KiB cap"
    );
    let (frontmatter, body) = split_frontmatter(source)?;
    let mut warnings = Vec::new();
    let fields = yaml(frontmatter, MAX_SKILL_BYTES as usize)?;
    let name = match fields.get("name") {
        Some(Value::String(value)) => SkillName::parse(value)?,
        Some(_) => anyhow::bail!("frontmatter name must be a string"),
        None => SkillName::parse(derived_name)?,
    };
    if layout == SkillLayout::Flat {
        anyhow::ensure!(
            name.as_str() == derived_name,
            "frontmatter name does not match the flat filename"
        );
    }
    let description = fields
        .get("description")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("frontmatter must set a non-empty string `description`"))?;
    let body = body.trim();
    anyhow::ensure!(!body.is_empty(), "skill has no instruction body");
    let mut metadata = SkillMetadata::new(description);
    let invalid_policy = merge_fields(&mut metadata, &fields, &mut warnings);
    metadata.validate()?;
    Ok(ParsedSkill {
        name,
        metadata,
        body: body.to_string(),
        warnings,
        invalid_policy,
    })
}

pub(crate) fn merge_sidecar(
    metadata: &mut SkillMetadata,
    source: &str,
    warnings: &mut Vec<String>,
) -> bool {
    match yaml(source, 32 * 1024) {
        Ok(fields) => merge_fields(metadata, &fields, warnings),
        Err(error) => {
            metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
            warnings.push(format!(
                "malformed policy-bearing sidecar: {error}; fresh activation is explicit-only"
            ));
            true
        }
    }
}

fn cosmetic_string(
    fields: &Map<String, Value>,
    key: &str,
    target: &mut Option<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(value) = fields.get(key) {
        if let Some(value) = value.as_str().filter(|value| {
            !value.trim().is_empty() && value.len() <= 4096 && !value.contains('\0')
        }) {
            *target = Some(value.to_string());
        } else {
            warnings.push(format!("ignored invalid cosmetic field {key:?}"));
        }
    }
}

fn merge_fields(
    metadata: &mut SkillMetadata,
    fields: &Map<String, Value>,
    warnings: &mut Vec<String>,
) -> bool {
    let mut invalid_policy = false;
    if let Some(nested) = fields.get("metadata") {
        if let Some(nested) = nested.as_object() {
            cosmetic_string(
                nested,
                "short-description",
                &mut metadata.short_description,
                warnings,
            );
        } else {
            warnings.push("ignored invalid cosmetic metadata group".into());
        }
    }
    if let Some(interface) = fields.get("interface") {
        if let Some(interface) = interface.as_object() {
            cosmetic_string(
                interface,
                "display_name",
                &mut metadata.interface.display_name,
                warnings,
            );
            cosmetic_string(
                interface,
                "short_description",
                &mut metadata.short_description,
                warnings,
            );
            cosmetic_string(
                interface,
                "default_prompt",
                &mut metadata.interface.default_prompt,
                warnings,
            );
            if let Some(color) = interface.get("brand_color") {
                if let Some(color) = color.as_str().filter(|value| {
                    value.len() == 7
                        && value.starts_with('#')
                        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
                }) {
                    metadata.interface.brand_color = Some(color.to_string());
                } else {
                    warnings.push("ignored invalid cosmetic brand_color".into());
                }
            }
            for (key, target) in [
                ("icon_small", &mut metadata.interface.icon_small),
                ("icon_large", &mut metadata.interface.icon_large),
            ] {
                if let Some(value) = interface.get(key) {
                    match value
                        .as_str()
                        .and_then(|value| SkillRelativePath::new(value).ok())
                    {
                        Some(path) => *target = Some(path),
                        None => {
                            warnings.push(format!("ignored invalid package-relative asset {key:?}"))
                        }
                    }
                }
            }
        } else {
            warnings.push("ignored invalid cosmetic interface group".into());
        }
    }
    if let Some(policy) = fields.get("policy") {
        match policy.as_object().and_then(|policy| {
            policy
                .get("allow_implicit_invocation")
                .map(Some)
                .or(Some(None))
        }) {
            Some(Some(Value::Bool(allowed))) => {
                metadata.invocation_policy = if *allowed {
                    SkillInvocationPolicy::ModelAllowed
                } else {
                    SkillInvocationPolicy::ExplicitOnly
                }
            }
            Some(None) if policy.as_object().is_some_and(Map::is_empty) => {}
            _ => {
                invalid_policy = true;
                metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
                warnings.push(
                    "invalid or unsupported invocation policy; fresh activation is explicit-only"
                        .into(),
                );
            }
        }
        if policy
            .as_object()
            .is_some_and(|policy| policy.keys().any(|key| key != "allow_implicit_invocation"))
        {
            invalid_policy = true;
            metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
            warnings.push("unsupported policy fields; fresh activation is explicit-only".into());
        }
    }
    if let Some(dependencies) = fields.get("dependencies") {
        if let Some(tools) = dependencies.get("tools").and_then(Value::as_array) {
            if tools.len() <= 64 {
                metadata.dependencies = tools
                    .iter()
                    .map(|tool| SkillDependency {
                        kind: tool
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unsupported")
                            .chars()
                            .take(128)
                            .collect(),
                        value: tool
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or("unresolved")
                            .chars()
                            .take(1024)
                            .collect(),
                    })
                    .collect();
            } else {
                warnings.push("too many inert dependency declarations; none resolved".into());
            }
        } else {
            warnings.push("unsupported dependency declarations; none resolved".into());
        }
    }
    for key in [
        "roots",
        "root",
        "skills.roots",
        "tools",
        "permissions",
        "environment",
        "execution",
        "product",
    ] {
        if fields.contains_key(key) {
            warnings.push(format!("unsupported declaration {key:?} is inert; packages cannot change roots or grant capabilities"));
        }
    }
    invalid_policy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_yaml_interoperates_without_a_partial_parser() {
        let parsed = parse_document("\u{feff}---\r\nname: explicit\r\ndescription: >\r\n  Build for AWS:\r\n  ECS and more\r\nmetadata:\r\n  short-description: 'Short # text' # comment\r\n---\r\nExact body\r\n", "package.v1", SkillLayout::Package).expect("YAML package");
        assert_eq!(parsed.name.as_str(), "explicit");
        assert_eq!(parsed.metadata.description, "Build for AWS: ECS and more\n");
        assert_eq!(
            parsed.metadata.short_description.as_deref(),
            Some("Short # text")
        );
        assert_eq!(parsed.body, "Exact body");
        assert!(
            parse_document(
                "---\nname: explicit\ndescription: Fine\n---\nBody",
                "different",
                SkillLayout::Flat
            )
            .is_err()
        );
        assert!(
            parse_document(
                "---\ndescription: Fine\n---\nBody",
                "package.v1",
                SkillLayout::Package
            )
            .is_err(),
            "complete basename is validated, not file_stem"
        );
        assert!(
            parse_document(
                "---\ndescription: Build for AWS: ECS\n---\nBody",
                "build",
                SkillLayout::Flat,
            )
            .is_err()
        );
        let quoted = parse_document(
            "---\ndescription: 'Build for AWS: ECS'\n---\nBody",
            "build",
            SkillLayout::Flat,
        )
        .unwrap();
        assert_eq!(quoted.metadata.description, "Build for AWS: ECS");
        assert!(quoted.warnings.is_empty());
    }

    #[test]
    fn skill_yaml_policy_is_fail_closed_and_merges_only_present_values() {
        let mut metadata = SkillMetadata::new("Description");
        let mut warnings = Vec::new();
        merge_sidecar(
            &mut metadata,
            "policy:\n  allow_implicit_invocation: false\ninterface:\n  display_name: Review",
            &mut warnings,
        );
        merge_sidecar(
            &mut metadata,
            "interface:\n  short_description: Short\n  icon_small: ../outside\n",
            &mut warnings,
        );
        assert_eq!(
            metadata.invocation_policy,
            SkillInvocationPolicy::ExplicitOnly
        );
        assert_eq!(metadata.interface.display_name.as_deref(), Some("Review"));
        assert!(metadata.interface.icon_small.is_none());
        merge_sidecar(
            &mut metadata,
            "policy:\n  allow_implicit_invocation: true",
            &mut warnings,
        );
        assert_eq!(
            metadata.invocation_policy,
            SkillInvocationPolicy::ModelAllowed
        );
        merge_sidecar(&mut metadata, "policy: [", &mut warnings);
        assert_eq!(
            metadata.invocation_policy,
            SkillInvocationPolicy::ExplicitOnly
        );
    }

    #[test]
    fn skill_yaml_rejects_duplicates_types_nesting_and_aliases() {
        for document in [
            "description: one\ndescription: two",
            "description: []",
            "description: 123",
            "description: &a hello\nmetadata: *a",
            "description: !include /etc/passwd",
        ] {
            assert!(
                parse_document(
                    &format!("---\n{document}\n---\nBody"),
                    "test",
                    SkillLayout::Flat
                )
                .is_err(),
                "{document}"
            );
        }
        let nested = format!(
            "description: Fine\nunknown: {}0{}",
            "[".repeat(1000),
            "]".repeat(1000)
        );
        assert!(
            parse_document(
                &format!("---\n{nested}\n---\nBody"),
                "test",
                SkillLayout::Flat
            )
            .is_err()
        );
    }
}
