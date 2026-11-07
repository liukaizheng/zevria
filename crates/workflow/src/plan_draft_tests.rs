use super::*;

fn text() -> String {
    "# Durable draft based plans\n\n## Goal\nShip.\n\n## Decisions\nRust.\n\n## Implementation\nBuild.\n\n## Validation\nTest.\n\n## Risks\nNone.\n".into()
}
fn initial() -> PlanWorkflowState {
    apply_plan_record(
        PlanWorkflowState::Idle,
        &PlanRecord::Started { id: PlanId::new() },
    )
    .unwrap()
}
fn draft(state: &PlanWorkflowState) -> PlanDraft {
    let scope = state.draft_scope(TurnId::new(1)).unwrap();
    PlanDraft {
        target: scope.target,
        sequence: scope.next_sequence,
        markdown: text(),
        source_turn_id: scope.turn_id,
    }
}
#[test]
fn draft_replay_revision_and_legacy() {
    let state = initial();
    let first = draft(&state);
    let record = PlanRecord::DraftSaved {
        draft: first.clone(),
    };
    assert_eq!(
        serde_json::from_str::<PlanRecord>(&serde_json::to_string(&record).unwrap()).unwrap(),
        record
    );
    assert!(record.model_message().is_none());
    let state = apply_plan_record(state, &record).unwrap();
    let second = draft(&state);
    assert_eq!(second.sequence, 2);
    let state = apply_plan_record(
        state,
        &PlanRecord::DraftSaved {
            draft: second.clone(),
        },
    )
    .unwrap();
    let artifact = PlanCandidate::from_markdown(text(), usize::MAX)
        .unwrap()
        .into_artifact(second.target, second.source_turn_id);
    let ready = apply_plan_record(
        state,
        &PlanRecord::Ready {
            artifact: artifact.clone(),
        },
    )
    .unwrap();
    assert!(ready.working_copy().is_none());
    assert!(apply_plan_record(ready.clone(), &record).is_err());
    let revised = apply_plan_record(ready, &PlanRecord::RevisionRequested { artifact }).unwrap();
    assert!(matches!(
        revised.working_copy(),
        Some(PlanWorkingCopy::Submitted(_))
    ));
    let next = draft(&revised);
    assert_eq!((next.target.revision, next.sequence), (2, 1));
    let old = initial();
    let target = draft(&old).target;
    let artifact = PlanCandidate::from_markdown(text(), usize::MAX)
        .unwrap()
        .into_artifact(target, TurnId::new(2));
    assert!(apply_plan_record(old, &PlanRecord::Ready { artifact }).is_ok());
}
#[test]
fn reducer_rejects_invalid_drafts_and_mismatched_artifacts() {
    let state = initial();
    let valid = draft(&state);
    for broken in [
        PlanDraft {
            target: PlanVersion {
                id: PlanId::new(),
                ..valid.target
            },
            ..valid.clone()
        },
        PlanDraft {
            target: PlanVersion {
                revision: 2,
                ..valid.target
            },
            ..valid.clone()
        },
        PlanDraft {
            sequence: 2,
            ..valid.clone()
        },
        PlanDraft {
            markdown: "# Too short".into(),
            ..valid.clone()
        },
        PlanDraft {
            markdown: format!("{}{}", text(), "x".repeat(MAX_PLAN_ARTIFACT_BYTES)),
            ..valid.clone()
        },
    ] {
        assert!(
            apply_plan_record(state.clone(), &PlanRecord::DraftSaved { draft: broken }).is_err()
        );
    }
    assert!(
        apply_plan_record(
            PlanWorkflowState::Idle,
            &PlanRecord::DraftSaved {
                draft: valid.clone()
            }
        )
        .is_err()
    );
    let saved = apply_plan_record(
        state,
        &PlanRecord::DraftSaved {
            draft: valid.clone(),
        },
    )
    .unwrap();
    let artifact = PlanCandidate::from_markdown(text().replace("Ship.", "Different."), usize::MAX)
        .unwrap()
        .into_artifact(valid.target, valid.source_turn_id);
    assert!(
        apply_plan_record(
            saved.clone(),
            &PlanRecord::Ready {
                artifact: artifact.clone()
            }
        )
        .is_err()
    );
    assert!(
        apply_plan_record(
            saved.clone(),
            &PlanRecord::Published {
                artifact,
                provenance: PlanPublicationProvenance::Synthesized
            }
        )
        .is_err()
    );
    assert_eq!(
        apply_plan_record(
            saved,
            &PlanRecord::Resolved {
                id: valid.target.id,
                artifact: None,
                resolution: PlanResolution::Abandoned
            }
        )
        .unwrap(),
        PlanWorkflowState::Idle
    );
}
fn replace(old: &str, new: &str, all: bool) -> PlanEdit {
    PlanEdit::Replace {
        old_string: old.into(),
        new_string: new.into(),
        replace_all: all,
    }
}
fn section(heading: &str, content: &str) -> PlanEdit {
    PlanEdit::ReplaceSection {
        heading: heading.into(),
        content: content.into(),
    }
}
#[test]
fn literal_edits_are_atomic_and_validate_the_final_text() {
    let source = text();
    assert!(
        apply_plan_edits(&source, &[replace("Ship.", "Done.", false)], usize::MAX)
            .unwrap()
            .contains("Done.")
    );
    for edit in [
        replace("", "x", false),
        replace("missing", "x", false),
        replace("## ", "x", false),
        replace("Ship.", "Ship.", false),
    ] {
        let error = apply_plan_edits(&source, &[edit], usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("edit 1") && error.contains("read_plan") && error.contains("## Goal")
        );
    }
    assert!(apply_plan_edits(&source, &[replace("## ", "### ", true)], usize::MAX).is_ok());
    assert!(
        apply_plan_edits(
            &source,
            &[
                replace("Ship.", "Done.", false),
                replace("absent", "x", false)
            ],
            usize::MAX
        )
        .is_err()
    );
    assert!(
        apply_plan_edits(
            &source,
            &[
                replace("Ship.", "Done.", false),
                replace("Done.", "Ship.", false)
            ],
            usize::MAX
        )
        .is_err()
    );
    assert!(
        apply_plan_edits(
            &source,
            &[replace("Durable draft based plans", "Short", false)],
            usize::MAX
        )
        .is_err()
    );
    assert!(
        apply_plan_edits(
            &source,
            &[replace("Ship.", "Longer content.", false)],
            source.len()
        )
        .is_err()
    );
}
#[test]
fn section_edits_preserve_bytes_and_ignore_fences() {
    let source = "# Durable draft based plans\r\n\r\n## Goal\r\nOld.\r\n### Nested\r\nNested body.\r\n```\r\n## Goal\r\n```\r\n\r\n## Risks\r\nLast.";
    assert_eq!(
        apply_plan_edits(source, &[section("## Goal", "New.\n\n  \n")], usize::MAX).unwrap(),
        "# Durable draft based plans\r\n\r\n## Goal\r\nNew.\r\n\r\n## Risks\r\nLast."
    );
    let nested = apply_plan_edits(
        source,
        &[section("### Nested", "Nested replacement.")],
        usize::MAX,
    )
    .unwrap();
    assert!(
        nested.starts_with("# Durable draft based plans\r\n\r\n## Goal\r\nOld.\r\n### Nested\r\n")
    );
    assert!(nested.ends_with("## Risks\r\nLast."));
    assert!(
        apply_plan_edits(source, &[section("## Risks", "End.")], usize::MAX)
            .unwrap()
            .ends_with("## Risks\r\nEnd.\r\n")
    );
    for edit in [
        section("# Durable draft based plans", "No."),
        section("## Absent", "No."),
    ] {
        assert!(
            apply_plan_edits(source, &[edit], usize::MAX)
                .unwrap_err()
                .to_string()
                .contains("Available headings:")
        );
    }
    let duplicate = format!("{source}\n## Risks\nAgain.");
    assert!(apply_plan_edits(&duplicate, &[section("## Risks", "No.")], usize::MAX).is_err());
}
#[test]
fn submission_errors_are_aggregated_and_title_is_derived() {
    let candidate = PlanCandidate::from_markdown(text(), usize::MAX).unwrap();
    assert_eq!(candidate.title(), "Durable draft based plans");
    let error = PlanCandidate::from_markdown(
        "# Durable draft based plans\n## Goal\n\n## Risks\n".into(),
        usize::MAX,
    )
    .unwrap_err()
    .to_string();
    for section in ["Goal", "Decisions", "Implementation", "Validation", "Risks"] {
        assert!(error.contains(section));
    }
}

#[test]
fn checkpoints_preserve_exact_working_text_or_submitted_artifacts_and_resolution_drops_drafts() {
    let state = initial();
    let saved = draft(&state);
    let state = apply_plan_record(
        state,
        &PlanRecord::DraftSaved {
            draft: saved.clone(),
        },
    )
    .unwrap();
    assert!(
        state
            .checkpoint_context()
            .unwrap()
            .ends_with(&saved.markdown)
    );
    let artifact = PlanCandidate::from_markdown(saved.markdown.clone(), usize::MAX)
        .unwrap()
        .into_artifact(saved.target, saved.source_turn_id);
    let ready = apply_plan_record(
        state.clone(),
        &PlanRecord::Ready {
            artifact: artifact.clone(),
        },
    )
    .unwrap();
    let published = apply_plan_record(
        state,
        &PlanRecord::Published {
            artifact: artifact.clone(),
            provenance: PlanPublicationProvenance::Synthesized,
        },
    )
    .unwrap();
    for submitted in [ready.clone(), published] {
        let context = submitted.checkpoint_context().unwrap();
        assert!(context.starts_with(PLAN_ARTIFACT_CONTEXT_PREFIX));
        assert!(context.ends_with(&artifact.markdown));
    }
    let revised = apply_plan_record(
        ready,
        &PlanRecord::RevisionRequested {
            artifact: artifact.clone(),
        },
    )
    .unwrap();
    assert!(
        revised
            .checkpoint_context()
            .unwrap()
            .contains("submitted revision 1")
    );
    let mut newer = draft(&revised);
    newer.markdown.push_str("\nUnsubmitted changes.");
    let revised = apply_plan_record(revised, &PlanRecord::DraftSaved { draft: newer }).unwrap();
    let resolved = apply_plan_record(
        revised,
        &PlanRecord::Resolved {
            id: artifact.version.id,
            artifact: Some(artifact),
            resolution: PlanResolution::ImplementedCurrent,
        },
    )
    .unwrap();
    assert!(resolved.working_copy().is_none() && resolved.checkpoint_context().is_none());
    assert!(PlanWorkflowState::Idle.checkpoint_context().is_none());
}

#[test]
fn confirmed_worker_publication_does_not_have_to_equal_a_saved_canonical_draft() {
    let state = initial();
    let saved = draft(&state);
    let state = apply_plan_record(
        state,
        &PlanRecord::DraftSaved {
            draft: saved.clone(),
        },
    )
    .unwrap();
    let artifact = DirectWorkerPlan::validate("Byte-exact external proposal.  ".into())
        .unwrap()
        .into_artifact(saved.target, saved.source_turn_id);
    let worker_id = AgentRunId::new();
    let published = apply_plan_record(
        state,
        &PlanRecord::Published {
            artifact: artifact.clone(),
            provenance: PlanPublicationProvenance::ConfirmedWorker {
                run_id: EnsembleRunId::new(),
                worker_id: worker_id.clone(),
                revision: WorkerPlanRevision {
                    worker_id,
                    generation: 0,
                    revision: 1,
                    digest: "proof".into(),
                },
            },
        },
    )
    .unwrap();
    assert_eq!(published.artifact(), Some(&artifact));
}
