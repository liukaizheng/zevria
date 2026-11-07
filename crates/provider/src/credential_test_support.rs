//! Cross-platform, non-interactive credential helper fixtures.
use super::*;

pub(crate) const PYTHON_COMMAND: &str = if cfg!(windows) { "python" } else { "python3" };

pub(crate) struct KeyHelper {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) source: ApiKeySource,
}
impl KeyHelper {
    pub(crate) fn new(script: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("helper.py");
        std::fs::write(&file, format!(
            "import pathlib, sys, time, subprocess, os\np = pathlib.Path('counter')\nn = int(p.read_text()) + 1 if p.exists() else 1\np.write_text(str(n))\n{script}\n"
        )).unwrap();
        let source = ApiKeySource::command(ApiKeyCommand {
            command: PYTHON_COMMAND.into(),
            args: vec![file.to_str().unwrap().into()],
            timeout_seconds: 30,
            refresh_interval_seconds: 300,
        })
        .with_config_directory(directory.path());
        Self { directory, source }
    }
    pub(crate) fn rotating() -> Self {
        Self::new("print('key-' + str(n))")
    }
    pub(crate) fn runs(&self) -> usize {
        std::fs::read_to_string(self.directory.path().join("counter"))
            .map(|text| text.parse().unwrap())
            .unwrap_or(0)
    }
}
