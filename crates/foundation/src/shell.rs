//! Frozen command environment. Windows uses Git for Windows Bash, never SHELL,
//! PowerShell, cmd.exe, or the legacy System32 WSL bash launcher.
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

#[derive(Debug, Clone)]
pub struct ShellSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    remove_env: Vec<&'static str>,
}
impl ShellSpec {
    pub fn apply(&self, command: &mut tokio::process::Command) {
        command.args(&self.args).envs(self.env.iter().cloned());
        for key in &self.remove_env {
            command.env_remove(key);
        }
        #[cfg(windows)]
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BASH_FUNC_") {
                command.env_remove(key);
            }
        }
    }
    pub fn command(&self, source: impl AsRef<OsStr>) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.executable);
        self.apply(&mut command);
        command.arg(source);
        command
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn resolve() -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            windows::discover()
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                executable: std::env::var_os("SHELL")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/bin/sh".into())
                    .into(),
                args: vec!["-lc".into()],
                env: vec![],
                remove_env: vec![],
            })
        }
    }

    /// Probe the actual frozen shell environment, not the launcher's PATH.
    pub async fn check_rtk(&self) -> anyhow::Result<()> {
        // On Windows, resolve() already validates that this is a Git for Windows
        // installation. Keep this probe focused on RTK availability rather than
        // adding a redundant shell-identity check that can cause false negatives.
        let probe = "command -v rtk >/dev/null 2>&1 && rtk --version";
        let output = crate::process::bounded_output(
            &mut self.command(probe),
            std::time::Duration::from_secs(8),
            8192,
        )
        .await?;
        anyhow::ensure!(
            output.status.success(),
            "RTK is unavailable in the command environment. Install RTK (on Windows: winget install rtk-ai.rtk), add rtk.exe to your user PATH, and restart Zevria. Git Bash: {}. {}",
            self.executable.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}

/// One selection for the entire process, shared by root, Explore and Build.
pub fn frozen() -> anyhow::Result<Arc<ShellSpec>> {
    static SHELL: OnceLock<Result<Arc<ShellSpec>, String>> = OnceLock::new();
    SHELL
        .get_or_init(|| {
            ShellSpec::resolve()
                .map(Arc::new)
                .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

pub async fn prepare_native() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        static READY: tokio::sync::OnceCell<Result<(), String>> =
            tokio::sync::OnceCell::const_new();
        READY
            .get_or_init(|| async {
                match frozen() {
                    Ok(shell) => shell.check_rtk().await.map_err(|e| format!("{e:#}")),
                    Err(e) => Err(format!("{e:#}")),
                }
            })
            .await
            .clone()
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Registry::*;

    const SETUP: &str = "Install Git for Windows, or set ZEVRIA_GIT_BASH to its bin\\bash.exe (not Windows\\System32\\bash.exe). PowerShell may launch Zevria but is not an agent command backend.";
    fn registry_root(hive: HKEY, view: u32) -> Option<PathBuf> {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let key = wide("SOFTWARE\\GitForWindows");
        let value = wide("InstallPath");
        let mut data = [0u16; 32768];
        let mut bytes = std::mem::size_of_val(&data) as u32;
        let status = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | view,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != 0 || bytes < 2 {
            return None;
        }
        let len = data.iter().position(|&c| c == 0)?;
        Some(OsString::from_wide(&data[..len]).into())
    }
    fn installation(candidate: &Path) -> Option<PathBuf> {
        let parent = candidate.parent()?;
        let root = if parent.file_name()?.eq_ignore_ascii_case("bin")
            && parent
                .parent()?
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("usr"))
        {
            parent.parent()?.parent()?
        } else {
            parent.parent()?
        };
        if !candidate.is_file()
            || !root.join("usr/bin/msys-2.0.dll").is_file()
            || !root.join("usr/bin/bash.exe").is_file()
            || !root.join("cmd/git.exe").is_file()
        {
            return None;
        }
        Some(root.to_path_buf())
    }
    pub(super) fn discover() -> anyhow::Result<ShellSpec> {
        discover_with(
            |key| std::env::var_os(key),
            || {
                [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
                    .into_iter()
                    .flat_map(|hive| {
                        [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
                            .into_iter()
                            .filter_map(move |view| registry_root(hive, view))
                    })
                    .collect()
            },
            crate::runtime_paths::home_dir()?,
        )
    }
    fn discover_with(
        env: impl Fn(&str) -> Option<OsString>,
        metadata: impl FnOnce() -> Vec<PathBuf>,
        home: PathBuf,
    ) -> anyhow::Result<ShellSpec> {
        let explicit = env("ZEVRIA_GIT_BASH");
        let mut candidates = Vec::new();
        if let Some(path) = explicit.as_ref() {
            let path = PathBuf::from(path);
            anyhow::ensure!(
                path.is_absolute()
                    && path
                        .file_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case("bash.exe")),
                "invalid ZEVRIA_GIT_BASH: use an absolute Git Bash executable path. {SETUP}"
            );
            candidates.push(path);
        } else {
            for root in metadata() {
                candidates.push(root.join("bin/bash.exe"));
            }
            for key in ["ProgramFiles", "ProgramFiles(x86)"] {
                if let Some(root) = env(key) {
                    candidates.push(PathBuf::from(root).join("Git/bin/bash.exe"));
                }
            }
            if let Some(root) = env("LOCALAPPDATA") {
                candidates.push(PathBuf::from(root).join("Programs/Git/bin/bash.exe"));
            }
            for dir in std::env::split_paths(&env("PATH").unwrap_or_default()) {
                candidates.push(dir.join("bash.exe"));
                if dir.join("git.exe").is_file() {
                    if let Some(root) = dir.parent() {
                        candidates.push(root.join("bin/bash.exe"));
                    }
                }
            }
        }
        let (executable, root) = candidates.into_iter().find_map(|p| installation(&p).map(|root| (p, root)))
            .ok_or_else(|| anyhow::anyhow!("{}Git Bash was not found or is not a valid Git for Windows installation. {SETUP}", if explicit.is_some() { "Invalid ZEVRIA_GIT_BASH override: " } else { "" }))?;
        let mut paths = vec![
            root.join("usr/bin"),
            root.join("mingw64/bin"),
            root.join("cmd"),
        ];
        paths.extend(std::env::split_paths(&env("PATH").unwrap_or_default()));
        Ok(ShellSpec {
            executable: executable.clone(),
            args: ["--noprofile", "--norc", "-c"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            env: vec![
                ("PATH".into(), std::env::join_paths(paths)?),
                ("HOME".into(), home.into_os_string()),
                ("SHELL".into(), executable.into_os_string()),
                ("CHERE_INVOKING".into(), "1".into()),
            ],
            remove_env: vec!["BASH_ENV", "ENV", "BASHOPTS", "SHELLOPTS", "CDPATH"],
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn discovery_validation_uses_an_injected_installation_tree() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("Git with spaces");
            for file in [
                "bin/bash.exe",
                "usr/bin/bash.exe",
                "usr/bin/msys-2.0.dll",
                "cmd/git.exe",
            ] {
                let file = root.join(file);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, b"fixture").unwrap();
            }
            assert_eq!(installation(&root.join("bin/bash.exe")), Some(root.clone()));
            assert_eq!(
                installation(&root.join("usr/bin/bash.exe")),
                Some(root.clone())
            );
            let spec = discover_with(
                |key| (key == "PATH").then(|| root.join("cmd").into_os_string()),
                Vec::new,
                temp.path().into(),
            )
            .unwrap();
            assert_eq!(spec.executable, root.join("bin/bash.exe"));
            assert_eq!(
                spec.args,
                ["--noprofile", "--norc", "-c"].map(OsString::from)
            );
            let invalid = discover_with(
                |key| {
                    (key == "ZEVRIA_GIT_BASH")
                        .then(|| OsString::from(r"C:\Windows\System32\bash.exe"))
                },
                || vec![root],
                temp.path().into(),
            );
            assert!(format!("{:#}", invalid.unwrap_err()).contains("ZEVRIA_GIT_BASH"));
            assert!(installation(Path::new(r"C:\Windows\System32\bash.exe")).is_none());
        }
    }
}
