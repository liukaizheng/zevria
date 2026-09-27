//! Small safe boundary around Windows handle-relative opens. No canonicalize-
//! then-open fallback: each component is opened from an owned directory handle.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
    ptr,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{RtlNtStatusToDosError, UNICODE_STRING},
        Storage::FileSystem::{self as fs, BY_HANDLE_FILE_INFORMATION},
        System::IO::IO_STATUS_BLOCK,
    },
};

pub fn validate_component(part: &OsStr) -> io::Result<()> {
    let text = part
        .to_str()
        .ok_or_else(|| io::Error::other("protected Windows paths must be Unicode"))?;
    let base = text.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (base.len() == 4
        && (base.starts_with("COM") || base.starts_with("LPT"))
        && base.as_bytes()[3].is_ascii_digit());
    if text.is_empty()
        || matches!(text, "." | "..")
        || text.ends_with(['.', ' '])
        || text
            .chars()
            .any(|c| c.is_control() || ":/\\<>\"|?*".contains(c))
        || reserved
    {
        return Err(io::Error::other(
            "unsupported protected Windows path component (traversal, stream, device, or ambiguous name)",
        ));
    }
    Ok(())
}

pub fn info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    if unsafe { fs::GetFileType(file.as_raw_handle()) } != fs::FILE_TYPE_DISK {
        return Err(io::Error::other(
            "protected reads require ordinary disk files",
        ));
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { fs::GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & fs::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "reparse points are unsupported in protected Windows reads",
        ));
    }
    Ok(info)
}

/// Use the full 128-bit file ID (including on ReFS), not a path or truncated
/// BY_HANDLE_FILE_INFORMATION file index. Unsupported identity queries fail closed.
pub fn identity(file: &File) -> io::Result<String> {
    let mut id: fs::FILE_ID_INFO = unsafe { std::mem::zeroed() };
    if unsafe {
        fs::GetFileInformationByHandleEx(
            file.as_raw_handle(),
            fs::FileIdInfo,
            &mut id as *mut _ as _,
            size_of::<fs::FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut identity = format!("{}:", id.VolumeSerialNumber);
    use std::fmt::Write;
    for byte in id.FileId.Identifier {
        write!(&mut identity, "{byte:02x}").expect("writing to String");
    }
    Ok(identity)
}

fn disk_parts(path: &Path) -> io::Result<(u8, Vec<std::ffi::OsString>)> {
    let mut parts = path.components();
    let drive = match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
            _ => {
                return Err(io::Error::other(
                    "protected reads support only local drive paths, not UNC/device paths",
                ));
            }
        },
        _ => {
            return Err(io::Error::other(
                "protected reads require a drive-absolute path",
            ));
        }
    };
    if parts.next() != Some(Component::RootDir) {
        return Err(io::Error::other(
            "protected reads require a drive-absolute path",
        ));
    }
    // components() normalizes interior '.'; reject raw traversal explicitly.
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    if raw
        .split(|c| *c == b'\\' as u16 || *c == b'/' as u16)
        .any(|p| p == [46] || p == [46, 46])
    {
        return Err(io::Error::other(
            "traversal is unsupported in protected paths",
        ));
    }
    let parts = parts
        .map(|part| {
            let Component::Normal(part) = part else {
                return Err(io::Error::other("invalid protected path"));
            };
            validate_component(part)?;
            Ok(part.to_os_string())
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok((drive.to_ascii_uppercase(), parts))
}

/// Normalize only the drive-prefix spelling. This performs no filesystem
/// resolution, so it cannot erase a reparse component before a protected open.
pub fn normalize_disk_path(path: &Path) -> io::Result<std::path::PathBuf> {
    let (drive, parts) = disk_parts(path)?;
    let mut path = std::path::PathBuf::from(format!("\\\\?\\{}:\\", drive as char));
    path.extend(parts);
    Ok(path)
}

/// Validate a startup directory without resolving filesystem aliases. The
/// returned lexical path is not an authority token: consumers must still use
/// handle-relative opens. In particular, never canonicalize it after this check,
/// which could erase a reparse point introduced by a concurrent replacement.
pub fn checked_directory_path(path: &Path) -> io::Result<std::path::PathBuf> {
    checked_directory_path_with(path, || {})
}

fn checked_directory_path_with(
    path: &Path,
    after_check: impl FnOnce(),
) -> io::Result<std::path::PathBuf> {
    let path = normalize_disk_path(path)?;
    open_directory(&path)?;
    after_check();
    Ok(path)
}

pub fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let (drive, parts) = disk_parts(path)?;
    let root = format!("{}:\\", drive as char);
    let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
    let kind = unsafe { fs::GetDriveTypeW(wide.as_ptr()) };
    // DRIVE_REMOVABLE=2, DRIVE_FIXED=3. Mapped network drives fail closed too.
    if kind != 2 && kind != 3 {
        return Err(io::Error::other(
            "protected reads require a local fixed or removable drive",
        ));
    }
    let mut directory = File::options()
        .read(true)
        .custom_flags(fs::FILE_FLAG_BACKUP_SEMANTICS | fs::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root)?;
    info(&directory)?;
    let mut filesystem = [0u16; 64];
    if unsafe {
        fs::GetVolumeInformationByHandleW(
            directory.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let filesystem = String::from_utf16_lossy(
        &filesystem[..filesystem
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(filesystem.len())],
    );
    if !matches!(filesystem.as_str(), "NTFS" | "ReFS") {
        return Err(io::Error::other(format!(
            "protected Windows reads do not support filesystem {filesystem:?}; use NTFS/ReFS or WSL"
        )));
    }
    for part in parts {
        directory = open_relative(&directory, &part, true, false)?;
    }
    Ok(directory)
}

pub fn open_relative(
    directory: &File,
    name: &OsStr,
    is_directory: bool,
    writable_lock: bool,
) -> io::Result<File> {
    open_relative_inner(directory, name, is_directory, writable_lock, writable_lock)
}
fn open_relative_inner(
    directory: &File,
    name: &OsStr,
    is_directory: bool,
    writable_lock: bool,
    create: bool,
) -> io::Result<File> {
    validate_component(name)?;
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let length =
        u16::try_from(wide.len() * 2).map_err(|_| io::Error::other("path component too long"))?;
    let mut name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle(),
        ObjectName: &mut name,
        Attributes: 0x40 | 0x1000,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null_mut(),
    };
    let mut status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle = ptr::null_mut();
    // SAFETY: directory is owned and live; name is a single validated component;
    // all buffers outlive the synchronous call. Successful handles are adopted once.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            fs::FILE_GENERIC_READ
                | if writable_lock {
                    fs::FILE_GENERIC_WRITE
                } else {
                    0
                },
            &attributes,
            &mut status,
            ptr::null(),
            0,
            fs::FILE_SHARE_READ
                | fs::FILE_SHARE_WRITE
                | if writable_lock {
                    0
                } else {
                    fs::FILE_SHARE_DELETE
                },
            if create { FILE_OPEN_IF } else { FILE_OPEN },
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if is_directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let metadata = info(&file)?;
    if (metadata.dwFileAttributes & fs::FILE_ATTRIBUTE_DIRECTORY != 0) != is_directory {
        return Err(io::Error::other(
            "protected path has an unexpected file type",
        ));
    }
    Ok(file)
}

pub fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    let path = std::path::absolute(path)?;
    let parent = open_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("lock has no parent"))?,
    )?;
    open_relative_inner(
        &parent,
        path.file_name()
            .ok_or_else(|| io::Error::other("lock has no name"))?,
        false,
        true,
        create,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_and_verbatim_prefixes_normalize_without_resolving_aliases() {
        assert_eq!(
            normalize_disk_path(Path::new(r"c:\work space\file")).unwrap(),
            normalize_disk_path(Path::new(r"\\?\C:\work space\file")).unwrap()
        );
        for path in [r"C:\a\..\b", r"C:\a\.\b", r"\\?\GLOBALROOT\Device\Disk"] {
            assert!(normalize_disk_path(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn rejects_ambiguous_components_and_non_drive_roots() {
        for name in [
            "..",
            ".",
            "x:stream",
            "NUL",
            "con.txt",
            "trailing.",
            "space ",
            "x/y",
            "x\\y",
        ] {
            assert!(validate_component(OsStr::new(name)).is_err(), "{name}");
        }
        for path in [r"\\server\share", r"\\.\C:\", r"C:relative", r"\rooted"] {
            assert!(open_directory(Path::new(path)).is_err(), "{path}");
        }
    }
    #[test]
    fn junctions_are_rejected_even_after_a_directory_was_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let inside = temp.path().join("inside");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(inside.join("file"), "inside").unwrap();
        std::fs::write(outside.join("file"), "outside secret").unwrap();
        let pinned = open_directory(temp.path()).unwrap();
        let checked = checked_directory_path_with(&inside, || {
            std::fs::rename(&inside, temp.path().join("parked")).unwrap();
            // Junction creation does not require Developer Mode/symlink privilege.
            // cmd is a test-fixture utility, never an agent command backend.
            let status = std::process::Command::new(
                crate::windows_process::system_executable("cmd.exe").unwrap(),
            )
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&inside)
            .arg(&outside)
            .output()
            .unwrap();
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
        })
        .unwrap();
        // A canonicalize-after-check implementation would redirect this name to
        // outside. Retaining the lexical name lets later protected opens reject it.
        assert_eq!(checked, normalize_disk_path(&inside).unwrap());
        assert!(open_relative(&pinned, OsStr::new("inside"), true, false).is_err());
        assert!(open_directory(&inside).is_err());
        assert!(open_directory(&checked).is_err());
        assert!(checked_directory_path(&inside).is_err());
        assert!(
            open_directory(&inside.canonicalize().unwrap()).is_ok(),
            "a separately supplied real directory remains usable"
        );
        std::fs::remove_dir(&inside).unwrap();
    }

    #[test]
    fn locks_are_no_follow_and_exclusive_across_handles() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.lock");
        let first = open_lock(&path, true).unwrap();
        first.try_lock().unwrap();
        let second = open_lock(&path, false).unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
        assert!(open_lock(&temp.path().join("absent"), false).is_err());
    }

    #[test]
    fn ordinary_and_verbatim_disk_paths_are_handle_relative() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file"), "body").unwrap();
        for path in [
            temp.path().to_path_buf(),
            temp.path().canonicalize().unwrap(),
        ] {
            let root = open_directory(&path).unwrap();
            let file = open_relative(&root, OsStr::new("file"), false, false).unwrap();
            assert_eq!(info(&file).unwrap().nNumberOfLinks, 1);
            std::fs::hard_link(temp.path().join("file"), temp.path().join("alias")).unwrap();
            let alias = open_relative(&root, OsStr::new("alias"), false, false).unwrap();
            assert_eq!(
                info(&file).unwrap().nFileIndexLow,
                info(&alias).unwrap().nFileIndexLow
            );
            assert_eq!(identity(&file).unwrap(), identity(&alias).unwrap());
            assert_eq!(info(&alias).unwrap().nNumberOfLinks, 2);
            std::fs::remove_file(temp.path().join("alias")).unwrap();
        }
    }
}
