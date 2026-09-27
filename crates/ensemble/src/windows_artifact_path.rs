//! Alias-aware Claude artifact confinement. Identity comes only from protected
//! opens; the returned path keeps the permitted root's spelling, not a resolved
//! path that could hide a reparse point from subsequent permission/snapshot checks.
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};
use zevria_foundation::windows_io;

struct Workspace {
    // Keep the boundary alive while comparing identities from the same walk that
    // opens the target. Independently reopening lexical ancestors is not evidence
    // about the ancestry of that target.
    _directory: File,
    identity: String,
}

impl Workspace {
    fn open(path: &Path) -> io::Result<Self> {
        let directory = windows_io::open_directory(path)?;
        let identity = windows_io::identity(&directory)?;
        Ok(Self {
            _directory: directory,
            identity,
        })
    }
}

struct Directory {
    path: PathBuf,
    directory: File,
    identity: String,
    // Only the workspace-local root may be absent before permission. Retain the
    // identity of its deepest existing ancestor plus the still-missing suffix.
    missing: PathBuf,
    in_workspace: bool,
}

impl Directory {
    fn open(path: &Path, workspace: &Workspace) -> io::Result<Self> {
        let path = windows_io::normalize_disk_path(path)?;
        let mut components = path.components();
        let mut root = PathBuf::from(components.next().expect("normalized drive").as_os_str());
        root.push(components.next().expect("normalized root").as_os_str());
        let mut directory = windows_io::open_directory(&root)?;
        let mut identity = windows_io::identity(&directory)?;
        let mut in_workspace = identity == workspace.identity;
        let mut missing = PathBuf::new();
        for component in components {
            if !missing.as_os_str().is_empty() {
                missing.push(component);
                continue;
            }
            match windows_io::open_relative(&directory, component.as_os_str(), true, false) {
                Ok(next) => {
                    identity = windows_io::identity(&next)?;
                    in_workspace |= identity == workspace.identity;
                    directory = next;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(component),
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            path,
            directory,
            identity,
            missing,
            in_workspace,
        })
    }

    fn exists(&self) -> bool {
        self.missing.as_os_str().is_empty()
    }

    fn same_location(&self, other: &Self) -> bool {
        self.identity == other.identity && self.missing == other.missing
    }
}

fn workspace_directory(
    directory: &Path,
    workspace_path: &Path,
    workspace: &Workspace,
) -> io::Result<Directory> {
    let expected = Directory::open(&workspace_path.join(".claude").join("plans"), workspace)?;
    let supplied = Directory::open(directory, workspace)?;
    if !supplied.same_location(&expected) {
        return Err(io::Error::other(format!(
            "Claude workspace plan artifact directory {} is not <workspace>/.claude/plans",
            directory.display()
        )));
    }
    Ok(supplied)
}

pub(super) fn validate_workspace_directory(directory: &Path, workspace: &Path) -> io::Result<()> {
    let boundary = Workspace::open(workspace)?;
    workspace_directory(directory, workspace, &boundary)?;
    Ok(())
}

pub(super) fn artifact_directory(requested: &Path, workspace: &Path) -> io::Result<PathBuf> {
    let boundary = Workspace::open(workspace)?;
    let requested = Directory::open(requested, &boundary)?;
    if requested.in_workspace {
        let permitted = Directory::open(&workspace.join(".claude").join("plans"), &boundary)?;
        if requested.same_location(&permitted) {
            return Ok(permitted.path);
        }
        return Err(io::Error::other(format!(
            "Claude plan artifact directory {} is inside the ensemble workspace but is not the permitted workspace-local .claude/plans directory",
            requested.path.display()
        )));
    }
    if !requested.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "failed to resolve the Claude plan artifact directory at {}",
                requested.path.display()
            ),
        ));
    }
    Ok(requested.path)
}

pub(super) fn validate_artifact_path(
    path: &Path,
    artifact_directory: &Path,
    workspace_artifact_directory: &Path,
    workspace: &Path,
) -> io::Result<PathBuf> {
    let path = windows_io::normalize_disk_path(path)?;
    if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
        return Err(io::Error::other(
            "Claude plan artifact must be a Markdown file",
        ));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Claude plan artifact path has no file name"))?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Claude plan artifact path has no parent directory"))?;
    let boundary = Workspace::open(workspace)?;
    let parent = Directory::open(parent, &boundary)?;
    let permitted = if parent.in_workspace {
        let permitted = workspace_directory(workspace_artifact_directory, workspace, &boundary)?;
        if !parent.same_location(&permitted) {
            return Err(io::Error::other(
                "Claude plan artifact workspace path must be a direct child of .claude/plans",
            ));
        }
        permitted
    } else {
        let permitted = Directory::open(artifact_directory, &boundary)?;
        // An alternate spelling must never turn another workspace directory into
        // an external artifact root, even when both spellings open the same file.
        if permitted.in_workspace || !permitted.exists() || !parent.same_location(&permitted) {
            return Err(io::Error::other(format!(
                "Claude Write target {} is not a direct child of either permitted plan artifact directory",
                path.display()
            )));
        }
        permitted
    };
    if parent.exists() {
        match windows_io::open_relative(&parent.directory, file_name, false, false) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(permitted.path.join(file_name))
}
