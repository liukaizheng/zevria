//! Global named themes. Lock order is always theme store → configuration.
//! Theme publication precedes selection; a failed selector commit never deletes
//! a saved theme. No runtime generator or renderer state belongs in this module.
use std::{
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use zevria_theme::{ThemeDefinition, validate_theme};

use crate::{
    config::{DEFAULT_CONFIG, ThemeSelector},
    settings,
};

const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

pub fn validate_name(name: &str) -> anyhow::Result<()> {
    let first = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    anyhow::ensure!(
        (1..=64).contains(&name.len())
            && name.bytes().next().is_some_and(first)
            && name.bytes().all(|c| first(c) || c == b'_' || c == b'-'),
        "invalid theme name {name:?}: use 1–64 lowercase ASCII letters/digits, underscores or hyphens, beginning with a letter or digit (no path or .toml extension)"
    );
    Ok(())
}

pub struct ThemeStore {
    root: PathBuf,
}

impl ThemeStore {
    pub fn global() -> anyhow::Result<Self> {
        Ok(Self {
            root: zevria_foundation::config::zevria_dir()?.join("themes"),
        })
    }

    fn path(&self, name: &str) -> anyhow::Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(format!("{name}.toml")))
    }

    fn check_root(&self) -> anyhow::Result<()> {
        let metadata = std::fs::symlink_metadata(&self.root)
            .with_context(|| format!("cannot read theme directory {}", self.root.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "theme directory must be a real directory, not a symlink: {}",
            self.root.display()
        );
        Ok(())
    }

    fn create_root(&self) -> anyhow::Result<()> {
        match std::fs::symlink_metadata(&self.root) {
            Ok(_) => self.check_root()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.root)
                    .context("cannot create global theme directory")?;
                self.check_root()?;
            }
            Err(e) => return Err(e.into()),
        }
        anyhow::ensure!(
            !std::fs::metadata(&self.root)?.permissions().readonly(),
            "theme directory is read-only"
        );
        Ok(())
    }

    fn check_config_location(&self, config: &Path) -> anyhow::Result<()> {
        // A config inside this reserved directory could otherwise replace a
        // shared theme or the stable lock inode during the selector commit.
        let root = self.root.canonicalize()?;
        let absolute = std::path::absolute(config)?;
        for ancestor in absolute.ancestors() {
            match ancestor.canonicalize() {
                Ok(resolved) => {
                    anyhow::ensure!(
                        !resolved.starts_with(&root),
                        "configuration must be outside the reserved theme directory"
                    );
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("cannot resolve configuration location"),
            }
        }
        anyhow::bail!("cannot resolve configuration location")
    }

    pub fn load(&self, name: &str) -> anyhow::Result<ThemeDefinition> {
        let path = self.path(name)?;
        self.read_document(&path).with_context(|| format!("cannot load selected theme {name:?} at {}; regenerate with another name or run `zevria theme reset`", path.display()))
    }

    fn read_document(&self, path: &Path) -> anyhow::Result<ThemeDefinition> {
        self.check_root()?;
        let metadata =
            std::fs::symlink_metadata(path).context("theme file is missing or inaccessible")?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "theme file must be regular, not a symlink or special file"
        );
        let mut options = OpenOptions::new();
        options.read(true);
        no_follow(&mut options);
        let file = options.open(path).context("cannot open theme file")?;
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file(), "theme file must be regular");
        anyhow::ensure!(
            metadata.len() <= MAX_DOCUMENT_BYTES,
            "theme document exceeds 64 KiB"
        );
        let mut contents = String::new();
        file.take(MAX_DOCUMENT_BYTES + 1)
            .read_to_string(&mut contents)
            .context("theme document must be UTF-8")?;
        anyhow::ensure!(
            contents.len() as u64 <= MAX_DOCUMENT_BYTES,
            "theme document exceeds 64 KiB"
        );
        let definition: ThemeDefinition =
            toml::from_str(&contents).context("malformed theme document")?;
        validate_theme(&definition).context("invalid theme palette")?;
        Ok(definition)
    }

    fn lock(&self) -> anyhow::Result<File> {
        let path = self.root.join(".themes.lock");
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "theme lock must be a regular file"
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        no_follow(&mut options);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let lock = options
            .open(&path)
            .context("cannot open theme store lock")?;
        anyhow::ensure!(
            lock.metadata()?.is_file(),
            "theme lock must be a regular file"
        );
        lock.try_lock()
            .context("theme store is busy in another process; retry")?;
        Ok(lock)
    }

    fn existing_identical(
        &self,
        path: &Path,
        definition: &ThemeDefinition,
    ) -> anyhow::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(_) => {
                let existing = self.read_document(path).with_context(|| {
                    format!(
                        "theme name collision at {}; existing file is invalid; choose another name",
                        path.display()
                    )
                })?;
                // Provenance and formatting do not affect semantic identity.
                anyhow::ensure!(
                    existing.schema_version == definition.schema_version
                        && existing.source_background == definition.source_background
                        && existing.palette == definition.palette,
                    "theme name collision at {}; a different theme already exists; choose another name",
                    path.display()
                );
                Ok(true)
            }
        }
    }

    pub fn save_and_select(
        &self,
        name: &str,
        definition: &ThemeDefinition,
        config: &Path,
    ) -> anyhow::Result<PathBuf> {
        self.save_and_select_with(name, definition, config, || Ok(()), || Ok(()))
    }

    fn save_and_select_with(
        &self,
        name: &str,
        definition: &ThemeDefinition,
        config: &Path,
        before_publication: impl FnOnce() -> anyhow::Result<()>,
        after_publication: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<PathBuf> {
        let path = self.path(name)?;
        validate_theme(definition)?;
        let serialized = toml::to_string_pretty(definition)?;
        anyhow::ensure!(
            serialized.len() as u64 <= MAX_DOCUMENT_BYTES,
            "theme document exceeds 64 KiB"
        );
        let roundtrip: ThemeDefinition = toml::from_str(&serialized)?;
        validate_theme(&roundtrip)?;
        anyhow::ensure!(
            &roundtrip == definition,
            "theme serialization changed the definition"
        );
        self.create_root()?;
        self.check_config_location(config)?;
        let _theme_lock = self.lock()?;
        self.existing_identical(&path, definition)?;
        // Prepare and sync BOTH files before either public file changes. Holding
        // this transaction also excludes skill/model writers through selection.
        let selector = settings::prepare_create_or_update(config, DEFAULT_CONFIG, |before| {
            select_document(before, Some(name))
        })?;
        let mut staged =
            tempfile::NamedTempFile::new_in(&self.root).context("cannot stage theme")?;
        staged.write_all(serialized.as_bytes())?;
        staged.flush()?;
        staged.as_file().sync_all()?; // NamedTempFile uses private permissions.
        before_publication()?;
        self.check_root()?;
        if !self.existing_identical(&path, definition)? {
            match staged.persist_noclobber(&path) {
                Ok(_) => settings::sync_parent(&path),
                Err(error) => {
                    // A non-cooperating writer may have won the no-clobber race.
                    if !self.existing_identical(&path, definition)? {
                        return Err(error.error)
                            .context("cannot publish theme; no selection was changed");
                    }
                }
            }
        }
        // Read the published bytes, not just our stage, before making them active.
        let commit = || -> anyhow::Result<()> {
            anyhow::ensure!(
                self.existing_identical(&path, definition)?,
                "saved theme disappeared before selection"
            );
            after_publication()?;
            selector.commit()
        };
        commit().with_context(|| format!("theme {name:?} saved but not selected at {}; configuration {} was not updated by this command. Retry `zevria theme generate --name {name} --background '{}'`", path.display(), config.display(), definition.source_background))?;
        Ok(path)
    }
}

fn no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
}

fn select_document(before: &str, name: Option<&str>) -> anyhow::Result<String> {
    // Recovery intentionally parses unrelated settings and any old selector as
    // TOML data only. It never resolves the previous file or provider routing.
    let mut document = parse_config_document(before)?;
    document.remove("theme");
    if let Some(name) = name {
        validate_name(name)?;
        let mut table = toml_edit::Table::new();
        table.insert("name", toml_edit::value(name));
        document.insert("theme", toml_edit::Item::Table(table));
    }
    let after = document.to_string();
    let parsed: toml::Table = toml::from_str(&after)?;
    let selector: Option<ThemeSelector> = parsed
        .get("theme")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()?;
    anyhow::ensure!(
        selector.as_ref().map(|s| s.name.as_str()) == name,
        "selector serialization changed the selected name"
    );
    Ok(after)
}

fn parse_config_document(contents: &str) -> anyhow::Result<toml_edit::DocumentMut> {
    contents.parse().map_err(|error: toml_edit::TomlError| {
        // Do not echo source lines: configuration can contain credentials.
        anyhow::anyhow!(
            "configuration must be syntactically valid TOML at byte {}: {}",
            error.span().map_or(0, |span| span.start),
            error.message()
        )
    })
}

pub fn reset(config: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(config) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "configuration must be regular, not a symlink"
        ),
    }
    let contents = settings::read_regular(config)?;
    let parsed = parse_config_document(&contents)?;
    if !parsed.contains_key("theme") {
        return Ok(());
    }
    settings::transaction(config, |before| Ok((select_document(before, None)?, ())))
}

#[cfg(test)]
#[path = "theme_store_tests.rs"]
mod tests;
