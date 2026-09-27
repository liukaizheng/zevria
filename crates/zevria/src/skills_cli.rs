//! Offline skills commands run before application/provider configuration.
use crate::skills::{LocalSkillService, load_skill_config};
use anyhow::Context as _;
use std::path::Path;
use zevria_instructions::skill::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillsCli {
    List { json: bool },
    Inspect { name: SkillName, json: bool },
    Validate { path: String, json: bool },
    SetEnabled { name: SkillName, enabled: bool },
}

impl SkillsCli {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let usage = "usage: zevria skills list [--json] | inspect <name> [--json] | validate <path> [--json] | enable <name> | disable <name>";
        let mut json = false;
        let mut positional = Vec::new();
        for arg in args {
            if arg == "--json" && !json {
                json = true;
            } else {
                anyhow::ensure!(
                    !arg.starts_with('-'),
                    "unknown skills flag {arg:?}; {usage}"
                );
                positional.push(arg.as_str());
            }
        }
        match positional.as_slice() {
            ["list"] => Ok(Self::List { json }),
            ["inspect", name] => Ok(Self::Inspect {
                name: name.parse()?,
                json,
            }),
            ["validate", path] => Ok(Self::Validate {
                path: (*path).into(),
                json,
            }),
            ["enable", name] if !json => Ok(Self::SetEnabled {
                name: name.parse()?,
                enabled: true,
            }),
            ["disable", name] if !json => Ok(Self::SetEnabled {
                name: name.parse()?,
                enabled: false,
            }),
            _ => anyhow::bail!(usage),
        }
    }

    pub fn run(self, workspace: &Path) -> anyhow::Result<()> {
        let roots = FixedSkillRoots::capture(workspace);
        if let Self::Validate { path, json } = self {
            let path = workspace.join(path);
            let outcome = validate_skill_path(&roots, &path);
            match outcome {
                Ok((definition, diagnostics)) => {
                    let issues = !diagnostics.is_empty();
                    let registry = std::sync::Arc::new(SkillCatalog::new([definition])?);
                    let context = SkillContext {
                        catalog: registry,
                        pins: ActiveSkills::default(),
                        mode_enabled: true,
                    };
                    let page = context.management_view(&SkillManagementRequest::List {
                        query: String::new(),
                    })?;
                    let diagnostic_count = diagnostics.len();
                    let diagnostics: Vec<_> = diagnostics
                        .iter()
                        .map(|d| d.message.chars().take(1024).collect::<String>())
                        .collect();
                    if json {
                        let report = serde_json::to_string(&serde_json::json!({
                            "valid": !issues, "entries": page.entries,
                            "diagnostics": diagnostics,
                            "omitted_diagnostics": diagnostic_count - diagnostics.len(),
                        }))?;
                        println!("{report}");
                    } else {
                        print_view(&page, false)?;
                        let omitted = diagnostic_count - diagnostics.len();
                        for diagnostic in diagnostics {
                            println!("! {}", terminal_text(&diagnostic));
                        }
                        if omitted > 0 {
                            println!("{omitted} additional diagnostics omitted");
                        }
                    }
                    anyhow::ensure!(!issues, "skill validation reported diagnostics");
                    return Ok(());
                }
                Err(error) => {
                    let error = format!("{error:#}").chars().take(2048).collect::<String>();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(
                                &serde_json::json!({"valid": false, "diagnostics": [error]})
                            )?
                        );
                    }
                    anyhow::bail!("skill validation failed: {error}");
                }
            }
        }
        let config_path = zevria_foundation::config::config_path()?;
        let settings = load_skill_config(&config_path)?.settings;
        let service = LocalSkillService::new(roots, config_path);
        let catalog = service.catalog(settings, false)?;
        let (query, json, inspect) = match self {
            Self::List { json } => (String::new(), json, false),
            Self::Inspect { name, json } => (name.to_string(), json, true),
            Self::SetEnabled { name, enabled } => {
                let updated = service.update_sync(
                    SkillManagementRequest::SetEnabled {
                        expected_revision: catalog.revision().into(),
                        name: name.clone(),
                        enabled,
                    },
                    &catalog,
                )?;
                println!(
                    "{} {name}; catalog {}. Running sessions change only after explicit reload.",
                    if enabled { "Enabled" } else { "Disabled" },
                    updated.revision()
                );
                return Ok(());
            }
            Self::Validate { .. } => unreachable!(),
        };
        let context = SkillContext {
            catalog,
            pins: ActiveSkills::default(),
            mode_enabled: true,
        };
        let request = if inspect {
            SkillManagementRequest::Inspect {
                name: query.parse()?,
            }
        } else {
            SkillManagementRequest::List {
                query: query.clone(),
            }
        };
        let view = context
            .management_view(&request)
            .context("cannot project skill catalog")?;
        let found = !view.entries.is_empty();
        print_view(&view, json)?;
        anyhow::ensure!(!inspect || found, "no skill candidate matches {query:?}");
        Ok(())
    }
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' {
                ch.escape_default().to_string()
            } else {
                ch.to_string()
            }
        })
        .collect()
}

fn print_view(page: &SkillManagementView, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(page)?);
        return Ok(());
    }
    println!(
        "Global: {}\nProject: {}\nRevision: {}",
        terminal_text(page.global_location.as_deref().unwrap_or("unavailable")),
        terminal_text(page.project_location.as_deref().unwrap_or("unavailable")),
        page.revision
    );
    for entry in &page.entries {
        println!(
            "{} [{}{}] {:?}: {}\n  {}\n  policy={:?} resources={}",
            entry.name,
            entry.status,
            if entry.enabled { "" } else { ", disabled" },
            entry.scope,
            entry
                .manifest
                .as_ref()
                .map(|p| p.as_str())
                .unwrap_or("body-only"),
            terminal_text(&entry.metadata.description),
            entry.metadata.invocation_policy,
            entry.resources
        );
        if entry.metadata.interface != SkillInterface::default() {
            println!("  Interface: {:?}", entry.metadata.interface);
        }
        if entry.metadata_shortened {
            println!("  Metadata shortened to per-field limits");
        }
        for declaration in &entry.metadata.dependencies {
            println!(
                "  Inert declaration: {} = {}",
                terminal_text(&declaration.kind),
                terminal_text(&declaration.value)
            );
        }
    }
    for invalid in &page.invalid_entries {
        println!(
            "[invalid] {:?}: {}\n  {}",
            invalid.scope,
            invalid.manifest.as_str(),
            terminal_text(&invalid.diagnostic)
        );
    }
    for diagnostic in &page.diagnostics {
        println!("! {}", terminal_text(diagnostic));
    }
    if page.omitted_diagnostics > 0 {
        println!("{} diagnostics omitted", page.omitted_diagnostics);
    }
    Ok(())
}
