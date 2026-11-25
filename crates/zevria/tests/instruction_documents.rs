//! Embedded instruction documents stay separate from human-facing documentation.
use std::path::PathBuf;
use zevria_instructions::prompts;
use zevria_model::compaction::{SUMMARIZATION_PROMPT, SUMMARY_PREFIX, SUMMARY_REQUEST_PREFACE};

#[test]
fn search_guidance_is_a_capability_module_not_a_workflow_permission() {
    use zevria_foundation::{ModelRole, TurnPolicy, WEB_SEARCH_TOOL_NAME};
    use zevria_instructions::{DirectivePolicy, InstructionSet};

    for enabled in [false, true] {
        let names = if enabled {
            vec![WEB_SEARCH_TOOL_NAME.to_owned()]
        } else {
            vec![]
        };
        let policy = TurnPolicy::new("Research", Some(names), ModelRole::Build, false);
        let instructions = InstructionSet {
            application: String::new(),
            system: Vec::new(),
            workflow: DirectivePolicy::new("build", &policy),
            catalog: None,
        }
        .render();
        assert_eq!(
            instructions
                .matches(prompts::HOSTED_SEARCH_INSTRUCTIONS)
                .count(),
            usize::from(enabled)
        );
        assert_eq!(instructions.contains("## Web search"), enabled);
    }
}

#[test]
fn instruction_documents_match_production_constants() {
    let embedded = [
        prompts::ENGINE_PROTOCOL_INSTRUCTIONS,
        prompts::DEFAULT_PREAMBLE,
        prompts::COMMAND_CONVENTIONS_INSTRUCTIONS,
        prompts::HOSTED_SEARCH_INSTRUCTIONS,
        prompts::INSPECTION_POLICY_INSTRUCTIONS,
        prompts::SKILL_SELECTION_INSTRUCTIONS,
        prompts::BUILD_MODE_INSTRUCTIONS,
        prompts::PLAN_MODE_INSTRUCTIONS,
        prompts::EXPLORE_AGENT_INSTRUCTIONS,
        prompts::BUILD_SUBTASK_INSTRUCTIONS,
        prompts::ENSEMBLE_WORKER_PLAN_INSTRUCTIONS,
        prompts::ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS,
        prompts::ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS,
        prompts::ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS,
        SUMMARY_REQUEST_PREFACE,
        SUMMARIZATION_PROMPT,
        SUMMARY_PREFIX,
    ];
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("docs/instructions");
    let mut documents = Vec::new();
    // Use containment rather than equality: ENSEMBLE_WORKER_{PLAN,REVIEW}_INSTRUCTIONS
    // each concatenate two documents, and SUMMARY_PREFIX drops its trailing newline.
    for entry in
        std::fs::read_dir(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    {
        let entry = entry.unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let name = entry.file_name();
        if name == "README.md" || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let document = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let body = document.trim_end();
        assert!(
            !body.is_empty() && embedded.iter().any(|constant| constant.contains(body)),
            "{} is not an embedded instruction document; human docs belong in docs/",
            path.display()
        );
        documents.push(document);
    }
    for constant in embedded {
        assert!(
            documents
                .iter()
                .any(|document| constant.contains(document.trim_end())),
            "embedded constant contains no document from docs/instructions/: {}",
            constant.lines().next().unwrap_or_default()
        );
    }
}
