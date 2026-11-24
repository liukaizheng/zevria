use std::{fs::File, io, path::Path};

/// Open a regular lock file without truncating it, following the final path
/// component if it is a link, or blocking on a special file.
pub fn open_lock_file(path: &Path, create: bool) -> io::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(not_regular()),
        Ok(_) => {}
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(not(windows))]
    let file = {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(create)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        options.open(path)?
    };
    #[cfg(windows)]
    let file = crate::windows_io::open_lock(path, create)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

fn not_regular() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "lock file must be a regular file",
    )
}
