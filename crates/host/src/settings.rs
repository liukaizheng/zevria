//! Formatting-preserving configuration transactions shared by skill/model/theme writers.
use anyhow::Context as _;
use std::path::{Path, PathBuf};

pub(crate) fn transaction<T>(
    path: &Path,
    prepare: impl FnOnce(&str) -> anyhow::Result<(String, T)>,
) -> anyhow::Result<T> {
    transaction_for(path, path, prepare)
}

pub(crate) fn transaction_for<T>(
    lock_anchor: &Path,
    path: &Path,
    prepare: impl FnOnce(&str) -> anyhow::Result<(String, T)>,
) -> anyhow::Result<T> {
    regular_writable(path)?;
    let _lock = lock_configuration(lock_anchor)?;
    regular_writable(path)?;
    let before = read_regular(path)?;
    let (after, prepared) = prepare(&before)?;
    regular_writable(path)?;
    anyhow::ensure!(
        read_regular(path)? == before,
        "configuration changed while preparing the update; nothing was written"
    );
    if after != before {
        zevria_foundation::atomic_file::replace(path, after.as_bytes())?;
    }
    Ok(prepared)
}

fn lock_configuration(path: &Path) -> anyhow::Result<std::fs::File> {
    // Compatibility: all writers keep the original stable skill-lock inode.
    let mut name = path.as_os_str().to_os_string();
    name.push(".skills.lock");
    let lock_path = PathBuf::from(name);
    if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "configuration lock must be a regular file"
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(windows))]
    let lock = options
        .open(&lock_path)
        .context("cannot open configuration lock")?;
    #[cfg(windows)]
    let lock = zevria_foundation::windows_io::open_lock(&lock_path, true)
        .context("cannot safely open configuration lock")?;
    anyhow::ensure!(
        lock.metadata()?.is_file(),
        "configuration lock must be regular"
    );
    lock.try_lock()
        .context("configuration is busy in another process; retry")?;
    Ok(lock)
}

fn regular_writable(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .context("mutation requires an existing regular configuration file")?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "configuration must not be a reparse point"
        );
    }
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "configuration must be a regular file, not a symlink"
    );
    anyhow::ensure!(
        !metadata.permissions().readonly(),
        "configuration file is read-only"
    );
    Ok(())
}

pub(crate) fn read_regular(path: &Path) -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(windows))]
    let mut file = options
        .open(path)
        .context("cannot read existing configuration")?;
    #[cfg(windows)]
    let mut file = {
        let path = std::path::absolute(path)?;
        let parent = zevria_foundation::windows_io::open_directory(
            path.parent().context("configuration has no parent")?,
        )?;
        zevria_foundation::windows_io::open_relative(
            &parent,
            path.file_name().context("configuration has no filename")?,
            false,
            false,
        )?
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "configuration must be a regular file"
    );
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(contents)
}

/// Opt-in first-run creation, used only by named-theme generation. Preparation
/// holds the shared lock and syncs a sibling stage, but does not publish it.
/// The caller may commit a theme before calling `commit` on this selector.
pub(crate) fn prepare_create_or_update(
    path: &Path,
    skeleton: &str,
    prepare: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<PreparedUpdate> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).context("cannot create configuration directory")?;
    let lock = lock_configuration(path)?;
    let before = match std::fs::symlink_metadata(path) {
        Ok(_) => {
            regular_writable(path)?;
            Some(read_regular(path)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let after = prepare(before.as_deref().unwrap_or(skeleton))?;
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).context("cannot stage configuration selector")?;
    if before.is_some() {
        staged
            .as_file()
            .set_permissions(std::fs::symlink_metadata(path)?.permissions())?;
    }
    staged.write_all(after.as_bytes())?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    Ok(PreparedUpdate {
        path: path.to_path_buf(),
        before,
        after,
        staged,
        _lock: lock,
    })
}

pub(crate) struct PreparedUpdate {
    path: PathBuf,
    before: Option<String>,
    after: String,
    staged: tempfile::NamedTempFile,
    _lock: std::fs::File,
}

impl PreparedUpdate {
    pub(crate) fn commit(self) -> anyhow::Result<()> {
        if let Some(before) = self.before {
            regular_writable(&self.path)?;
            anyhow::ensure!(
                read_regular(&self.path)? == before,
                "configuration changed while preparing the update; nothing was written"
            );
            if before == self.after {
                return Ok(());
            }
            self.staged
                .persist(&self.path)
                .map_err(|e| e.error)
                .context("cannot atomically select theme")?;
        } else {
            // No-clobber even against a writer that ignores .skills.lock.
            self.staged
                .persist_noclobber(&self.path)
                .map_err(|e| e.error)
                .context(
                    "configuration appeared while preparing the update; nothing was overwritten",
                )?;
        }
        sync_parent(&self.path);
        Ok(())
    }
}

/// Directory sync failures after publication cannot be rolled back safely.
pub(crate) fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(error) = std::fs::File::open(parent).and_then(|directory| directory.sync_all())
    {
        tracing::warn!(target: "zevria::settings", %error, path = %path.display(), "file committed but parent directory sync failed");
    }
}
