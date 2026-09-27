//! Leading command/skill classification and registry matches.
//!
//! Built-in commands are data: one [`SlashCommand`] variant plus one
//! [`COMMANDS`] entry per command, so the menu, completion, and parsing all
//! stay in sync by construction. A [`CommandRegistry`] holds the built-ins
//! alongside the session's skill metadata; the two live in disjoint sigil
//! namespaces — `/name` runs a built-in, `$name` invokes a skill — so no
//! name collision is possible. The menu's visibility and its filter are both
//! *derived* from the input box and caret (a leading `/` or `$` plus the
//! whitespace-free text before the caret) on every use. The composer stores
//! the highlighted logical row; `ViewState` owns the shared render-only
//! viewport that reveals that row in short popups.

use zevria_instructions::SkillMeta;
use zevria_workflow::EnsembleWorkflow;

use crate::completion::{CompletionKind, command_query as completion};

/// A built-in slash command, executed by the frontend rather than submitted
/// to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    /// Pane-local, typed review controls; absent from the root registry.
    Confirm,
    Unconfirm,
    Baseline,
    Unbaseline,
    Retry,
    CancelPrompt,
    Abandon,
    /// Change this session's role selection and the role's global default.
    Model,
    /// Change only this session's current role, saved for resume; config unchanged.
    ModelSession,
    /// Inspect and manage the two fixed skill locations.
    Skills,
    /// Pick a previous session of this workspace and resume it.
    Resume,
    /// Start a fresh session in the current workspace.
    New,
    /// Select ordinary local implementation mode.
    Build,
    /// Opt this Build prompt into explicit concurrent delegation.
    Orchestrate,
    /// Select read-only planning mode.
    Plan,
    /// Summarize the active model context into a durable checkpoint.
    Compact,
    /// Implement the last submitted Plan with the current conversation.
    Implement,
    /// Implement the last submitted Plan in a fresh session.
    ImplementFresh,
    /// Plan from independent ACP reports, then submit one canonical Plan.
    EnsemblePlan,
    /// Produce a read-only review from independent ACP reports.
    EnsembleReview,
}

/// One registry entry: the command plus its menu presentation.
#[derive(Debug)]
pub struct CommandSpec {
    pub command: SlashCommand,
    pub name: &'static str,
    pub description: &'static str,
}

/// Every built-in command, in menu order. Adding one is a new
/// [`SlashCommand`] variant, an entry here, and frontend execution handling.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: SlashCommand::Resume,
        name: "resume",
        description: "List previous sessions and resume one",
    },
    CommandSpec {
        command: SlashCommand::New,
        name: "new",
        description: "Start an empty session in this workspace",
    },
    CommandSpec {
        command: SlashCommand::Build,
        name: "build",
        description: "Select Build mode for local implementation",
    },
    CommandSpec {
        command: SlashCommand::Orchestrate,
        name: "orchestrate",
        description: "Orchestrate this Build prompt with concurrent subtasks",
    },
    CommandSpec {
        command: SlashCommand::Plan,
        name: "plan",
        description: "Select Plan mode for read-only planning",
    },
    CommandSpec {
        command: SlashCommand::Compact,
        name: "compact",
        description: "Summarize the conversation to free context.",
    },
    CommandSpec {
        command: SlashCommand::Implement,
        name: "implement",
        description: "Implement the last submitted plan in this session",
    },
    CommandSpec {
        command: SlashCommand::ImplementFresh,
        name: "implement-fresh",
        description: "Clear context and implement the last submitted plan",
    },
    CommandSpec {
        command: SlashCommand::EnsemblePlan,
        name: "ensemble-plan",
        description: "Plan with independent ACP agents",
    },
    CommandSpec {
        command: SlashCommand::EnsembleReview,
        name: "ensemble-review",
        description: "Review with independent ACP agents",
    },
    CommandSpec {
        command: SlashCommand::Skills,
        name: "skills",
        description: "Inspect, enable/disable, and reload local skills",
    },
    CommandSpec {
        command: SlashCommand::Model,
        name: "model",
        description: "Choose this mode's model and reasoning; save to session and config",
    },
    CommandSpec {
        command: SlashCommand::ModelSession,
        name: "model-session",
        description: "Choose this mode's model and reasoning for resume; config unchanged",
    },
];

pub(crate) const WORKER_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: SlashCommand::Confirm,
        name: "confirm",
        description: "Confirm this exact proposal revision",
    },
    CommandSpec {
        command: SlashCommand::Unconfirm,
        name: "unconfirm",
        description: "Withdraw this worker confirmation",
    },
    CommandSpec {
        command: SlashCommand::Baseline,
        name: "baseline",
        description: "Confirm this revision and use it as the synthesis baseline",
    },
    CommandSpec {
        command: SlashCommand::Unbaseline,
        name: "unbaseline",
        description: "Remove the baseline mark; keep confirmation",
    },
    CommandSpec {
        command: SlashCommand::Retry,
        name: "retry",
        description: "Retry this worker in the same ACP session",
    },
    CommandSpec {
        command: SlashCommand::CancelPrompt,
        name: "cancel",
        description: "Cancel only this worker's current attempt",
    },
    CommandSpec {
        command: SlashCommand::Abandon,
        name: "abandon",
        description: "Permanently exclude this worker's plan and captured answers",
    },
];

/// One validated submission, classified identically for fresh input and any
/// recalled transcript target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassifiedInput {
    Message(zevria_content::UserPrompt),
    Orchestrate(zevria_content::UserPrompt),
    Builtin(SlashCommand),
    Ensemble {
        workflow: EnsembleWorkflow,
        prompt: zevria_content::UserPrompt,
    },
    Skill {
        name: zevria_instructions::SkillName,
        args: zevria_content::UserPrompt,
    },
}

/// Local validation failure that must leave the composer unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassificationError {
    UnknownCommand(String),
    UnknownSkill(String),
    MissingEnsemblePrompt(EnsembleWorkflow),
    MissingOrchestrationPrompt,
    ImagesOnControl,
}

impl std::fmt::Display for ClassificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOrchestrationPrompt => {
                formatter.write_str("/orchestrate requires a prompt; the draft was retained.")
            }
            Self::ImagesOnControl => {
                formatter.write_str("Host controls do not accept images; the draft was retained.")
            }
            Self::UnknownCommand(input) => write!(formatter, "Unknown command: {input}"),
            Self::UnknownSkill(input) => write!(formatter, "Unknown skill: {input}"),
            Self::MissingEnsemblePrompt(workflow) => {
                write!(formatter, "{} requires a prompt.", workflow.slash_command())
            }
        }
    }
}

/// The namespace a leading sigil selects: `/` for built-in commands, `$` for
/// skills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sigil {
    Command,
    Skill,
}

/// One completion-menu row: a built-in or a skill.
#[derive(Clone, Copy, Debug)]
pub enum MatchEntry<'registry> {
    Builtin(&'static CommandSpec),
    Skill(&'registry SkillMeta),
}

impl MatchEntry<'_> {
    pub fn name(&self) -> &str {
        match self {
            Self::Builtin(spec) => spec.name,
            Self::Skill(meta) => meta.name.as_str(),
        }
    }

    /// The sigil that invokes this entry, for rendering and completion.
    pub fn sigil(&self) -> char {
        match self {
            Self::Builtin(_) => '/',
            Self::Skill(_) => '$',
        }
    }

    pub(crate) fn description(&self) -> &str {
        match self {
            Self::Builtin(spec) => spec.description,
            Self::Skill(meta) => &meta.description,
        }
    }
}

/// The built-ins plus one session's skills, each behind its own sigil.
#[derive(Debug, Default)]
pub struct CommandRegistry {
    skills: Vec<SkillMeta>,
    worker: bool,
}

impl CommandRegistry {
    /// Snapshot the session's skills. Built-ins and skills occupy disjoint
    /// sigil namespaces, so a skill may freely share a built-in's name.
    pub fn new(skills: Vec<SkillMeta>) -> Self {
        Self {
            skills,
            worker: false,
        }
    }

    pub fn worker() -> Self {
        Self {
            skills: Vec::new(),
            worker: true,
        }
    }
    fn commands(&self) -> &'static [CommandSpec] {
        if self.worker {
            WORKER_COMMANDS
        } else {
            COMMANDS
        }
    }

    /// Registry entries of the typed sigil's namespace whose name starts
    /// with the text between the sigil and caret, in declaration (built-in)
    /// or catalog (skill) order. Once whitespace begins before the caret,
    /// there is no active completion filter; text after the caret is ignored.
    pub fn matches(&self, input: &str, cursor: usize) -> Vec<MatchEntry<'_>> {
        let Some(completion) = completion(input, cursor) else {
            return Vec::new();
        };
        match completion.kind {
            CompletionKind::Command => self
                .commands()
                .iter()
                .filter(|spec| spec.name.starts_with(&completion.prefix))
                .map(MatchEntry::Builtin)
                .collect(),
            CompletionKind::Skill => self
                .skills
                .iter()
                .filter(|meta| meta.name.as_str().starts_with(&completion.prefix))
                .map(MatchEntry::Skill)
                .collect(),
            CompletionKind::File => Vec::new(),
        }
    }

    /// Classify a submitted input by its raw first character. Completion is
    /// deliberately separate: partial names remain validation errors until
    /// the user explicitly accepts a palette entry. A leading space forces a
    /// literal message and is removed by ordinary trimming.
    #[cfg(any(test, feature = "test-support"))]
    pub fn classify(&self, input: &str) -> Result<ClassifiedInput, ClassificationError> {
        self.classify_prompt(&zevria_content::UserPrompt::from_text(input))
    }

    pub fn classify_prompt(
        &self,
        prompt: &zevria_content::UserPrompt,
    ) -> Result<ClassifiedInput, ClassificationError> {
        let literal = || ClassifiedInput::Message(prompt.clone().trimmed());
        let Some(zevria_content::PromptBlock::Text(input)) = prompt.blocks().first() else {
            return Ok(literal());
        };
        let trimmed = prompt.display_projection().trim().to_string();
        if input.starts_with(' ') {
            return Ok(literal());
        }
        if self.worker && input.starts_with("//") {
            let mut blocks = prompt.blocks().to_vec();
            if let zevria_content::PromptBlock::Text(prefix) = &mut blocks[0] {
                prefix.remove(0);
            }
            return Ok(ClassifiedInput::Message(
                zevria_content::UserPrompt::new(blocks).expect("validated literal prompt"),
            ));
        }
        let Some((sigil, name)) = typed_name(input) else {
            return Ok(literal());
        };
        let mut blocks = prompt.blocks().to_vec();
        if let zevria_content::PromptBlock::Text(prefix) = &mut blocks[0] {
            *prefix = prefix[1 + name.len()..].to_string();
        }
        let args = zevria_content::UserPrompt::new(blocks)
            .expect("validated prompt")
            .trimmed();
        match sigil {
            Sigil::Command => {
                let Some(spec) = self.commands().iter().find(|spec| spec.name == name) else {
                    return Err(ClassificationError::UnknownCommand(trimmed));
                };
                if prompt.has_images()
                    && !matches!(
                        spec.command,
                        SlashCommand::EnsemblePlan
                            | SlashCommand::EnsembleReview
                            | SlashCommand::Orchestrate
                    )
                {
                    return Err(ClassificationError::ImagesOnControl);
                }
                match spec.command {
                    SlashCommand::Orchestrate if args.is_blank() => {
                        Err(ClassificationError::MissingOrchestrationPrompt)
                    }
                    SlashCommand::Orchestrate => Ok(ClassifiedInput::Orchestrate(args)),
                    SlashCommand::EnsemblePlan | SlashCommand::EnsembleReview
                        if args.is_blank() =>
                    {
                        let workflow = match spec.command {
                            SlashCommand::EnsemblePlan => EnsembleWorkflow::Plan,
                            SlashCommand::EnsembleReview => EnsembleWorkflow::Review,
                            _ => unreachable!("guarded ensemble command"),
                        };
                        Err(ClassificationError::MissingEnsemblePrompt(workflow))
                    }
                    SlashCommand::EnsemblePlan => Ok(ClassifiedInput::Ensemble {
                        workflow: EnsembleWorkflow::Plan,
                        prompt: args,
                    }),
                    SlashCommand::EnsembleReview => Ok(ClassifiedInput::Ensemble {
                        workflow: EnsembleWorkflow::Review,
                        prompt: args,
                    }),
                    command if args.is_blank() => Ok(ClassifiedInput::Builtin(command)),
                    _ => Err(ClassificationError::UnknownCommand(trimmed)),
                }
            }
            Sigil::Skill => {
                let Some(meta) = self.skills.iter().find(|meta| meta.name.as_str() == name) else {
                    return Err(ClassificationError::UnknownSkill(trimmed));
                };
                Ok(ClassifiedInput::Skill {
                    name: meta.name.clone(),
                    args,
                })
            }
        }
    }
}

/// The typed sigil and name: the leading `/` or `$` plus the text up to the
/// first whitespace. `None` when the input is not a sigil invocation at all.
fn typed_name(input: &str) -> Option<(Sigil, &str)> {
    let sigil = typed_sigil(input)?;
    let name = input[1..].split_whitespace().next().unwrap_or("");
    Some((sigil, name))
}

/// The namespace the input's leading character selects, if any.
fn typed_sigil(input: &str) -> Option<Sigil> {
    match input.as_bytes().first()? {
        b'/' => Some(Sigil::Command),
        b'$' => Some(Sigil::Skill),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_active(input: &str, cursor: usize) -> bool {
        completion(input, cursor).is_some()
    }

    fn meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.parse().unwrap(),
            description: description.to_string(),
        }
    }

    fn test_registry() -> CommandRegistry {
        CommandRegistry::new(vec![
            meta("commit", "Commit changes"),
            meta("review", "Review changes"),
        ])
    }

    #[test]
    fn matches_filter_each_sigil_to_its_own_namespace() {
        let registry = test_registry();
        assert_eq!(
            registry
                .matches("/", 1)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            [
                "resume",
                "new",
                "build",
                "orchestrate",
                "plan",
                "compact",
                "implement",
                "implement-fresh",
                "ensemble-plan",
                "ensemble-review",
                "skills",
                "model",
                "model-session",
            ]
        );
        assert_eq!(
            registry
                .matches("$", 1)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            ["commit", "review"]
        );
        // Each prefix filters only within its namespace.
        assert_eq!(
            registry
                .matches("$re", 3)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            ["review"]
        );
        assert!(registry.matches("/commit", "/commit".len()).is_empty());
        assert!(registry.matches("$resume", "$resume".len()).is_empty());
        assert!(registry.matches("$zzz", "$zzz".len()).is_empty());
        assert!(
            registry
                .matches("plain text", "plain text".len())
                .is_empty()
        );
        assert!(completion_active("/", 1));
        assert!(completion_active("$comm", "$comm".len()));
        assert!(!completion_active(
            "$comm with args",
            "$comm with args".len()
        ));
        assert!(!completion_active("/resume\n", "/resume\n".len()));
        assert!(
            registry
                .matches("$comm with args", "$comm with args".len())
                .is_empty()
        );
        assert!(registry.matches("/resume\n", "/resume\n".len()).is_empty());
    }

    #[test]
    fn completion_uses_only_text_before_the_caret() {
        let registry = test_registry();

        assert_eq!(
            registry
                .matches("/hello world", 1)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            [
                "resume",
                "new",
                "build",
                "orchestrate",
                "plan",
                "compact",
                "implement",
                "implement-fresh",
                "ensemble-plan",
                "ensemble-review",
                "skills",
                "model",
                "model-session",
            ]
        );
        assert_eq!(
            registry
                .matches("/cohello world", 3)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            ["compact"]
        );
        assert_eq!(
            registry
                .matches("$rethe auth module", 3)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            ["review"]
        );
        assert_eq!(completion("/cohello world", 3).unwrap().replacement(), 0..3);
        assert_eq!(completion("/e\u{301}cho", 2).unwrap().replacement(), 0..1);
        assert!(completion_active("/hello world", 1));
        assert!(!completion_active("/compact hello", "/compact ".len()));
    }

    #[test]
    fn classification_requires_exact_names_and_trims_arguments() {
        let registry = test_registry();
        assert_eq!(
            registry.classify("/resume"),
            Ok(ClassifiedInput::Builtin(SlashCommand::Resume))
        );
        for input in ["/new", "/new \n\t "] {
            assert_eq!(
                registry.classify(input),
                Ok(ClassifiedInput::Builtin(SlashCommand::New))
            );
        }
        assert_eq!(
            registry.classify("/compact   "),
            Ok(ClassifiedInput::Builtin(SlashCommand::Compact))
        );
        assert_eq!(
            registry.classify("$commit  ship it  "),
            Ok(ClassifiedInput::Skill {
                name: "commit".parse().unwrap(),
                args: "ship it".into(),
            })
        );
        assert_eq!(
            registry.classify("$commit"),
            Ok(ClassifiedInput::Skill {
                name: "commit".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
            })
        );
        assert_eq!(
            registry.classify(" plain text "),
            Ok(ClassifiedInput::Message("plain text".into()))
        );
    }

    #[test]
    fn classification_rejects_partial_and_unknown_namespaces() {
        let registry = test_registry();
        assert_eq!(registry.matches("/re", "/re".len()).len(), 1);
        for removed in ["/reasoning", "/reasoning-session", "/model-reasoning"] {
            assert!(registry.matches(removed, removed.len()).is_empty());
            assert_eq!(
                registry.classify(removed),
                Err(ClassificationError::UnknownCommand(removed.into()))
            );
        }
        assert_eq!(
            registry.classify("/re"),
            Err(ClassificationError::UnknownCommand("/re".to_string()))
        );
        assert_eq!(registry.matches("/imple", "/imple".len()).len(), 2);
        assert_eq!(
            registry.classify("/new later"),
            Err(ClassificationError::UnknownCommand(
                "/new later".to_string()
            ))
        );
        assert_eq!(
            registry.classify("/compact now"),
            Err(ClassificationError::UnknownCommand(
                "/compact now".to_string()
            ))
        );
        assert_eq!(
            registry.classify("/commit"),
            Err(ClassificationError::UnknownCommand("/commit".to_string()))
        );
        assert_eq!(
            registry.classify("$resume"),
            Err(ClassificationError::UnknownSkill("$resume".to_string()))
        );
        assert_eq!(
            registry.classify("$c focus on errors"),
            Err(ClassificationError::UnknownSkill(
                "$c focus on errors".to_string()
            ))
        );
    }

    #[test]
    fn classification_preserves_multiline_skill_and_ensemble_arguments() {
        let registry = test_registry();
        assert_eq!(
            registry.classify("$review \n first line\nsecond line \n"),
            Ok(ClassifiedInput::Skill {
                name: "review".parse().unwrap(),
                args: "first line\nsecond line".into(),
            })
        );
        assert_eq!(
            registry.classify("/ensemble-plan \n first line\nsecond line \n"),
            Ok(ClassifiedInput::Ensemble {
                workflow: EnsembleWorkflow::Plan,
                prompt: "first line\nsecond line".into(),
            })
        );
        assert_eq!(
            registry.classify("/ensemble-review   inspect src and  tests  "),
            Ok(ClassifiedInput::Ensemble {
                workflow: EnsembleWorkflow::Review,
                prompt: "inspect src and  tests".into(),
            })
        );
    }

    #[test]
    fn orchestration_is_a_prompt_modifier_with_ordered_images_and_literal_arguments() {
        use zevria_content::{PromptBlock, PromptImage, UserPrompt};
        let registry = test_registry();
        for bare in ["/orchestrate", "/orchestrate \n\t"] {
            assert_eq!(
                registry.classify(bare),
                Err(ClassificationError::MissingOrchestrationPrompt)
            );
        }
        assert_eq!(
            registry.classify("/orchestrate /plan $review literal"),
            Ok(ClassifiedInput::Orchestrate("/plan $review literal".into()))
        );
        assert_eq!(
            registry.classify(" /orchestrate literal"),
            Ok(ClassifiedInput::Message("/orchestrate literal".into()))
        );
        assert!(matches!(
            registry.classify("$/orchestrate x"),
            Err(ClassificationError::UnknownSkill(_))
        ));
        let image = PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
        for tail in [
            vec![PromptBlock::Image(image.clone())],
            vec![
                PromptBlock::Text("before ".into()),
                PromptBlock::Image(image.clone()),
                PromptBlock::Text(" after".into()),
            ],
        ] {
            let mut blocks = vec![PromptBlock::Text("/orchestrate ".into())];
            blocks.extend(tail.clone());
            let classified = registry
                .classify_prompt(&UserPrompt::new(blocks).unwrap())
                .unwrap();
            assert_eq!(
                classified,
                ClassifiedInput::Orchestrate(UserPrompt::new(tail).unwrap())
            );
        }
        let prompt = UserPrompt::new(vec![
            PromptBlock::Image(image),
            PromptBlock::Text("/orchestrate literal".into()),
        ])
        .unwrap();
        assert_eq!(
            registry.classify_prompt(&prompt),
            Ok(ClassifiedInput::Message(prompt))
        );
    }

    #[test]
    fn fixed_mode_commands_are_zero_argument_root_controls() {
        let registry = test_registry();
        let worker = CommandRegistry::worker();
        for (name, command) in [("build", SlashCommand::Build), ("plan", SlashCommand::Plan)] {
            let input = format!("/{name}");
            for exact in [input.clone(), format!("{input} \n\t")] {
                assert_eq!(
                    registry.classify(&exact),
                    Ok(ClassifiedInput::Builtin(command))
                );
            }
            assert_eq!(registry.matches(&input, input.len()).len(), 1);
            assert!(
                !registry.matches(&input, input.len())[0]
                    .description()
                    .is_empty()
            );
            for invalid in [format!("{input} extra"), format!("{input}\nextra")] {
                assert!(matches!(
                    registry.classify(&invalid),
                    Err(ClassificationError::UnknownCommand(_))
                ));
            }
            assert!(worker.matches(&input, input.len()).is_empty());
            assert!(matches!(
                worker.classify(&input),
                Err(ClassificationError::UnknownCommand(_))
            ));
            assert_eq!(
                registry.classify(&format!(" {input}")),
                Ok(ClassifiedInput::Message(input.into()))
            );
        }
        assert_eq!(registry.matches("/orch", 5)[0].name(), "orchestrate");
        assert!(matches!(
            registry.classify("/orch"),
            Err(ClassificationError::UnknownCommand(_))
        ));
    }

    #[test]
    fn classification_reports_missing_ensemble_prompts() {
        let registry = test_registry();
        assert_eq!(
            registry.classify("/ensemble-plan"),
            Err(ClassificationError::MissingEnsemblePrompt(
                EnsembleWorkflow::Plan
            ))
        );
        assert_eq!(
            registry.classify("/ensemble-review \n\t "),
            Err(ClassificationError::MissingEnsemblePrompt(
                EnsembleWorkflow::Review
            ))
        );
    }

    #[test]
    fn leading_space_escapes_all_sigil_classification() {
        let registry = test_registry();
        assert_eq!(
            registry.classify(" /ensemble-plan literal"),
            Ok(ClassifiedInput::Message("/ensemble-plan literal".into()))
        );
        assert_eq!(
            registry.classify(" $commit literal"),
            Ok(ClassifiedInput::Message("$commit literal".into()))
        );
        assert_eq!(
            registry.classify(" /unknown"),
            Ok(ClassifiedInput::Message("/unknown".into()))
        );
    }

    #[test]
    fn a_skill_may_share_a_builtin_name_across_namespaces() {
        let registry = CommandRegistry::new(vec![
            meta("resume", "A skill named like the built-in"),
            meta("commit", "Commit changes"),
        ]);
        assert_eq!(
            registry.classify("/resume"),
            Ok(ClassifiedInput::Builtin(SlashCommand::Resume))
        );
        assert_eq!(
            registry.classify("$resume"),
            Ok(ClassifiedInput::Skill {
                name: "resume".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
            })
        );
    }

    #[test]
    fn worker_registry_excludes_new_command() {
        assert_eq!(
            test_registry().classify("/abandon"),
            Err(ClassificationError::UnknownCommand("/abandon".into()))
        );
        let registry = CommandRegistry::worker();
        assert_eq!(
            registry.classify("/abandon"),
            Ok(ClassifiedInput::Builtin(SlashCommand::Abandon))
        );
        assert_eq!(
            registry
                .matches("/", 1)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            [
                "confirm",
                "unconfirm",
                "baseline",
                "unbaseline",
                "retry",
                "cancel",
                "abandon"
            ]
        );
        assert!(registry.matches("/new", "/new".len()).is_empty());
        assert_eq!(
            registry.classify("/new"),
            Err(ClassificationError::UnknownCommand("/new".to_string()))
        );
    }

    #[test]
    fn default_registry_serves_only_builtins() {
        let registry = CommandRegistry::default();
        assert_eq!(
            registry
                .matches("/", 1)
                .iter()
                .map(MatchEntry::name)
                .collect::<Vec<_>>(),
            [
                "resume",
                "new",
                "build",
                "orchestrate",
                "plan",
                "compact",
                "implement",
                "implement-fresh",
                "ensemble-plan",
                "ensemble-review",
                "skills",
                "model",
                "model-session",
            ]
        );
        assert!(registry.matches("$", 1).is_empty());
        assert_eq!(
            registry.classify("$commit"),
            Err(ClassificationError::UnknownSkill("$commit".to_string()))
        );
    }
}
