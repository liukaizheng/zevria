// Sanitized regressions for native handoffs; never read or rewrite old sessions.
use zevria_workflow::{NativePlanCapture, NativePlanSource};

struct NativeFixture {
    _directory: tempfile::TempDir,
    supervisor: EnsembleSupervisor,
    logs: PathBuf,
    artifact: PathBuf,
    trace: PathBuf,
}
impl NativeFixture {
    fn new(scenario: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let config = directory.path().join("claude-config");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(config.join("plans")).unwrap();
        let script = directory.path().join("native.py");
        let fixture = directory.path().join("native.json");
        let trace = directory.path().join("responses");
        std::fs::write(
            &script,
            python_fixture_script(include_str!("fixtures/native_handoff.py")),
        )
        .unwrap();
        std::fs::write(&fixture, include_str!("fixtures/native_handoff.json")).unwrap();
        let mut agent = fake_stdio_agent(
            &script,
            BTreeMap::from([
                ("SCENARIO".into(), scenario.into()),
                ("TRACE".into(), trace.display().to_string()),
                ("FIXTURE".into(), fixture.display().to_string()),
                (CLAUDE_CONFIG_DIR_ENV.into(), config.display().to_string()),
            ]),
        );
        agent.plan_handoff_transport = Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
        let logs = directory.path().join("logs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(agent),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .unwrap();
        Self {
            _directory: directory,
            supervisor,
            logs,
            // Match Python's component-wise os.path.join spelling on Windows.
            artifact: config.join("plans").join("proposal.md"),
            trace,
        }
    }
}

#[tokio::test]
async fn native_rejection_precedes_terminal_and_capture_in_both_prompt_orderings() {
    for scenario in ["prompt-first", "capture-first", "explicit", "transient"] {
        let fixture = NativeFixture::new(scenario);
        let (start, outcome, records) = launch_fake_worker(
            &fixture.supervisor,
            &fixture.logs,
            EnsembleWorkflow::Plan,
            "native handoff",
        )
        .await;
        assert!(
            outcome.failure.is_none(),
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert!(outcome.confirmation.is_none());
        assert_eq!(
            std::fs::read_to_string(&fixture.trace).unwrap_or_else(|error| {
                panic!("{scenario}: rejection trace: {error}; {}", fixture_diagnostics(&outcome, &records))
            }),
            "reject_once\n",
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert_eq!(
            recorded_prompts(&records).len(),
            1,
            "no model-visible repair envelope"
        );
        let captures = records
            .iter()
            .filter_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::NativePlanCaptured { plan, capture },
                } => Some((plan, capture)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(captures.len(), 1);
        let (plan, capture) = captures[0];
        capture.validate(plan).unwrap();
        assert_eq!(capture.generation, 1);
        if scenario == "explicit" {
            assert_eq!(capture.source, NativePlanSource::Explicit);
            assert_eq!(
                plan.markdown.as_deref(),
                Some("# Explicit proposal\n\n* Preserve Plan mode.")
            );
        } else {
            assert_eq!(
                plan.markdown.as_deref(),
                Some("# Frozen proposal\n\n- Preserve Plan mode.\n")
            );
            assert!(
                matches!(&capture.source, NativePlanSource::Artifact { artifact_tool_id, .. } if artifact_tool_id == "write-1")
            );
        }
        let reject = records.iter().position(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Permission { decision, option_id: Some(id), .. } } if decision == "reject_once" && id == "stay-planning-exact-id")).unwrap();
        let captured = records
            .iter()
            .position(|record| {
                matches!(
                    record,
                    AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::NativePlanCaptured { .. }
                    }
                )
            })
            .unwrap();
        assert!(reject < captured);
        assert!(!records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Permission { decision, .. } } if decision.starts_with("allow"))));
        let path = agent_run_path(&fixture.logs, &start.run_id, &outcome.descriptor.id);
        let before = load_agent_run_projection(&path).unwrap();
        std::fs::write(&fixture.artifact, "changed after capture").unwrap();
        assert_eq!(before, load_agent_run_projection(&path).unwrap());
        std::fs::remove_file(&fixture.artifact).unwrap();
        let replay = load_agent_run_projection(&path).unwrap();
        assert_eq!(before, replay);
        assert_eq!(replay.native_capture.as_ref(), Some(capture));
    }
}

#[tokio::test]
async fn native_refined_edit_capture_survives_timeout_continuation_and_replay() {
    let template: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native_handoff.json")).unwrap();
    let markdown = template["edited_markdown"].as_str().unwrap();
    for scenario in ["edit-refinement", "timeout-edit-refinement"] {
        let fixture = NativeFixture::new(scenario);
        let (start, outcome, records) = launch_fake_worker(
            &fixture.supervisor,
            &fixture.logs,
            EnsembleWorkflow::Plan,
            "refine a native proposal",
        )
        .await;
        assert_eq!(
            outcome.status,
            AgentRunStatus::AwaitingConfirmation,
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert!(
            outcome.failure.is_none(),
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert!(!outcome.partial);
        assert!(outcome.confirmation.is_none());
        assert_unconfirmed_review(&records);
        assert!(!records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Failure { .. }
            }
        )));
        assert_eq!(
            std::fs::read_to_string(&fixture.trace).unwrap_or_else(|error| {
                panic!("{scenario}: rejection trace: {error}; {}", fixture_diagnostics(&outcome, &records))
            }),
            "reject_once\n",
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert_eq!(
            std::fs::read_to_string(fixture.trace.with_extension("processes")).unwrap_or_else(|error| {
                panic!("{scenario}: process trace: {error}; {}", fixture_diagnostics(&outcome, &records))
            }),
            "spawn\n",
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );

        let prompts = recorded_prompts(&records);
        let recovered = scenario == "timeout-edit-refinement";
        assert_eq!(prompts.len(), if recovered { 2 } else { 1 });
        assert!(!prompts[0].continuation);
        assert!(prompts.iter().all(|prompt| prompt.repair.is_none()));
        if recovered {
            assert_eq!(
                prompts[1],
                WorkerPrompt {
                    text: "continue".into(),
                    continuation: true,
                    repair: None
                }
            );
        }
        let requests = protocol_requests(&records);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "session/new")
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|r| r["method"] == "session/resume" || r["method"] == "session/load")
        );
        let prompt_requests = requests
            .iter()
            .filter(|r| r["method"] == "session/prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompt_requests.len(), prompts.len());
        assert!(
            prompt_requests
                .iter()
                .all(|r| r["params"]["sessionId"] == "native-session")
        );
        let errors = records
            .iter()
            .filter_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        AgentRunEvent::Protocol {
                            direction: AgentProtocolDirection::AgentToClient,
                            json,
                        },
                } => serde_json::from_str::<serde_json::Value>(json)
                    .unwrap()
                    .get("error")
                    .cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            if recovered {
                vec![template["timeout_error"].clone()]
            } else {
                vec![]
            }
        );

        let captures = records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match record {
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::NativePlanCaptured { plan, capture },
                } => Some((index, plan, capture)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(captures.len(), 1);
        let (captured, plan, capture) = captures[0];
        assert_native_artifact(plan, capture, &fixture.artifact, "edit-1", markdown);
        assert_eq!(
            capture.generation, 1,
            "continuation preserves the proposal generation"
        );
        assert_eq!(capture.exit_tool_id, "exit-1");
        let terminal = records.iter().position(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::ToolCallUpdate { id, status: Some(status), .. } }
                if id == "edit-1" && status == "completed"
        )).unwrap();
        let reject = records.iter().position(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Permission { decision, option_id: Some(id), .. } }
                if decision == "reject_once" && id == "stay-planning-exact-id"
        )).unwrap();
        assert!(terminal < reject && reject < captured);
        assert!(!records.iter().any(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Permission { decision, .. } }
                if decision.starts_with("allow")
        )));
        let cancellations = records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        AgentRunEvent::Protocol {
                            direction: AgentProtocolDirection::ClientToAgent,
                            json,
                        },
                } if serde_json::from_str::<serde_json::Value>(json).unwrap()["method"]
                    == "session/cancel" =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !cancellations.is_empty(),
            "live prompt should receive bounded post-capture cancellation"
        );
        assert!(
            cancellations.iter().all(|index| *index > captured),
            "no early evidence-violation cancellation"
        );

        // Normalization must retain both diff representations as display data.
        let displayed = records
            .iter()
            .filter_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        AgentRunEvent::ToolCallUpdate {
                            id,
                            content: Some(content),
                            ..
                        },
                } if id == "edit-1" => Some(content),
                _ => None,
            })
            .flatten()
            .map(|content| serde_json::from_str::<serde_json::Value>(content).unwrap())
            .collect::<Vec<_>>();
        let expected = ["edit_preview", "edit_result"].map(|key| {
            let mut diff = template[key]["content"][0].clone();
            diff.as_object_mut().unwrap().remove("type");
            diff["path"] = serde_json::json!(fixture.artifact);
            diff
        });
        assert_eq!(displayed, expected);

        let path = agent_run_path(&fixture.logs, &start.run_id, &outcome.descriptor.id);
        let before = load_agent_run_projection(&path).unwrap();
        std::fs::write(&fixture.artifact, "changed after capture").unwrap();
        assert_eq!(before, load_agent_run_projection(&path).unwrap());
        std::fs::remove_file(&fixture.artifact).unwrap();
        let replay = load_agent_run_projection(&path).unwrap();
        assert_eq!(before, replay);
        assert_eq!(replay.native_capture.as_ref(), Some(capture));
        assert_eq!(
            replay.plan.as_ref().unwrap().markdown.as_deref(),
            Some(markdown)
        );
    }
}

#[tokio::test]
async fn native_settlement_timeout_and_disconnect_never_publish_success() {
    for scenario in ["timeout", "disconnect", "transient-timeout", "edit-timeout"] {
        let fixture = NativeFixture::new(scenario);
        let (_, outcome, records) = launch_fake_worker(
            &fixture.supervisor,
            &fixture.logs,
            EnsembleWorkflow::Plan,
            "bounded settlement",
        )
        .await;
        assert!(
            outcome.failure.is_some(),
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert!(outcome.confirmation.is_none());
        assert!(!records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::NativePlanCaptured { .. }
            }
        )));
        assert_eq!(
            std::fs::read_to_string(&fixture.trace).unwrap_or_else(|error| {
                panic!("{scenario}: rejection trace: {error}; {}", fixture_diagnostics(&outcome, &records))
            }),
            "reject_once\n",
            "{scenario}: {}",
            fixture_diagnostics(&outcome, &records)
        );
        assert_eq!(
            recorded_prompts(&records).len(),
            1,
            "failed native settlement must not dispatch a transient continuation"
        );
        if scenario != "disconnect" {
            assert!(
                outcome.failure.as_ref().unwrap().contains("settlement timed out"),
                "{scenario}: {}",
                fixture_diagnostics(&outcome, &records)
            );
        }
    }
}

fn native_exit(
    handoff: &ClaudePlanHandoff,
    id: &str,
    input: serde_json::Value,
) -> ClaudePlanCandidate {
    handoff.inspect_update(&acp_update(serde_json::json!({
        "sessionUpdate":"tool_call", "toolCallId":id, "title":"Propose", "kind":"switch_mode", "status":"pending", "rawInput":input,
        "_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}
    }))).unwrap();
    let request = permission_request(serde_json::json!({
        "sessionId":"session", "toolCall":{"toolCallId":id,"kind":"switch_mode","rawInput":input},
        "options":[{"optionId":"exact-reject","name":"Stay","kind":"reject_once"}]
    }));
    match handoff.permission(&request) {
        ClaudeHandoffPermission::ExitPlanMode { source } => source,
        _ => panic!("expected native permission"),
    }
}

fn native_mutation(
    handoff: &ClaudePlanHandoff,
    id: &str,
    operation: &str,
    path: &Path,
    status: &str,
) {
    handoff.inspect_update(&acp_update(serde_json::json!({
        "sessionUpdate":"tool_call", "toolCallId":id, "title":"Artifact", "kind":"edit", "status":status,
        "rawInput":{"file_path":path}, "_meta":{"claudeCode":{"toolName":operation}}
    }))).unwrap();
}

async fn resolve_native(
    handoff: &ClaudePlanHandoff,
    id: &str,
    input: serde_json::Value,
) -> Result<(AgentStructuredPlan, NativePlanCapture), String> {
    let source = native_exit(handoff, id, input);
    handoff.begin_settlement(id, source).unwrap();
    handoff.rejection_delivered().unwrap();
    handoff.resolve_capture().await
}

fn native_diff(path: &Path, old: Option<&str>, new: &str) -> serde_json::Value {
    serde_json::json!({"type": "diff", "path": path, "oldText": old, "newText": new})
}

fn native_diff_update(
    id: &str,
    operation: &str,
    input: serde_json::Value,
    diffs: Vec<serde_json::Value>,
) -> AcpSessionUpdate {
    acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update", "toolCallId": id,
        "rawInput": input, "content": diffs,
        "_meta": {"claudeCode": {"toolName": operation}}
    }))
}

fn assert_native_artifact(
    plan: &AgentStructuredPlan,
    capture: &NativePlanCapture,
    path: &Path,
    tool: &str,
    markdown: &str,
) {
    assert_eq!(plan.markdown.as_deref(), Some(markdown));
    let NativePlanSource::Artifact {
        artifact_tool_id,
        path: captured_path,
        content_digest,
    } = &capture.source
    else {
        panic!("expected an artifact capture");
    };
    assert_eq!(artifact_tool_id, tool);
    assert_eq!(
        content_digest,
        &NativePlanCapture::content_digest(markdown.as_bytes())
    );
    #[cfg(windows)]
    {
        // The configured root spelling is retained deliberately. A long/short
        // directory alias is the same source, not a different artifact.
        use zevria_foundation::contained_read::{OpenedRoot, RelativePath, file_snapshot};
        let identity = |path: &Path| {
            let root = OpenedRoot::open_absolute(path.parent().unwrap()).unwrap();
            let relative = RelativePath::new(Path::new(path.file_name().unwrap())).unwrap();
            file_snapshot(&root.open_file(&relative).unwrap())
                .unwrap()
                .identity
        };
        assert_eq!(identity(captured_path), identity(path));
    }
    #[cfg(not(windows))]
    assert_eq!(captured_path, &std::fs::canonicalize(path).unwrap());
    capture.validate(plan).unwrap();
}

#[tokio::test]
async fn native_edit_diff_refinements_resolve_the_complete_file() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let path = plans.join("plan.md");
    let before = "# Proposal\n\n## Approach\n\n- Draft detail.\n- Keep review.\n\n## Validation\n\n- Run checks.\n";
    let after = before.replace("Draft detail", "Clarified detail");
    let old_context = "- Draft detail.\n- Keep review.\n";
    let new_context = "- Clarified detail.\n- Keep review.\n";
    assert_eq!(before.replace(old_context, new_context), after);
    std::fs::write(&path, before).unwrap();
    handoff
        .inspect_update(&artifact_announcement("edit", "Edit"))
        .unwrap();
    handoff
        .inspect_update(&native_diff_update(
            "edit",
            "Edit",
            serde_json::json!({"file_path": path}),
            vec![],
        ))
        .unwrap();
    let preview = native_diff_update(
        "edit",
        "Edit",
        serde_json::json!({"file_path": path, "old_string": "Draft detail", "new_string": "Clarified detail"}),
        vec![native_diff(&path, Some("Draft detail"), "Clarified detail")],
    );
    // Repeated, statusless previews and context-expanded results describe the
    // same edit; neither is a whole document or terminal evidence.
    handoff.inspect_update(&preview).unwrap();
    handoff.inspect_update(&preview).unwrap();
    std::fs::write(&path, &after).unwrap();
    handoff
        .inspect_update(&native_diff_update(
            "edit",
            "Edit",
            serde_json::json!({}),
            vec![native_diff(&path, Some(old_context), new_context)],
        ))
        .unwrap();
    assert!(handoff.unresolved_violation().unwrap().contains("edit"));
    handoff
        .inspect_update(&artifact_status("edit", "Edit", "completed"))
        .unwrap();
    let (plan, capture) = resolve_native(&handoff, "exit", serde_json::json!({}))
        .await
        .unwrap();
    assert_native_artifact(&plan, &capture, &path, "edit", &after);
}

#[tokio::test]
async fn native_completed_edit_fragment_uses_full_file_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let path = plans.join("plan.md");
    let markdown = "# Complete proposal\n\n- Refined detail.\n- Preserve review.\n";
    std::fs::write(&path, markdown).unwrap();
    handoff
        .inspect_update(&acp_update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "edit", "title": "Refine plan",
            "kind": "edit", "status": "completed", "rawInput": {"file_path": path},
            "content": [native_diff(&path, None, "Refined detail")],
            "rawOutput": {"toolResponse": "Display output is not approval evidence."},
            "_meta": {"claudeCode": {"toolName": "Edit"}}
        })))
        .unwrap();
    // This must exercise the snapshot reader, not just notification acceptance.
    // Even oldText: null does not prove that newText is a complete document.
    let (plan, capture) = resolve_native(&handoff, "exit", serde_json::json!({}))
        .await
        .unwrap();
    assert_native_artifact(&plan, &capture, &path, "edit", markdown);
}

#[tokio::test]
async fn native_latest_multiedit_hunks_capture_whole_file_after_write_and_edit() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let path = plans.join("plan.md");
    let initial = "# Proposal\n\n- Draft approach.\n- Draft checks.\n- Keep review.\n";
    std::fs::write(&path, initial).unwrap();
    handoff
        .inspect_update(&acp_update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "write", "title": "Write plan",
            "kind": "edit", "status": "completed",
            "rawInput": {"file_path": path, "content": initial},
            "_meta": {"claudeCode": {"toolName": "Write"}}
        })))
        .unwrap();
    native_mutation(&handoff, "edit", "Edit", &path, "in_progress");
    handoff
        .inspect_update(&native_diff_update(
            "edit",
            "Edit",
            serde_json::json!({}),
            vec![native_diff(
                &path,
                Some("Draft approach"),
                "Refined approach",
            )],
        ))
        .unwrap();
    let edited = initial.replace("Draft approach", "Refined approach");
    std::fs::write(&path, &edited).unwrap();
    handoff
        .inspect_update(&artifact_status("edit", "Edit", "completed"))
        .unwrap();
    native_mutation(&handoff, "multi", "MultiEdit", &path, "in_progress");
    let hunks = native_diff_update(
        "multi",
        "MultiEdit",
        serde_json::json!({"file_path": path, "edits": [
            {"old_string": "Refined approach", "new_string": "Final approach"},
            {"old_string": "Draft checks", "new_string": "Final checks"}
        ]}),
        vec![
            native_diff(&path, Some("Refined approach"), "Final approach"),
            native_diff(&path, Some("Draft checks"), "Final checks"),
        ],
    );
    handoff.inspect_update(&hunks).unwrap();
    handoff.inspect_update(&hunks).unwrap();
    let final_markdown = edited
        .replace("Refined approach", "Final approach")
        .replace("Draft checks", "Final checks");
    std::fs::write(&path, &final_markdown).unwrap();
    handoff
        .inspect_update(&artifact_status("multi", "MultiEdit", "completed"))
        .unwrap();
    handoff
        .inspect_update(&native_diff_update(
            "multi",
            "MultiEdit",
            serde_json::json!({}),
            vec![native_diff(
                &path,
                Some("- Draft checks.\n- Keep review.\n"),
                "- Final checks.\n- Keep review.\n",
            )],
        ))
        .unwrap();
    let (plan, capture) = resolve_native(&handoff, "exit", serde_json::json!({}))
        .await
        .unwrap();
    assert_native_artifact(&plan, &capture, &path, "multi", &final_markdown);
}

#[tokio::test]
async fn native_write_display_diffs_do_not_override_whole_file_input() {
    for source in ["announcement", "update", "permission", "display-only"] {
        let directory = tempfile::tempdir().unwrap();
        let (_, plans, handoff) = claude_handoff_fixture(&directory);
        let _attempt = handoff.start_attempt(CancellationToken::new());
        let path = plans.join("plan.md");
        let markdown = "# Complete proposal\n\n- Preserve review.\n";
        std::fs::write(&path, markdown).unwrap();
        let mut input = serde_json::json!({"file_path": path});
        if source == "announcement" {
            input["content"] = markdown.into();
        }
        handoff
            .inspect_update(&acp_update(serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "write", "title": "Write plan",
                "kind": "edit", "status": "pending", "rawInput": input,
                "content": [native_diff(&path, None, "# Complete proposal")],
                "_meta": {"claudeCode": {"toolName": "Write"}}
            })))
            .unwrap();
        if source == "update" {
            handoff
                .inspect_update(&native_diff_update(
                    "write",
                    "Write",
                    serde_json::json!({"content": markdown}),
                    vec![],
                ))
                .unwrap();
        } else if source == "permission" {
            let request = permission_request(serde_json::json!({
                "sessionId": "session", "toolCall": {
                    "toolCallId": "write", "kind": "edit",
                    "rawInput": {"file_path": path, "content": markdown},
                    "content": [native_diff(&path, None, "- Preserve review.")],
                    "_meta": {"claudeCode": {"toolName": "Write"}}
                },
                "options": [{"optionId": "write-once", "name": "Once", "kind": "allow_once"}]
            }));
            assert!(matches!(
                handoff.permission(&request),
                ClaudeHandoffPermission::ArtifactMutation
            ));
        }
        handoff
            .inspect_update(&native_diff_update(
                "write",
                "Write",
                serde_json::json!({}),
                vec![native_diff(&path, None, "- Preserve review.\n")],
            ))
            .unwrap();
        handoff
            .inspect_update(&artifact_status("write", "Write", "completed"))
            .unwrap();
        let (plan, capture) = resolve_native(&handoff, "exit", serde_json::json!({}))
            .await
            .unwrap();
        assert_native_artifact(&plan, &capture, &path, "write", markdown);
    }
}

#[test]
fn native_write_authoritative_content_conflicts_fail_on_every_ingress() {
    for first in ["announcement", "update", "permission"] {
        for conflicting in ["announcement", "update", "permission"] {
            let directory = tempfile::tempdir().unwrap();
            let (_, plans, handoff) = claude_handoff_fixture(&directory);
            let path = plans.join("plan.md");
            handoff
                .inspect_update(&artifact_announcement("write", "Write"))
                .unwrap();
            let inspect = |ingress: &str, content: &str| -> Result<(), String> {
                let input = serde_json::json!({"file_path": path, "content": content});
                match ingress {
                    "announcement" => handoff.inspect_update(&acp_update(serde_json::json!({
                        "sessionUpdate": "tool_call", "toolCallId": "write", "title": "Write plan",
                        "kind": "edit", "status": "pending", "rawInput": input,
                        "_meta": {"claudeCode": {"toolName": "Write"}}
                    }))),
                    "update" => {
                        handoff.inspect_update(&native_diff_update("write", "Write", input, vec![]))
                    }
                    "permission" => {
                        let mut request = artifact_permission("write", "Write", &path);
                        request.tool_call.fields.raw_input = Some(input);
                        match handoff.permission(&request) {
                            ClaudeHandoffPermission::ArtifactMutation => Ok(()),
                            ClaudeHandoffPermission::Invalid(error) => Err(error),
                            _ => panic!("expected artifact permission"),
                        }
                    }
                    _ => unreachable!(),
                }
            };
            inspect(first, "# Original").unwrap();
            inspect(first, "# Original").unwrap();
            let error = inspect(conflicting, "# Contradiction").unwrap_err();
            assert!(
                error.contains("complete-content evidence"),
                "{first} -> {conflicting}: {error}"
            );
        }
    }
}

#[test]
fn native_display_diffs_still_validate_every_path() {
    for operation in ["Write", "Edit", "MultiEdit"] {
        for ingress in ["announcement", "update", "permission"] {
            for outside in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
                let _attempt = handoff.start_attempt(CancellationToken::new());
                let path = plans.join("plan.md");
                let other = if outside {
                    workspace.join("source.md")
                } else {
                    plans.join("other.md")
                };
                handoff
                    .inspect_update(&artifact_announcement("mutation", operation))
                    .unwrap();
                let input = serde_json::json!({"file_path": path});
                let diffs = vec![
                    native_diff(&path, None, "first"),
                    native_diff(&other, None, "second"),
                ];
                let error = match ingress {
                    "announcement" => handoff.inspect_update(&acp_update(serde_json::json!({
                        "sessionUpdate": "tool_call", "toolCallId": "mutation", "title": "Artifact",
                        "kind": "edit", "status": "pending", "rawInput": input,
                        "locations": [{"path": path}], "content": diffs,
                        "_meta": {"claudeCode": {"toolName": operation}}
                    }))).unwrap_err(),
                    "update" => handoff.inspect_update(&native_diff_update("mutation", operation, input, diffs)).unwrap_err(),
                    "permission" => {
                        let mut request = artifact_permission("mutation", operation, &path);
                        request.tool_call.fields.content = Some(serde_json::from_value(serde_json::json!(diffs)).unwrap());
                        match handoff.permission(&request) {
                            ClaudeHandoffPermission::Invalid(error) => error,
                            _ => panic!("invalid diff path accepted for {operation}"),
                        }
                    }
                    _ => unreachable!(),
                };
                if !outside {
                    assert!(error.contains("conflicting paths"), "{error}");
                }
                assert!(handoff.state.lock().unwrap().current_attempt.is_none());
            }
        }
    }
}

#[tokio::test]
async fn native_pending_latest_edit_cannot_fall_back_to_an_older_write() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let path = plans.join("plan.md");
    std::fs::write(&path, "# Older proposal").unwrap();
    native_mutation(&handoff, "write", "Write", &path, "completed");
    native_mutation(&handoff, "edit", "Edit", &path, "in_progress");
    let source = native_exit(&handoff, "exit", serde_json::json!({}));
    handoff.begin_settlement("exit", source).unwrap();
    handoff.rejection_delivered().unwrap();
    let capture = handoff.resolve_capture();
    tokio::pin!(capture);
    assert!(futures_util::poll!(&mut capture).is_pending());
    handoff
        .inspect_update(&artifact_status("edit", "Edit", "failed"))
        .unwrap();
    assert!(
        capture
            .await
            .unwrap_err()
            .contains("no eligible completed artifact")
    );
}

#[tokio::test]
async fn native_failed_generation_is_retired_without_erasing_ungranted_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let error = resolve_native(&handoff, "empty", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(error.contains("no eligible completed artifact"));
    handoff.abort_capture(error);
    handoff.begin_generation().unwrap();
    handoff.inspect_update(&acp_update(serde_json::json!({
        "sessionUpdate":"tool_call_update", "toolCallId":"empty", "status":"failed",
        "content":[{"type":"content","content":{"type":"text","text":"rejected"}}], "_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}
    }))).unwrap();
    assert!(handoff.unresolved_violation().is_none());
    native_mutation(
        &handoff,
        "write",
        "Write",
        &plans.join("plan.md"),
        "in_progress",
    );
    let source = native_exit(
        &handoff,
        "valid",
        serde_json::json!({"plan":"# Valid feedback"}),
    );
    handoff.begin_settlement("valid", source).unwrap();
    handoff.rejection_delivered().unwrap();
    let capture = handoff.resolve_capture();
    tokio::pin!(capture);
    assert!(futures_util::poll!(&mut capture).is_pending());
    handoff
        .inspect_update(&artifact_status("write", "Write", "completed"))
        .unwrap();
    let (plan, _) = capture.await.unwrap();
    assert_eq!(plan.markdown.as_deref(), Some("# Valid feedback"));
    handoff.finish_capture("valid").unwrap();
    assert!(handoff.is_completed());
    handoff.begin_generation().unwrap();
    native_mutation(
        &handoff,
        "ungranted",
        "Write",
        &plans.join("pending.md"),
        "in_progress",
    );
    handoff.begin_generation().unwrap();
    assert!(
        handoff
            .unresolved_violation()
            .unwrap()
            .contains("native execution may still be active")
    );
    handoff
        .inspect_update(&artifact_status("ungranted", "Write", "completed"))
        .unwrap();
    assert!(handoff.unresolved_violation().is_none());
}

#[tokio::test]
async fn native_failed_exit_statusless_output_is_inert_during_capture_and_later_generations() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let path = plans.join("plan.md");
    std::fs::write(&path, "# Frozen\n").unwrap();
    native_mutation(&handoff, "write", "Write", &path, "in_progress");
    let source = native_exit(&handoff, "exit", serde_json::json!({}));
    handoff.begin_settlement("exit", source).unwrap();
    handoff.rejection_delivered().unwrap();
    let output = serde_json::json!({
        "sessionUpdate":"tool_call_update", "toolCallId":"exit", "status":"failed",
        "content":[{"type":"content", "content":{"type":"text", "text":"The mode switch was rejected."}}],
        "_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}
    });
    handoff.inspect_update(&acp_update(output.clone())).unwrap();
    let mut statusless = output;
    statusless.as_object_mut().unwrap().remove("status");
    let statusless = acp_update(statusless);
    handoff.inspect_update(&statusless).unwrap();
    handoff
        .inspect_update(&artifact_status("write", "Write", "completed"))
        .unwrap();
    let (plan, _) = handoff.resolve_capture().await.unwrap();
    assert_eq!(plan.markdown.as_deref(), Some("# Frozen\n"));
    handoff.finish_capture("exit").unwrap();
    handoff.inspect_update(&statusless).unwrap();
    handoff.begin_generation().unwrap();
    handoff.inspect_update(&statusless).unwrap();
    assert!(handoff.unresolved_violation().is_none());
    assert!(!handoff.is_completed());
}

#[tokio::test]
async fn native_file_selection_uses_latest_current_generation_target_and_explicit_hint() {
    for scenario in [
        "ambiguous",
        "hint",
        "sequential",
        "failed",
        "old",
        "unobserved",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (_, plans, handoff) = claude_handoff_fixture(&directory);
        let _attempt = handoff.start_attempt(CancellationToken::new());
        let first = plans.join("first.md");
        let second = plans.join("second.md");
        std::fs::write(&first, "# First\n").unwrap();
        std::fs::write(&second, "# Second\n").unwrap();
        if scenario != "unobserved" {
            native_mutation(&handoff, "write", "Write", &first, "completed");
        }
        if matches!(scenario, "ambiguous" | "hint") {
            native_mutation(&handoff, "second", "Write", &second, "completed");
        }
        if scenario == "sequential" {
            native_mutation(&handoff, "edit", "Edit", &first, "completed");
            native_mutation(&handoff, "multi", "MultiEdit", &first, "completed");
        }
        if scenario == "failed" {
            native_mutation(&handoff, "edit", "Edit", &first, "failed");
        }
        if scenario == "old" {
            handoff.begin_generation().unwrap();
        }
        let input = if scenario == "hint" {
            serde_json::json!({"planFilePath":second})
        } else {
            serde_json::json!({})
        };
        let result = resolve_native(&handoff, "exit", input).await;
        match scenario {
            "hint" => assert_eq!(result.unwrap().0.markdown.as_deref(), Some("# Second\n")),
            "sequential" => assert!(
                matches!(result.unwrap().1.source, NativePlanSource::Artifact { artifact_tool_id, .. } if artifact_tool_id == "multi")
            ),
            "ambiguous" => assert!(result.unwrap_err().contains("ambiguous")),
            _ => assert!(result.unwrap_err().contains("no eligible"), "{scenario}"),
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_snapshot_rejects_unsafe_files_and_content_without_truncation() {
    use std::os::unix::fs::symlink;
    for scenario in [
        "empty",
        "oversized",
        "utf8",
        "symlink",
        "hardlink",
        "directory",
        "replaced-file",
        "replaced-directory",
        "disagreement",
        "traversal",
        "hint-unobserved",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (_, plans, handoff) = claude_handoff_fixture(&directory);
        let _attempt = handoff.start_attempt(CancellationToken::new());
        let path = plans.join("plan.md");
        std::fs::write(&path, "# Before").unwrap();
        native_mutation(&handoff, "write", "Write", &path, "completed");
        match scenario {
            "empty" => std::fs::write(&path, " \n").unwrap(),
            "oversized" => std::fs::write(
                &path,
                vec![b'x'; zevria_workflow::MAX_PLAN_ARTIFACT_BYTES + 1],
            )
            .unwrap(),
            "utf8" => std::fs::write(&path, [0xff]).unwrap(),
            "symlink" => {
                std::fs::remove_file(&path).unwrap();
                symlink("elsewhere.md", &path).unwrap();
            }
            "hardlink" => std::fs::hard_link(&path, plans.join("linked.md")).unwrap(),
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "replaced-file" => {
                std::fs::rename(&path, plans.join("old.md")).unwrap();
                std::fs::write(&path, "# Before").unwrap();
                handoff
                    .inspect_update(&artifact_status("write", "Write", "completed"))
                    .unwrap();
            }
            "replaced-directory" => {
                std::fs::rename(&plans, plans.with_extension("retired")).unwrap();
                std::fs::create_dir(&plans).unwrap();
                std::fs::write(&path, "# Replacement").unwrap();
            }
            "disagreement" => {
                handoff.inspect_update(&acp_update(serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"write","rawInput":{"content":"# Expected"},"_meta":{"claudeCode":{"toolName":"Write"}}}))).unwrap();
            }
            _ => {}
        }
        if matches!(scenario, "empty" | "oversized" | "utf8") {
            native_mutation(&handoff, "invalid-content", "Write", &path, "completed");
        }
        let input = match scenario {
            "traversal" => serde_json::json!({"planFilePath":plans.join("../plan.md")}),
            "hint-unobserved" => serde_json::json!({"planFilePath":plans.join("other.md")}),
            _ => serde_json::json!({}),
        };
        let error = resolve_native(&handoff, "exit", input).await.unwrap_err();
        if let Some(expected) = match scenario {
            "empty" => Some("empty"),
            "oversized" => Some("byte cap"),
            "utf8" => Some("UTF-8"),
            "replaced-file" => Some("replaced or changed"),
            _ => None,
        } {
            assert!(error.contains(expected), "{scenario}: {error}");
        }
    }
}
