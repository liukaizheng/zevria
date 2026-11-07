//! Skill-only configuration and fixed-root catalog composition. This loader
//! does not require provider routing, create a first-run skeleton, or resolve
//! roots from the selected configuration file's location.

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use zevria_instructions::skill::SkillsConfig;

#[derive(Debug, Clone)]
pub struct SkillConfigSource {
    pub path: PathBuf,
    pub contents: Option<String>,
    pub settings: SkillsConfig,
}

pub fn load_skill_config(path: &Path) -> anyhow::Result<SkillConfigSource> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read skill configuration at {}", path.display()));
        }
    };
    let settings = contents
        .as_deref()
        .map(parse_skill_config)
        .transpose()?
        .unwrap_or_default();
    Ok(SkillConfigSource {
        path: path.to_path_buf(),
        contents,
        settings,
    })
}

pub(crate) fn parse_skill_config(contents: &str) -> anyhow::Result<SkillsConfig> {
    // Parse unrelated settings as data only, not provider/credential models.
    // Match the full application's top-level schema, while allowing those
    // unrelated tables to remain intentionally incomplete for offline use.
    let document: toml::Table = toml::from_str(contents)?;
    for key in document.keys() {
        anyhow::ensure!(
            key != "providers",
            "configuration key {key:?} moved to models.jsonc next to this file"
        );
        anyhow::ensure!(
            matches!(
                key.as_str(),
                "session" | "skills" | "theme" | "acp" | "ensemble" | "command" | "log" | "modes"
            ),
            "unsupported configuration key {key:?}"
        );
    }
    let settings: SkillsConfig = document
        .get("skills")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()?
        .unwrap_or_default();
    settings.validate()?;
    Ok(settings)
}

/// Captured composition-root state. Neither config reload nor a client request
/// can replace HOME, the startup workspace, provider settings, or Plan policy.
#[derive(Clone)]
pub struct LocalSkillService {
    pub(crate) roots: zevria_instructions::skill::FixedSkillRoots,
    pub(crate) config_path: PathBuf,
}

impl LocalSkillService {
    pub fn new(roots: zevria_instructions::FixedSkillRoots, config_path: PathBuf) -> Self {
        Self { roots, config_path }
    }

    pub fn catalog(
        &self,
        settings: SkillsConfig,
        strict: bool,
    ) -> anyhow::Result<std::sync::Arc<zevria_instructions::skill::SkillCatalog>> {
        let discovery = zevria_instructions::skill::discover_skills(&self.roots);
        anyhow::ensure!(
            !strict || discovery.incomplete_scopes.is_empty(),
            "skill discovery is incomplete; the installed catalog was retained"
        );
        Ok(std::sync::Arc::new(
            zevria_instructions::skill::SkillCatalog::from_discovery(discovery, settings)?,
        ))
    }

    pub fn update_sync(
        &self,
        request: zevria_instructions::skill::SkillManagementRequest,
        installed: &zevria_instructions::skill::SkillCatalog,
    ) -> anyhow::Result<std::sync::Arc<zevria_instructions::skill::SkillCatalog>> {
        use zevria_instructions::skill::SkillManagementRequest;
        anyhow::ensure!(
            request.expected_revision() == Some(installed.revision()),
            "stale catalog revision"
        );
        match request {
            SkillManagementRequest::Reload { .. } => {
                self.catalog(load_skill_config(&self.config_path)?.settings, true)
            }
            SkillManagementRequest::SetEnabled { name, enabled, .. } => write_skill_config(
                &self.config_path,
                installed.config(),
                &name,
                enabled,
                |settings| self.catalog(settings, true),
            ),
            _ => anyhow::bail!("not a skill mutation"),
        }
    }
}

impl zevria_instructions::skill::SkillManagementService for LocalSkillService {
    fn update<'a>(
        &'a self,
        request: zevria_instructions::skill::SkillManagementRequest,
        installed: std::sync::Arc<zevria_instructions::skill::SkillCatalog>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<
                        std::sync::Arc<zevria_instructions::skill::SkillCatalog>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let service = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || service.update_sync(request, &installed))
                .await
                .context("skill management worker failed")?
        })
    }
}

/// The writer is shared by all clients. `prepare` is fallible and completes
/// before commit; after commit the caller only installs its returned value.
fn write_skill_config<T>(
    path: &Path,
    expected: &SkillsConfig,
    name: &zevria_instructions::skill::SkillName,
    enabled: bool,
    prepare: impl FnOnce(SkillsConfig) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    crate::settings::transaction(path, |before| {
        anyhow::ensure!(
            &parse_skill_config(before)? == expected,
            "skill configuration changed externally; reload before writing"
        );
        let after = set_name_enabled(before, name, enabled)?;
        let prepared = prepare(parse_skill_config(&after)?)?;
        Ok((after, prepared))
    })
}

fn set_name_enabled(
    contents: &str,
    name: &zevria_instructions::skill::SkillName,
    enabled: bool,
) -> anyhow::Result<String> {
    let settings = parse_skill_config(contents)?;
    if settings
        .rules
        .iter()
        .rev()
        .find(|rule| &rule.name == name)
        .is_some_and(|rule| rule.enabled == enabled)
    {
        return Ok(contents.into());
    }
    let mut document = contents.parse::<toml_edit::DocumentMut>()?;
    if document.get("skills").is_none() {
        document["skills"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if document["skills"].get("rules").is_none() {
        document["skills"]["rules"] = toml_edit::value(toml_edit::Array::new());
    }
    let rules = &mut document["skills"]["rules"];
    if let Some(index) = settings.rules.iter().rposition(|rule| &rule.name == name) {
        let value = if let Some(tables) = rules.as_array_of_tables_mut() {
            tables
                .get_mut(index)
                .and_then(|table| table.get_mut("enabled"))
                .and_then(toml_edit::Item::as_value_mut)
        } else {
            rules
                .as_array_mut()
                .and_then(|array| array.get_mut(index))
                .and_then(toml_edit::Value::as_inline_table_mut)
                .and_then(|table| table.get_mut("enabled"))
        }
        .context("cannot locate the existing skill rule")?;
        let decor = value.decor().clone();
        *value = toml_edit::Value::from(enabled);
        *value.decor_mut() = decor;
    } else if let Some(rules) = rules.as_array_of_tables_mut() {
        let mut rule = toml_edit::Table::new();
        rule["name"] = toml_edit::value(name.as_str());
        rule["enabled"] = toml_edit::value(enabled);
        rules.push(rule);
    } else if let Some(rules) = rules.as_array_mut() {
        let mut rule = toml_edit::InlineTable::new();
        rule.insert("name", name.as_str().into());
        rule.insert("enabled", enabled.into());
        rules.push(rule);
    } else {
        anyhow::bail!("skills.rules must be an array");
    }
    let after = document.to_string();
    parse_skill_config(&after)?;
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn skill_only_config_needs_no_providers_and_never_creates_files() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("elsewhere/config.toml");
        let source = load_skill_config(&path).expect("missing config defaults");
        assert_eq!(source.path, path);
        assert!(source.contents.is_none());
        assert_eq!(source.settings, SkillsConfig::default());
        assert!(!path.exists());
        let settings = parse_skill_config(
            "[skills]\nenabled = false\n[ensemble.agents.unconfigured.env]\napi_key = 'preserved'",
        )
        .expect("no routing required");
        assert!(!settings.enabled);
    }

    #[test]
    fn skill_writer_preserves_comments_secrets_rule_order_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let name = zevria_instructions::skill::SkillName::parse("review").unwrap();
        for source in [
            "# top\n[ensemble.agents.private.env]\napi_key = 'SECRET' # keep secret comment\n[skills]\nrules = [{name='review', enabled=false}, {name='review', enabled=true}] # keep array\n",
            "# top\n[ensemble.agents.private.env]\napi_key = 'SECRET' # keep secret comment\n[[skills.rules]]\nname = 'review'\nenabled = true # keep bool\n",
            "# top\nskills = {rules = [{name='review', enabled=true}]} # keep inline\n[ensemble.agents.private.env]\napi_key = 'SECRET' # keep secret comment\n",
        ] {
            std::fs::write(&path, source).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let permissions = std::fs::metadata(&path).unwrap().permissions();
            let settings = parse_skill_config(source).unwrap();
            let prepared = write_skill_config(&path, &settings, &name, false, Ok).unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            assert!(!prepared.name_enabled(&name));
            assert!(after.contains("api_key = 'SECRET' # keep secret comment"));
            for line in source.lines().filter(|line| line.contains("# keep")) {
                assert!(after.contains(line.split('#').next_back().unwrap()));
            }
            assert_eq!(std::fs::metadata(&path).unwrap().permissions(), permissions);
            assert_eq!(
                parse_skill_config(&after).unwrap().rules.len(),
                settings.rules.len()
            );
            write_skill_config(&path, &prepared, &name, false, Ok).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                after,
                "unchanged setting is byte-for-byte no-op"
            );
        }
    }

    #[test]
    fn theme_only_changes_leave_skill_settings_stable_and_skill_writes_preserve_selection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let before = "[skills]\nenabled = true\n";
        let selected = "[theme]\nname = 'not-installed' # shared theme\n";
        let settings = parse_skill_config(before).unwrap();
        let themed = format!("{before}{selected}");
        assert_eq!(settings, parse_skill_config(&themed).unwrap());
        std::fs::write(&path, themed).unwrap();
        let name = zevria_instructions::skill::SkillName::parse("review").unwrap();
        write_skill_config(&path, &settings, &name, false, Ok).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains(selected));
    }

    #[test]
    fn skill_writer_detects_external_edits_and_preparation_failures_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let name = zevria_instructions::skill::SkillName::parse("review").unwrap();
        std::fs::write(&path, "# before\n").unwrap();
        let result: anyhow::Result<()> =
            write_skill_config(&path, &SkillsConfig::default(), &name, false, |_| {
                anyhow::bail!("fatal discovery")
            });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# before\n");
        let result =
            write_skill_config(&path, &SkillsConfig::default(), &name, false, |settings| {
                std::fs::write(&path, "# concurrent editor\n").unwrap();
                Ok(settings)
            });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("changed while preparing")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# concurrent editor\n"
        );
        std::fs::write(&path, "[skills]\nenabled=false\n").unwrap();
        assert!(
            write_skill_config(&path, &SkillsConfig::default(), &name, false, Ok)
                .unwrap_err()
                .to_string()
                .contains("externally")
        );
    }

    #[test]
    fn skill_writer_does_not_create_configs_or_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let name = zevria_instructions::skill::SkillName::parse("review").unwrap();
        assert!(write_skill_config(&path, &SkillsConfig::default(), &name, false, Ok).is_err());
        assert!(!path.exists());
        #[cfg(unix)]
        {
            let outside = dir.path().join("outside");
            std::fs::write(&outside, "# do not alter\n").unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            assert!(write_skill_config(&path, &SkillsConfig::default(), &name, false, Ok).is_err());
            assert_eq!(
                std::fs::read_to_string(&outside).unwrap(),
                "# do not alter\n"
            );
        }
    }

    #[test]
    fn skill_config_root_overrides_are_rejected_by_both_loaders() {
        for field in [
            "roots = []",
            "extra_roots = []",
            "global_path = '/tmp'",
            "[skills.roots]\nproject = '/tmp'",
            "catalog_max_tokens = 0",
        ] {
            let source = format!("[skills]\n{field}");
            assert!(parse_skill_config(&source).is_err(), "{source}");
            assert!(Config::parse(&source, "{}").is_err(), "{source}");
        }
    }
}
