//! Runtime storage paths. Project instructions and `.zevria/skills` are shared
//! inputs and must never be resolved through this state helper.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The runtime state directory within the workspace chosen by the frontend.
pub fn workspace_state_root(workspace: &Path) -> PathBuf {
    workspace.join(".zevria")
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
    fn workspace_state_root_is_dot_zevria() {
        assert_eq!(
            workspace_state_root(Path::new("workspace")),
            Path::new("workspace/.zevria")
        );
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
