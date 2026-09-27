//! Shared regression check used by instruction and tool-description tests.
use std::collections::BTreeMap;

pub fn module_docs() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "engine-protocol",
            include_str!("../../../docs/instructions/engine-protocol.md"),
        ),
        (
            "application",
            include_str!("../../../docs/instructions/system-prompt.md"),
        ),
        (
            "command-conventions",
            include_str!("../../../docs/instructions/command-conventions.md"),
        ),
        (
            "hosted-search",
            include_str!("../../../docs/instructions/hosted-search.md"),
        ),
        (
            "inspection-policy",
            include_str!("../../../docs/instructions/inspection-policy.md"),
        ),
        (
            "skill-selection",
            include_str!("../../../docs/instructions/skill-selection.md"),
        ),
        (
            "build-mode",
            include_str!("../../../docs/instructions/build-mode.md"),
        ),
        (
            "plan-mode",
            include_str!("../../../docs/instructions/plan-mode.md"),
        ),
        (
            "explore-agent",
            include_str!("../../../docs/instructions/explore-agent.md"),
        ),
        (
            "build-subtask",
            include_str!("../../../docs/instructions/build-subtask.md"),
        ),
        (
            "ensemble-worker",
            include_str!("../../../docs/instructions/ensemble-worker.md"),
        ),
        (
            "ensemble-worker-plan",
            include_str!("../../../docs/instructions/ensemble-worker-plan.md"),
        ),
        (
            "ensemble-worker-review",
            include_str!("../../../docs/instructions/ensemble-worker-review.md"),
        ),
        (
            "ensemble-plan-synthesis",
            include_str!("../../../docs/instructions/ensemble-plan-synthesis.md"),
        ),
        (
            "ensemble-review-synthesis",
            include_str!("../../../docs/instructions/ensemble-review-synthesis.md"),
        ),
        (
            "maintenance-mode",
            include_str!("../../../docs/instructions/maintenance-mode.md"),
        ),
    ]
}

pub fn assert_non_overlapping<'a>(modules: impl IntoIterator<Item = (&'a str, &'a str)>) {
    let mut owners = BTreeMap::new();
    for (name, text) in modules {
        // Check both sentence punctuation across soft wraps and individual
        // Markdown lines (bullets/headings can otherwise join adjacent prose).
        let prose = text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        for sentence in prose
            .split(['.', '!', '?'])
            .chain(prose.split(['.', '!', '?', '\n']))
        {
            let words = sentence
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect::<Vec<_>>();
            if words.len() < 8 {
                continue;
            }
            let sentence = words.join(" ");
            if let Some(previous) = owners.insert(sentence.clone(), name) {
                assert_eq!(
                    previous, name,
                    "duplicate sentence in {previous} and {name}: {sentence}"
                );
            }
        }
    }
}
