use std::path::PathBuf;

pub const USAGE: &str = "Usage: zevria-desktop [WORKSPACE] [--continue|-c]";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cli {
    pub workspace: Option<PathBuf>,
    pub continue_session: bool,
}
impl Cli {
    pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut cli = Self::default();
        let mut positional = false;
        for arg in args {
            match arg.as_str() {
                "--continue" | "-c" if !positional => cli.continue_session = true,
                "--" if !positional => positional = true,
                "--help" | "-h" if !positional => {
                    anyhow::bail!(USAGE)
                }
                _ if !positional && arg.starts_with('-') => anyhow::bail!("unknown option: {arg}"),
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
}
