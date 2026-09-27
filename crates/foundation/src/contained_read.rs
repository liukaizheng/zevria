//! Shared bounded, handle-relative regular-file reads. Callers own discovery
//! and alias policy. Unsupported platforms fail closed, never use ambient reads.
use std::{
    ffi::OsString,
    fs::{File, Metadata},
    io::Read as _,
    path::{Component, Path},
};

#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    InvalidPath,
    NonRegular,
    Oversized {
        cap: usize,
    },
    Changed,
    #[cfg(not(unix))]
    Unsupported,
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "contained file I/O failed: {error}"),
            Self::InvalidPath => f.write_str("invalid contained relative path"),
            Self::NonRegular => f.write_str(
                "resource is not a regular file; directories, devices and FIFOs are unsupported",
            ),
            Self::Oversized { cap } => write!(f, "resource exceeds its {cap}-byte cap"),
            Self::Changed => {
                f.write_str("resource changed during resolution or reading; restart the read")
            }
            #[cfg(not(unix))]
            Self::Unsupported => {
                f.write_str("safe handle-relative reads are unavailable on this platform")
            }
        }
    }
}
impl std::error::Error for ReadError {}
impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Only normal, nonempty components, independent of skill names or registries.
pub struct RelativePath(Vec<OsString>);
impl RelativePath {
    pub fn new(path: &Path) -> Result<Self, ReadError> {
        let parts = path
            .components()
            .map(|part| match part {
                Component::Normal(part) => {
                    #[cfg(windows)]
                    crate::windows_io::validate_component(part)?;
                    Ok(part.to_os_string())
                }
                _ => Err(ReadError::InvalidPath),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if parts.is_empty() {
            return Err(ReadError::InvalidPath);
        }
        Ok(Self(parts))
    }
}

pub fn read_regular(file: File, cap: usize) -> Result<Vec<u8>, ReadError> {
    read_regular_with(file, cap, || {}, || {})
}

// Injection at metadata/read boundaries gives deterministic growth/rewrite tests.
fn read_regular_with(
    mut file: File,
    cap: usize,
    before_read: impl FnOnce(),
    after_read: impl FnOnce(),
) -> Result<Vec<u8>, ReadError> {
    let before = file_snapshot(&file)?;
    if !before.metadata.is_file() {
        return Err(ReadError::NonRegular);
    }
    if before.metadata.len() > cap as u64 {
        return Err(ReadError::Oversized { cap });
    }
    before_read();
    let mut bytes = Vec::new();
    (&mut file)
        .take((cap as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(ReadError::Oversized { cap });
    }
    after_read();
    if !before.same_version(&file_snapshot(&file)?) || bytes.len() as u64 != before.metadata.len() {
        return Err(ReadError::Changed);
    }
    Ok(bytes)
}

/// Metadata is a race detector, not a transactional snapshot. It cannot detect
/// every concurrent same-size rewrite (including restored/coarse timestamps).
pub fn same_version(before: &Metadata, after: &Metadata) -> Result<bool, ReadError> {
    Ok(metadata_identity(before)? == metadata_identity(after)?
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok())
}
/// Identity and versions always come from owned handles, never path metadata.
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub metadata: Metadata,
    pub identity: String,
    pub links: u64,
}
impl FileSnapshot {
    pub fn same_version(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.metadata.len() == other.metadata.len()
            && self.metadata.modified().ok() == other.metadata.modified().ok()
    }
}
pub fn file_snapshot(file: &File) -> Result<FileSnapshot, ReadError> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileSnapshot {
            identity: metadata_identity(&metadata)?,
            links: metadata.nlink(),
            metadata,
        })
    }
    #[cfg(windows)]
    {
        let info = crate::windows_io::info(file)?;
        Ok(FileSnapshot {
            identity: crate::windows_io::identity(file)?,
            links: info.nNumberOfLinks as u64,
            metadata,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(ReadError::Unsupported)
    }
}
pub fn file_identity(file: &File) -> Result<String, ReadError> {
    Ok(file_snapshot(file)?.identity)
}
#[cfg(unix)]
pub fn metadata_identity(metadata: &Metadata) -> Result<String, ReadError> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
pub fn metadata_identity(_metadata: &Metadata) -> Result<String, ReadError> {
    Err(ReadError::Unsupported)
}

#[cfg(unix)]
pub struct OpenedRoot(File);
#[cfg(unix)]
impl OpenedRoot {
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        use std::os::unix::fs::OpenOptionsExt as _;
        Ok(Self(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
                .open(path)?,
        ))
    }
    /// Open every component without following symlinks, starting at the filesystem
    /// root. Unlike `open`, this also rejects symlinked ancestors.
    pub fn open_absolute(path: &Path) -> Result<Self, ReadError> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        let relative =
            RelativePath::new(path.strip_prefix("/").map_err(|_| ReadError::InvalidPath)?)?;
        let mut root = Self::open(Path::new("/"))?;
        for part in relative.0 {
            let part =
                std::ffi::CString::new(part.as_bytes()).map_err(|_| ReadError::InvalidPath)?;
            // SAFETY: root owns a live directory and part is one validated component.
            let fd = unsafe {
                libc::openat(
                    root.0.as_raw_fd(),
                    part.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: successful openat returned a fresh owned descriptor.
            root = Self(unsafe { File::from_raw_fd(fd) });
        }
        Ok(root)
    }
    pub fn metadata(&self) -> Result<Metadata, ReadError> {
        Ok(self.0.metadata()?)
    }
    pub fn open_file(&self, relative: &RelativePath) -> Result<File, ReadError> {
        use std::{
            ffi::CString,
            os::{
                fd::{AsRawFd as _, FromRawFd as _},
                unix::ffi::OsStrExt as _,
            },
        };
        let mut directory = self.0.try_clone()?;
        for (index, part) in relative.0.iter().enumerate() {
            let final_component = index + 1 == relative.0.len();
            let part = CString::new(part.as_bytes()).map_err(|_| ReadError::InvalidPath)?;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if final_component {
                    0
                } else {
                    libc::O_DIRECTORY
                };
            // SAFETY: directory is an owned live descriptor and part is a NUL-
            // terminated validated single component. Adopt a successful fd once.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), part.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: successful openat returned a fresh owned descriptor.
            directory = unsafe { File::from_raw_fd(fd) };
        }
        if !directory.metadata()?.is_file() {
            return Err(ReadError::NonRegular);
        }
        Ok(directory)
    }
}
#[cfg(windows)]
pub struct OpenedRoot(File);
#[cfg(windows)]
impl OpenedRoot {
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        Self::open_absolute(path)
    }
    pub fn open_absolute(path: &Path) -> Result<Self, ReadError> {
        Ok(Self(crate::windows_io::open_directory(path)?))
    }
    pub fn metadata(&self) -> Result<Metadata, ReadError> {
        Ok(self.0.metadata()?)
    }
    pub fn open_file(&self, relative: &RelativePath) -> Result<File, ReadError> {
        let mut file = self.0.try_clone()?;
        for (index, part) in relative.0.iter().enumerate() {
            file =
                crate::windows_io::open_relative(&file, part, index + 1 < relative.0.len(), false)?;
        }
        Ok(file)
    }
}
#[cfg(any(unix, windows))]
impl OpenedRoot {
    pub fn identity(&self) -> Result<String, ReadError> {
        file_identity(&self.0)
    }
}
#[cfg(not(any(unix, windows)))]
pub struct OpenedRoot;
#[cfg(not(any(unix, windows)))]
impl OpenedRoot {
    pub fn identity(&self) -> Result<String, ReadError> {
        Err(ReadError::Unsupported)
    }
    pub fn open_absolute(_path: &Path) -> Result<Self, ReadError> {
        Err(ReadError::Unsupported)
    }
    pub fn open(_path: &Path) -> Result<Self, ReadError> {
        Err(ReadError::Unsupported)
    }
    pub fn metadata(&self) -> Result<Metadata, ReadError> {
        Err(ReadError::Unsupported)
    }
    pub fn open_file(&self, relative: &RelativePath) -> Result<File, ReadError> {
        let _ = &relative.0;
        Err(ReadError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contained_read_bounds_growth_and_detects_read_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"four").unwrap();
        assert!(matches!(
            read_regular_with(
                File::open(&path).unwrap(),
                4,
                || std::fs::write(&path, b"fives").unwrap(),
                || {}
            ),
            Err(ReadError::Oversized { .. })
        ));
        std::fs::write(&path, b"four").unwrap();
        assert!(matches!(
            read_regular_with(
                File::open(&path).unwrap(),
                4,
                || {},
                || std::fs::write(&path, b"x").unwrap()
            ),
            Err(ReadError::Changed)
        ));
        std::fs::write(&path, b"four").unwrap();
        assert_eq!(
            read_regular(File::open(&path).unwrap(), 4).unwrap(),
            b"four"
        );
        let modified = File::open(&path)
            .unwrap()
            .metadata()
            .unwrap()
            .modified()
            .unwrap();
        assert!(matches!(
            read_regular_with(
                File::open(&path).unwrap(),
                4,
                || {},
                || File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(modified + std::time::Duration::from_secs(5))
                    .unwrap()
            ),
            Err(ReadError::Changed)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn contained_read_rejects_replaced_components_and_invalid_paths() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        let root = OpenedRoot::open(dir.path()).unwrap();
        symlink(outside.path(), dir.path().join("alias")).unwrap();
        assert!(
            root.open_file(&RelativePath::new(Path::new("alias/secret")).unwrap())
                .is_err()
        );
        for path in ["", ".", "..", "../secret", "/secret"] {
            assert!(RelativePath::new(Path::new(path)).is_err());
        }
    }
}
