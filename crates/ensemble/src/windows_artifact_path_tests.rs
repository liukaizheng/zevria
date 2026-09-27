use super::*;
use zevria_foundation::windows_io::{identity, normalize_disk_path, open_directory};

fn ordinary(path: &Path) -> PathBuf {
    let text = path.to_str().expect("Unicode test path");
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
}

fn directory_spellings(path: &Path) -> Vec<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let long = std::fs::canonicalize(path).unwrap();
    let mut paths = vec![ordinary(&long), long.clone()];
    let wide = long
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: the input is NUL-terminated; the second call owns an output buffer
    // of the size requested by the first call, including the terminator.
    let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if required != 0 {
        let mut buffer = vec![0; required as usize];
        let written = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), required) };
        assert!(
            written > 0 && written < required,
            "short path query failed: {}",
            std::io::Error::last_os_error()
        );
        let short = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..written as usize]));
        if normalize_disk_path(&short).unwrap() != normalize_disk_path(&long).unwrap() {
            assert_eq!(
                identity(&open_directory(&short).unwrap()).unwrap(),
                identity(&open_directory(&long).unwrap()).unwrap()
            );
            paths.extend([ordinary(&short), short]);
        }
    }
    if paths.len() == 2 {
        eprintln!(
            "No distinct 8.3 alias for {}; exercising both drive prefixes",
            long.display()
        );
    }
    paths
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace with a long name");
    let config = root.join("external Claude configuration");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir_all(config.join("plans")).unwrap();
    (temp, workspace, config)
}

fn agent(config: &Path) -> EnsembleAgentConfig {
    let mut agent = EnsembleConfig::default().agents["claude"].clone();
    agent
        .env
        .insert(CLAUDE_CONFIG_DIR_ENV.into(), config.display().to_string());
    agent
}

#[test]
fn windows_supervisor_session_cwd_preserves_checked_directory_spellings() {
    let (temp, workspace, _config) = fixture();
    // Always covers ordinary and verbatim drive paths; distinct short/long
    // aliases participate when the filesystem provides them, without setup.
    for spelling in directory_spellings(&workspace) {
        let supervisor = EnsembleSupervisor::new(
            EnsembleConfig::default(),
            &spelling,
            temp.path().join("agent-runs"),
            test_questions(),
        )
        .unwrap_or_else(|error| panic!("workspace {spelling:?}: {error:#}"));
        assert_eq!(
            supervisor.workspace,
            normalize_disk_path(&spelling).unwrap(),
            "checked workspace spelling must not be canonicalized: {spelling:?}"
        );
        let request =
            serde_json::to_value(new_session_request(&supervisor.workspace, &None)).unwrap();
        assert_session_workspace(&request["cwd"], &spelling);
    }
}

#[test]
fn windows_claude_artifact_roots_and_targets_accept_equivalent_directory_spellings() {
    let (_temp, workspace, config) = fixture();
    let workspaces = directory_spellings(&workspace);
    let configs = directory_spellings(&config);
    let external_roots = directory_spellings(&config.join("plans"));
    let workspace_plans = workspace_claude_plan_artifact_directory(&workspace);
    let name = "plan 雪 space.md";
    // Neither workspace component exists, only .claude exists, both exist, and
    // finally both permitted roots have an existing regular-file target.
    for stage in 0..4 {
        match stage {
            1 => std::fs::create_dir(workspace.join(".claude")).unwrap(),
            2 => std::fs::create_dir(&workspace_plans).unwrap(),
            3 => {
                std::fs::write(workspace_plans.join(name), "# Local plan").unwrap();
                std::fs::write(config.join("plans").join(name), "# External plan").unwrap();
            }
            _ => {}
        }
        let mut local_roots = workspaces
            .iter()
            .map(|workspace| workspace_claude_plan_artifact_directory(workspace))
            .collect::<Vec<_>>();
        if workspace_plans.exists() {
            local_roots.extend(directory_spellings(&workspace_plans));
        }
        for workspace in &workspaces {
            let local = workspace_claude_plan_artifact_directory(workspace);
            for root in &local_roots {
                validate_workspace_claude_plan_artifact_directory(root, workspace).unwrap();
                let local_config = root.parent().unwrap();
                assert_eq!(
                    claude_plan_artifact_directory(&agent(local_config), workspace).unwrap(),
                    normalize_disk_path(&local).unwrap()
                );
            }
            for config in &configs {
                let external = claude_plan_artifact_directory(&agent(config), workspace).unwrap();
                assert_eq!(
                    external,
                    normalize_disk_path(&config.join("plans")).unwrap()
                );
                for (roots, permitted) in [(&external_roots, &external), (&local_roots, &local)] {
                    for root in roots {
                        let validated = validate_claude_plan_artifact_path(
                            &root.join(name),
                            &external,
                            &local,
                            workspace,
                        )
                        .unwrap();
                        assert_eq!(
                            validated,
                            normalize_disk_path(permitted).unwrap().join(name)
                        );
                        for invalid in [
                            root.join("plan.md:stream"),
                            root.join(r"..\plan.md"),
                            root.join("plan.txt"),
                        ] {
                            assert!(
                                validate_claude_plan_artifact_path(
                                    &invalid, &external, &local, workspace
                                )
                                .is_err()
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(
            workspace_plans.exists(),
            stage >= 2,
            "validation must not create workspace directories"
        );
    }
}

#[tokio::test]
async fn windows_claude_aliases_survive_permission_and_completed_artifact_capture() {
    for local in [false, true] {
        for existing in [false, true] {
            let (_temp, workspace, config) = fixture();
            let workspaces = directory_spellings(&workspace);
            let configs = directory_spellings(&config);
            for (index, workspace) in workspaces.iter().enumerate() {
                for config_form in &configs {
                    let handoff = ClaudePlanHandoff::new(&agent(config_form), workspace).unwrap();
                    let attempt = handoff.start_attempt(CancellationToken::new());
                    let target_root = if local {
                        workspace_claude_plan_artifact_directory(
                            &workspaces[(index + 1) % workspaces.len()],
                        )
                    } else {
                        config.join("plans")
                    };
                    if existing {
                        std::fs::create_dir_all(&target_root).unwrap();
                    }
                    let roots = if target_root.exists() {
                        directory_spellings(&target_root)
                    } else {
                        vec![target_root.clone()]
                    };
                    for (target_index, target_root) in roots.iter().enumerate() {
                        let id = format!("write-{target_index}");
                        let name = format!("plan-{index}-{target_index}.md");
                        let target = target_root.join(&name);
                        if existing {
                            std::fs::write(&target, "# Draft\n").unwrap();
                        }
                        handoff
                            .inspect_update(&artifact_announcement(&id, "Write"))
                            .unwrap();
                        let normalized_target = {
                            let state = handoff.state.lock().unwrap();
                            (if local {
                                &state.workspace_artifact_directory
                            } else {
                                &state.artifact_directory
                            })
                            .join(&name)
                        };
                        let refinement = acp_update(serde_json::json!({
                            "sessionUpdate": "tool_call_update", "toolCallId": id,
                            "rawInput": {"file_path": target},
                            "locations": [{"path": normalized_target}],
                            "_meta": {"claudeCode": {"toolName": "Write"}}
                        }));
                        handoff.inspect_update(&refinement).unwrap();
                        let request = artifact_permission(&id, "Write", &target);
                        let ticket = handoff.register_permission(&request, attempt.id).unwrap();
                        assert_eq!(
                            ticket.try_admit(&request, || false).unwrap(),
                            ClaudePlanAdmission::Granted
                        );
                        ticket.delivered();
                        assert!(target_root.is_dir());
                        std::fs::write(&target, "# Complete alias plan\n").unwrap();
                        handoff
                            .inspect_update(&artifact_status(&id, "Write", "completed"))
                            .unwrap();
                        let exit = format!("exit-{target_index}");
                        let (plan, capture) = resolve_native(
                            &handoff,
                            &exit,
                            serde_json::json!({"planFilePath": target}),
                        )
                        .await
                        .unwrap();
                        assert_native_artifact(
                            &plan,
                            &capture,
                            &target,
                            &id,
                            "# Complete alias plan\n",
                        );
                        assert!(
                            matches!(&capture.source, NativePlanSource::Artifact { path, .. } if path == &normalized_target)
                        );
                        handoff.finish_capture(&exit).unwrap();
                        handoff.begin_generation().unwrap();
                        std::fs::remove_file(&target).unwrap();
                    }
                }
            }
        }
    }
}

#[test]
fn windows_claude_aliases_cannot_authorize_other_workspace_or_unconfigured_directories() {
    let (temp, workspace, config) = fixture();
    let other = workspace.join("another Claude configuration");
    let outside = temp.path().join("not the configured directory");
    std::fs::create_dir(&other).unwrap();
    std::fs::create_dir_all(outside.join("plans")).unwrap();
    for existing in [false, true] {
        if existing {
            std::fs::create_dir(other.join("plans")).unwrap();
            std::fs::write(other.join("plans/plan.md"), "# Forbidden\n").unwrap();
            std::fs::write(outside.join("plans/plan.md"), "# Forbidden\n").unwrap();
        }
        for workspace in directory_spellings(&workspace) {
            let local = workspace_claude_plan_artifact_directory(&workspace);
            for config_alias in directory_spellings(&other) {
                assert!(claude_plan_artifact_directory(&agent(&config_alias), &workspace).is_err());
                let forbidden_root = config_alias.join("plans");
                assert!(
                    validate_claude_plan_artifact_path(
                        &forbidden_root.join("plan.md"),
                        &forbidden_root,
                        &local,
                        &workspace
                    )
                    .is_err(),
                    "even an injected external-root alias must respect the workspace boundary"
                );
                assert!(
                    validate_claude_plan_artifact_path(
                        &forbidden_root.join("plan.md"),
                        &config.join("plans"),
                        &local,
                        &workspace
                    )
                    .is_err()
                );
            }
            for outside in directory_spellings(&outside.join("plans")) {
                assert!(
                    validate_claude_plan_artifact_path(
                        &outside.join("plan.md"),
                        &config.join("plans"),
                        &local,
                        &workspace
                    )
                    .is_err()
                );
                assert!(
                    validate_workspace_claude_plan_artifact_directory(&outside, &workspace)
                        .is_err()
                );
            }
        }
    }
}

fn junction(link: &Path, target: &Path) {
    let output = std::process::Command::new(
        zevria_foundation::windows_process::system_executable("cmd.exe").unwrap(),
    )
    .args(["/d", "/c", "mklink", "/J"])
    .arg(ordinary(link))
    .arg(ordinary(target))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn windows_claude_reparse_paths_are_rejected_even_when_their_destinations_are_permitted() {
    let (temp, workspace, config) = fixture();
    let plans = config.join("plans");
    let target = plans.join("plan.md");
    std::fs::write(&target, "# Plan\n").unwrap();
    let link = temp.path().join("configuration junction");
    junction(&link, &config);
    let local = workspace_claude_plan_artifact_directory(&workspace);
    assert!(claude_plan_artifact_directory(&agent(&link), &workspace).is_err());
    for name in ["plan.md", "missing.md"] {
        assert!(
            validate_claude_plan_artifact_path(
                &link.join("plans").join(name),
                &plans,
                &local,
                &workspace
            )
            .is_err()
        );
    }
    std::fs::remove_dir(&link).unwrap();

    // A missing plans directory beyond a reparse ancestor must not be mistaken
    // for an ordinary, not-yet-prepared workspace-local artifact directory.
    let destination = temp.path().join("empty junction destination");
    std::fs::create_dir(&destination).unwrap();
    junction(&workspace.join(".claude"), &destination);
    assert!(validate_workspace_claude_plan_artifact_directory(&local, &workspace).is_err());
    assert!(
        validate_claude_plan_artifact_path(&local.join("missing.md"), &plans, &local, &workspace)
            .is_err()
    );
    assert!(
        claude_plan_artifact_directory(&agent(&workspace.join(".claude")), &workspace).is_err()
    );
    std::fs::remove_dir(workspace.join(".claude")).unwrap();

    let file_link = plans.join("linked.md");
    match std::os::windows::fs::symlink_file(&target, &file_link) {
        Ok(()) => {
            assert!(
                validate_claude_plan_artifact_path(&file_link, &plans, &local, &workspace).is_err()
            );
            std::fs::remove_file(file_link).unwrap();
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD as i32) =>
        {
            eprintln!("File-symlink fixture needs Developer Mode/privilege: {error}");
        }
        Err(error) => panic!("file-symlink fixture failed: {error}"),
    }
}
