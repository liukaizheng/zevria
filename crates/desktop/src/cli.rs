use std::{ffi::OsString, path::PathBuf};

pub const USAGE: &str = "Usage: zevria-desktop [WORKSPACE] [--continue|-c]";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cli {
    pub workspace: Option<PathBuf>,
    pub continue_session: bool,
}
impl Cli {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Self> {
        let mut cli = Self::default();
        let mut positional = false;
        for arg in args {
            match arg.to_str() {
                Some("--continue" | "-c") if !positional => cli.continue_session = true,
                Some("--") if !positional => positional = true,
                Some("--help" | "-h") if !positional => anyhow::bail!(USAGE),
                _ if !positional && arg.as_encoded_bytes().first() == Some(&b'-') => {
                    anyhow::bail!("unknown option: {}", arg.to_string_lossy())
                }
                _ => {
                    anyhow::ensure!(cli.workspace.is_none(), "only one workspace may be opened");
                    cli.workspace = Some(PathBuf::from(arg));
                }
            }
        }
        Ok(cli)
    }
    pub fn workspace(&self) -> anyhow::Result<Option<PathBuf>> {
        let path = self.workspace.clone().or_else(|| {
            // Explorer launches in the extraction directory. Use cwd on Windows
            // only for an explicit resume (or an explicit `.` above).
            if cfg!(windows) && !self.continue_session {
                return None;
            }
            std::env::current_dir()
                .ok()
                .filter(|p| p.parent().is_some())
        });
        path.map(|path| {
            let path = std::fs::canonicalize(path)?;
            anyhow::ensure!(path.is_dir(), "workspace is not a directory");
            Ok(path)
        })
        .transpose()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_flags_and_end_of_options() {
        assert_eq!(
            Cli::parse(["-c".into(), "/tmp".into()]).unwrap(),
            Cli {
                workspace: Some("/tmp".into()),
                continue_session: true
            }
        );
        assert_eq!(
            Cli::parse(["--".into(), "-c".into()]).unwrap().workspace,
            Some("-c".into())
        );
        assert!(Cli::parse(["a".into(), "b".into()]).is_err());
        assert!(Cli::parse(["--bogus".into()]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn explorer_launch_has_no_workspace_but_dot_and_continue_use_cwd() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(Cli::default().workspace().unwrap(), None);
        assert_eq!(
            Cli::parse([".".into()]).unwrap().workspace().unwrap(),
            Some(cwd.canonicalize().unwrap())
        );
        assert_eq!(
            Cli::parse(["--continue".into()])
                .unwrap()
                .workspace()
                .unwrap(),
            cwd.parent().map(|_| cwd.canonicalize().unwrap())
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_paths_preserve_spaces_and_non_ascii() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("demo workspace 雪");
        std::fs::create_dir(&path).unwrap();
        let cli = Cli::parse([path.clone().into_os_string()]).unwrap();
        assert_eq!(cli.workspace().unwrap(), Some(path.canonicalize().unwrap()));
    }
}
