//! Stored working copies and pure, atomic Plan editing helpers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    MAX_PLAN_ARTIFACT_BYTES, PlanArtifact, PlanValidationError, PlanVersion, PlanWorkflowState,
};
use crate::TurnId;

pub const PLAN_DRAFT_CONTEXT_PREFIX: &str = "Engine-preserved Plan working copy";
pub const PLAN_ARTIFACT_CONTEXT_PREFIX: &str = "Engine-preserved submitted Plan";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDraft {
    pub target: PlanVersion,
    pub sequence: u32,
    pub markdown: String,
    pub source_turn_id: TurnId,
}

impl rig_core::tool::ContextValue for PlanDraft {
    const KEY: &'static str = "zevria.plan_draft";
}

#[derive(Debug, Clone, Copy)]
pub enum PlanWorkingCopy<'a> {
    Draft(&'a PlanDraft),
    Submitted(&'a PlanArtifact),
}

impl PlanWorkingCopy<'_> {
    pub fn markdown(&self) -> &str {
        match self {
            Self::Draft(draft) => &draft.markdown,
            Self::Submitted(artifact) => &artifact.markdown,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Draft(draft) => format!(
                "draft {} for revision {}",
                draft.sequence, draft.target.revision
            ),
            Self::Submitted(artifact) => {
                format!("submitted revision {}", artifact.version.revision)
            }
        }
    }
}

/// Read-only, per-call capabilities. Never serialized into provider context.
#[derive(Debug, Clone)]
pub struct PlanDraftScope {
    pub target: PlanVersion,
    pub next_sequence: u32,
    pub markdown: Option<String>,
    pub label: Option<String>,
    pub sequence: Option<u32>,
    pub turn_id: TurnId,
}

impl PlanWorkflowState {
    pub fn working_copy(&self) -> Option<PlanWorkingCopy<'_>> {
        match self {
            Self::Planning {
                draft: Some(draft), ..
            } => Some(PlanWorkingCopy::Draft(draft)),
            Self::Planning {
                previous: Some(artifact),
                ..
            } => Some(PlanWorkingCopy::Submitted(artifact)),
            _ => None,
        }
    }

    pub fn draft_scope(&self, turn_id: TurnId) -> Option<PlanDraftScope> {
        let Self::Planning {
            id,
            previous,
            draft,
        } = self
        else {
            return None;
        };
        Some(PlanDraftScope {
            target: PlanVersion {
                id: *id,
                revision: previous
                    .as_ref()
                    .map_or(Some(1), |artifact| artifact.version.revision.checked_add(1))?,
            },
            next_sequence: draft
                .as_ref()
                .map_or(Some(1), |draft| draft.sequence.checked_add(1))?,
            markdown: self.working_copy().map(|copy| copy.markdown().to_owned()),
            label: self.working_copy().map(|copy| copy.label()),
            sequence: draft.as_ref().map(|draft| draft.sequence),
            turn_id,
        })
    }

    /// Suffix for newly created checkpoints only; never changes instructions.
    pub fn checkpoint_context(&self) -> Option<String> {
        match self {
            Self::Planning { .. } => self.working_copy().map(|copy| {
                format!(
                    "{PLAN_DRAFT_CONTEXT_PREFIX} ({}):\n\n{}",
                    copy.label(),
                    copy.markdown()
                )
            }),
            Self::Ready { artifact } | Self::Published { artifact } => Some(format!(
                "{PLAN_ARTIFACT_CONTEXT_PREFIX} (revision {}):\n\n{}",
                artifact.version.revision, artifact.markdown
            )),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanEdit {
    Replace {
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    ReplaceSection {
        heading: String,
        content: String,
    },
}

pub fn derived_title(markdown: &str) -> Result<&str, PlanValidationError> {
    let title = markdown
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .ok_or_else(|| {
            PlanValidationError("markdown must begin with exactly `# <title>`".into())
        })?;
    super::validate_title(title)?;
    Ok(title)
}

pub fn validate_draft_markdown(
    markdown: &str,
    max_artifact_bytes: usize,
) -> Result<(), PlanValidationError> {
    if markdown.trim().is_empty() {
        return Err(PlanValidationError("markdown must not be empty".into()));
    }
    let max = max_artifact_bytes.min(MAX_PLAN_ARTIFACT_BYTES);
    if markdown.len() > max {
        return Err(PlanValidationError(format!(
            "markdown is {} bytes; the configured limit is {max} bytes",
            markdown.len()
        )));
    }
    derived_title(markdown)?;
    Ok(())
}

/// All canonical problems are reported together so one edit can fix them.
pub fn submission_problems(markdown: &str, max_artifact_bytes: usize) -> Vec<String> {
    let mut problems = Vec::new();
    if let Err(error) = derived_title(markdown) {
        problems.push(error.to_string());
    }
    let max = max_artifact_bytes.min(MAX_PLAN_ARTIFACT_BYTES);
    if markdown.len() > max {
        problems.push(format!(
            "markdown is {} bytes; the configured limit is {max} bytes",
            markdown.len()
        ));
    }
    for heading in ["Goal", "Decisions", "Implementation", "Validation", "Risks"] {
        if let Err(error) = super::validate_nonempty_section(markdown, heading) {
            problems.push(error.to_string());
        }
    }
    problems
}

#[derive(Debug)]
struct Heading<'a> {
    line: &'a str,
    level: usize,
    start: usize,
    body: usize,
}

/// Scan ATX headings while ignoring backtick/tilde fenced code. Offsets retain
/// the original UTF-8 bytes and line endings outside an edited section.
fn headings(markdown: &str) -> Vec<Heading<'_>> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut fence = None::<(u8, usize)>;
    for raw in markdown.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        let text = &line[indent..];
        if indent <= 3 {
            let marker = text.as_bytes().first().copied();
            let count = text
                .bytes()
                .take_while(|byte| Some(*byte) == marker)
                .count();
            if let Some((kind, width)) = fence {
                if marker == Some(kind) && count >= width && text[count..].trim().is_empty() {
                    fence = None;
                }
            } else if matches!(marker, Some(b'`' | b'~'))
                && count >= 3
                && (marker != Some(b'`') || !text[count..].contains('`'))
            {
                fence = Some((marker.unwrap(), count));
            } else if marker == Some(b'#')
                && (1..=6).contains(&count)
                && (text.len() == count || matches!(text.as_bytes()[count], b' ' | b'\t'))
            {
                result.push(Heading {
                    line,
                    level: count,
                    start: offset,
                    body: offset + raw.len(),
                });
            }
        }
        offset += raw.len();
    }
    result
}

fn edit_error(index: usize, message: &str, markdown: &str) -> PlanValidationError {
    let available = headings(markdown)
        .iter()
        .map(|heading| format!("`{}`", heading.line))
        .collect::<Vec<_>>()
        .join(", ");
    PlanValidationError(format!(
        "edit {}: {message}. Available headings: {}. Call read_plan for the exact stored text",
        index + 1,
        if available.is_empty() {
            "(none)"
        } else {
            &available
        }
    ))
}

pub fn apply_plan_edits(
    markdown: &str,
    edits: &[PlanEdit],
    max_artifact_bytes: usize,
) -> Result<String, PlanValidationError> {
    if !(1..=32).contains(&edits.len()) {
        return Err(PlanValidationError(
            "edits must contain 1–32 entries".into(),
        ));
    }
    let mut updated = markdown.to_owned();
    for (index, edit) in edits.iter().enumerate() {
        match edit {
            PlanEdit::Replace {
                old_string,
                new_string,
                replace_all,
            } => {
                if old_string.is_empty() {
                    return Err(edit_error(index, "old_string must not be empty", &updated));
                }
                let count = updated.matches(old_string.as_str()).count();
                if count == 0 || (count > 1 && !replace_all) {
                    return Err(edit_error(
                        index,
                        &format!(
                            "old_string matched {count} times; expected {}",
                            if *replace_all {
                                "at least one match"
                            } else {
                                "exactly one match"
                            }
                        ),
                        &updated,
                    ));
                }
                if old_string == new_string {
                    return Err(edit_error(
                        index,
                        "old_string and new_string are identical",
                        &updated,
                    ));
                }
                updated = if *replace_all {
                    updated.replace(old_string, new_string)
                } else {
                    updated.replacen(old_string, new_string, 1)
                };
            }
            PlanEdit::ReplaceSection { heading, content } => {
                let scanned = headings(&updated);
                let matches = scanned
                    .iter()
                    .enumerate()
                    .filter(|(_, found)| found.line == heading)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(edit_error(
                        index,
                        &format!(
                            "heading matched {} times; expected exactly one match",
                            matches.len()
                        ),
                        &updated,
                    ));
                }
                let (position, found) = matches[0];
                if found.level < 2 {
                    return Err(edit_error(
                        index,
                        "replace_section requires an ATX heading of level 2 or deeper",
                        &updated,
                    ));
                }
                let end = scanned[position + 1..]
                    .iter()
                    .find(|next| next.level <= found.level)
                    .map_or(updated.len(), |next| next.start);
                let newline = if updated[found.start..found.body].ends_with("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                // Normalize only trailing blank lines, retaining all other bytes.
                let mut offset = 0;
                let mut keep = 0;
                for raw in content.split_inclusive('\n') {
                    let line = raw.trim_end_matches(['\r', '\n']);
                    if !line.trim().is_empty() {
                        keep = offset + line.len();
                    }
                    offset += raw.len();
                }
                let mut body = content[..keep].to_owned();
                if found.body == found.start + found.line.len() {
                    body.insert_str(0, newline);
                }
                body.push_str(newline);
                if end < updated.len() {
                    body.push_str(newline);
                }
                updated.replace_range(found.body..end, &body);
            }
        }
    }
    if updated == markdown {
        return Err(PlanValidationError(
            "edits leave the working copy unchanged".into(),
        ));
    }
    validate_draft_markdown(&updated, max_artifact_bytes)?;
    Ok(updated)
}
