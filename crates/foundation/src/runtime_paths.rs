//! Runtime-local storage. Project instructions and `.zevria/skills` are shared
//! inputs and must never be resolved through this state helper.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub fn workspace_state_root(workspace: &Path) -> PathBuf {
    state_root_for(workspace, cfg!(windows))
}

fn state_root_for(workspace: &Path, windows: bool) -> PathBuf {
    let root = workspace.join(".zevria");
    if windows { root.join("windows") } else { root }
}

/// A valid explicit HOME wins, including in native Windows tests/portable setups.
/// Invalid or relative overrides are ignored, never resolved against the workspace.
pub fn home_dir() -> anyhow::Result<PathBuf> {
    resolve_home(cfg!(windows), |key| std::env::var_os(key))
}

fn resolve_home(windows: bool, env: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    let valid = |value: Option<OsString>| value.map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(home) = valid(env("HOME")) {
        return Ok(home);
    }
    if windows {
        if let Some(home) = valid(env("USERPROFILE")) {
            return Ok(home);
        }
        if let (Some(mut drive), Some(path)) = (env("HOMEDRIVE"), env("HOMEPATH")) {
            drive.push(path);
            if let Some(home) = valid(Some(drive)) {
                return Ok(home);
            }
        }
    }
    anyhow::bail!(
        "cannot locate the user home: set an absolute HOME{}",
        if windows { " or USERPROFILE" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn histories_are_separate_but_unix_layout_is_unchanged() {
        let root = Path::new("workspace");
        assert_eq!(state_root_for(root, false), root.join(".zevria"));
        assert_eq!(state_root_for(root, true), root.join(".zevria/windows"));
    }
    #[test]
    fn home_resolution_is_injected_not_process_global() {
        let absolute = std::env::current_dir().unwrap();
        let env = |key: &str| match key {
            "HOME" => Some("relative".into()),
            "USERPROFILE" => Some(absolute.clone().into_os_string()),
            _ => None,
        };
        assert_eq!(resolve_home(true, env).unwrap(), absolute);
        assert!(resolve_home(false, env).is_err());
        assert_eq!(
            resolve_home(true, |key| (key == "HOME")
                .then(|| absolute.clone().into_os_string()))
            .unwrap(),
            absolute
        );
    }
}
