//! All destructive and interleaving tests use fresh temporary directories.
use super::*;
use std::{
    cell::RefCell,
    sync::{Arc, mpsc},
    thread,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Event {
    CoordinatorBlocked,
    ContenderClosed,
    BeforeRemove,
}

type Hook = Box<dyn FnMut(Event) + Send>;
thread_local! {
    // Per-thread hooks cannot interfere with concurrently running tests. They
    // pause actual production paths at protocol boundaries, never with sleeps.
    static HOOK: RefCell<Option<Hook>> = RefCell::new(None);
}

pub(super) fn checkpoint(event: Event) {
    HOOK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(event);
        }
    });
}

fn pause_once(event: Event) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>, Hook) {
    let (arrived, arrival) = mpsc::sync_channel(0);
    let (proceed, permission) = mpsc::sync_channel(0);
    let mut once = true;
    let hook = Box::new(move |actual| {
        if actual == event && std::mem::take(&mut once) {
            arrived.send(()).unwrap();
            permission.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    });
    (arrival, proceed, hook)
}

fn arrived(receiver: &mpsc::Receiver<()>) {
    receiver.recv_timeout(Duration::from_secs(10)).unwrap();
}

#[test]
fn last_arc_release_removes_sidecar_but_never_transcript_or_coordinator() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.jsonl");
    let original = b"substantive history, unchanged\n";
    fs::write(&path, original).unwrap();
    let lease = Arc::new(RootSessionLease::acquire(&path).unwrap());
    let engine = lease.clone();
    let sidecar = path.with_extension("jsonl.lock");
    assert!(sidecar.exists());
    let error = RootSessionLease::acquire(&path).unwrap_err();
    assert!(error.to_string().contains("held by another"));
    assert!(
        sidecar.exists(),
        "failed contenders must not unlink the owner"
    );
    drop(lease);
    assert!(sidecar.exists());
    assert!(RootSessionLease::acquire(&path).is_err());
    drop(engine);
    assert!(!sidecar.exists());
    assert!(directory.path().join(COORDINATOR_NAME).is_file());
    assert_eq!(fs::read(&path).unwrap(), original);
    drop(RootSessionLease::acquire(&path).unwrap());
    assert!(!sidecar.exists());
}

#[test]
fn release_closes_the_lock_handle_before_attempting_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("closed.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    let lease = RootSessionLease::acquire(&path).unwrap();
    let probe_path = sidecar.clone();
    let (checked, check) = mpsc::channel();
    HOOK.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move |event| {
            if event == Event::BeforeRemove {
                // This callback runs inside the remover's coordinator section.
                // A separate open must already be able to acquire the OS lock.
                let file = open_regular(&probe_path, false).unwrap();
                file.try_lock()
                    .expect("the lease handle must already be closed");
                drop(file);
                checked.send(()).unwrap();
            }
        }))
    });
    drop(lease);
    HOOK.with_borrow_mut(|hook| *hook = None);
    arrived(&check);
    assert!(!sidecar.exists());
}

#[test]
fn sweep_removes_only_inactive_eligible_sidecars_and_never_recurses() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for name in ["orphan.jsonl.lock", "saved.jsonl.lock"] {
        fs::write(root.join(name), []).unwrap();
    }
    fs::write(root.join("saved.jsonl"), b"saved transcript\n").unwrap();
    for name in [
        ".jsonl.lock",
        "custom.lock",
        "suffix.jsonl.lock.bak",
        ".skills.lock",
        ".gitignore",
    ] {
        fs::write(root.join(name), []).unwrap();
    }
    fs::write(root.join("customized.jsonl.lock"), b"keep me").unwrap();
    fs::write(root.join(COORDINATOR_NAME), b"coordinator is not truncated").unwrap();
    fs::create_dir(root.join("directory.jsonl.lock")).unwrap();
    fs::write(root.join("directory.jsonl.lock/nested.jsonl.lock"), []).unwrap();
    let lease = RootSessionLease::acquire(&root.join("live.jsonl")).unwrap();
    assert!(!root.join("live.jsonl").exists());
    sweep_inactive(root);
    for name in ["orphan.jsonl.lock", "saved.jsonl.lock"] {
        assert!(!root.join(name).exists());
    }
    for name in [
        ".jsonl.lock",
        "custom.lock",
        "suffix.jsonl.lock.bak",
        ".skills.lock",
        ".gitignore",
        "live.jsonl.lock",
        "directory.jsonl.lock/nested.jsonl.lock",
    ] {
        assert!(root.join(name).exists(), "{name}");
    }
    assert_eq!(
        fs::read(root.join("customized.jsonl.lock")).unwrap(),
        b"keep me"
    );
    assert_eq!(
        fs::read(root.join(COORDINATOR_NAME)).unwrap(),
        b"coordinator is not truncated"
    );
    assert_eq!(
        fs::read(root.join("saved.jsonl")).unwrap(),
        b"saved transcript\n"
    );
    assert!(RootSessionLease::acquire(&root.join("live.jsonl")).is_err());
    drop(lease);
    assert!(!root.join("live.jsonl.lock").exists());
}

#[test]
fn sweep_revalidates_stale_candidates_and_never_creates_disappeared_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("candidate.jsonl.lock");
    fs::write(&path, []).unwrap();
    assert!(eligible_sidecar(&path).unwrap()); // Enumeration snapshot.
    fs::remove_file(&path).unwrap();
    sweep_candidate(&path).unwrap();
    assert!(!path.exists());
    fs::write(&path, b"customized after enumeration").unwrap();
    sweep_candidate(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"customized after enumeration");
    fs::write(&path, []).unwrap();
    let lease = RootSessionLease::acquire(&path.with_extension("")).unwrap();
    sweep_candidate(&path).unwrap();
    assert!(path.exists(), "an owner acquired since enumeration");
    drop(lease);
    assert!(!path.exists());
}

#[test]
fn release_preserves_nonempty_sidecars_without_truncating_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("customized.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    fs::write(&sidecar, b"custom lease data").unwrap();
    drop(RootSessionLease::acquire(&path).unwrap());
    sweep_inactive(directory.path());
    assert_eq!(fs::read(&sidecar).unwrap(), b"custom lease data");
    fs::write(&sidecar, []).unwrap();
    let lease = RootSessionLease::acquire(&path).unwrap();
    // Customize the owned handle, not a second competing locking identity.
    use std::io::Write as _;
    lease
        .file
        .as_ref()
        .unwrap()
        .write_all(b"changed during ownership")
        .unwrap();
    drop(lease);
    assert_eq!(fs::read(sidecar).unwrap(), b"changed during ownership");
}

#[test]
fn busy_coordinator_fails_closed_and_defers_release_without_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("held.jsonl");
    let lease = RootSessionLease::acquire(&path).unwrap();
    let coordinator =
        DirectoryCoordinator::acquire(&directory.path().join(COORDINATOR_NAME), Duration::ZERO)
            .unwrap();
    let other = directory.path().join("other.jsonl");
    let error = RootSessionLease::acquire(&other).unwrap_err();
    assert!(error.to_string().contains("coordinator") && error.to_string().contains("busy"));
    assert!(!other.exists());
    assert!(!other.with_extension("jsonl.lock").exists());
    // Drop must return even though this same thread still holds coordination.
    drop(lease);
    assert!(path.with_extension("jsonl.lock").exists());
    assert!(sweep_candidate(&path.with_extension("jsonl.lock")).is_err());
    drop(coordinator);
    sweep_inactive(directory.path());
    assert!(!path.with_extension("jsonl.lock").exists());
    drop(RootSessionLease::acquire(&path).unwrap());
}

#[test]
fn nonregular_paths_fail_closed_and_release_defers_when_coordinator_is_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    fs::create_dir(&sidecar).unwrap();
    let error = RootSessionLease::acquire(&path).unwrap_err();
    assert!(format!("{error:#}").contains("regular file"));
    assert!(!format!("{error:#}").contains("held by another"));
    assert!(sidecar.is_dir());
    let coordinator_path = directory.path().join(COORDINATOR_NAME);
    let other = directory.path().join("other.jsonl");
    let lease = RootSessionLease::acquire(&other).unwrap();
    // Script external damage solely to exercise fail-safe handling. This is
    // not a supported way of maintaining a real workspace coordinator.
    fs::remove_file(&coordinator_path).unwrap();
    fs::create_dir(&coordinator_path).unwrap();
    assert!(RootSessionLease::acquire(&other).is_err());
    drop(lease);
    assert!(other.with_extension("jsonl.lock").exists());
    sweep_inactive(directory.path());
    assert!(other.with_extension("jsonl.lock").exists());
    fs::remove_dir(&coordinator_path).unwrap();
    sweep_inactive(directory.path());
    assert!(!other.with_extension("jsonl.lock").exists());
}

#[cfg(unix)]
#[test]
fn unix_modes_symlinks_and_special_files_are_protected() {
    use std::os::unix::{
        fs::{PermissionsExt as _, symlink},
        net::UnixListener,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let lease = RootSessionLease::acquire(&root.join("mode.jsonl")).unwrap();
    for name in [COORDINATOR_NAME, "mode.jsonl.lock"] {
        assert_eq!(
            fs::metadata(root.join(name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(root.join("target"), []).unwrap();
    symlink(root.join("target"), root.join("link.jsonl.lock")).unwrap();
    symlink(root.join("missing"), root.join("dangling.jsonl.lock")).unwrap();
    let _socket = UnixListener::bind(root.join("socket.jsonl.lock")).unwrap();
    let fifo = std::ffi::CString::new(root.join("fifo.jsonl.lock").as_os_str().as_encoded_bytes())
        .unwrap();
    // SAFETY: fifo is a NUL-terminated path in this test's temporary directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for name in ["link", "dangling", "socket", "fifo"] {
        assert!(RootSessionLease::acquire(&root.join(format!("{name}.jsonl"))).is_err());
    }
    sweep_inactive(root);
    for name in ["link", "dangling", "socket", "fifo"] {
        assert!(fs::symlink_metadata(root.join(format!("{name}.jsonl.lock"))).is_ok());
    }
    assert!(root.join("target").exists());
    drop(lease);
    fs::remove_file(root.join(COORDINATOR_NAME)).unwrap();
    symlink(root.join("target"), root.join(COORDINATOR_NAME)).unwrap();
    assert!(RootSessionLease::acquire(&root.join("mode.jsonl")).is_err());
    assert_eq!(fs::read(root.join("target")).unwrap(), b"");
}

#[cfg(unix)]
#[test]
fn permission_and_deletion_errors_leave_history_and_defer_reclamation() {
    use std::os::unix::fs::PermissionsExt as _;
    // Unix root bypasses mode-bit denial; that cannot exercise this fixture.
    // SAFETY: geteuid has no preconditions or side effects.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("permission fixture requires a non-root Unix account");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("permissions.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    fs::write(&path, b"history\n").unwrap();
    let lease = RootSessionLease::acquire(&path).unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o500)).unwrap();
    drop(lease); // Opening coordinator works; directory unlink permission fails.
    assert!(sidecar.exists());
    sweep_inactive(root);
    assert!(sidecar.exists());
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o400)).unwrap();
    let error = RootSessionLease::acquire(&path).unwrap_err();
    assert!(format!("{error:#}").contains("cannot open session lease"));
    assert!(!format!("{error:#}").contains("held by another"));
    fs::write(root.join("removable.jsonl.lock"), []).unwrap();
    sweep_inactive(root);
    assert!(sidecar.exists());
    assert!(
        !root.join("removable.jsonl.lock").exists(),
        "maintenance continues after errors"
    );
    fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600)).unwrap();
    // Existing, unlocked files never prevent later ownership by mere presence.
    let lease = RootSessionLease::acquire(&path).unwrap();
    fs::set_permissions(
        root.join(COORDINATOR_NAME),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    drop(lease);
    assert!(sidecar.exists());
    assert!(RootSessionLease::acquire(&path).is_err());
    fs::set_permissions(
        root.join(COORDINATOR_NAME),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    sweep_inactive(root);
    assert!(!sidecar.exists());
    assert_eq!(fs::read(&path).unwrap(), b"history\n");
}

fn removal_cannot_strand_a_contender_on_the_old_identity(sweep: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("race.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    let lease = if sweep {
        fs::write(&sidecar, []).unwrap();
        None
    } else {
        Some(RootSessionLease::acquire(&path).unwrap())
    };
    let (removing, remove, removal_hook) = pause_once(Event::BeforeRemove);
    let removal_path = sidecar.clone();
    let remover = thread::spawn(move || {
        HOOK.with_borrow_mut(|hook| *hook = Some(removal_hook));
        if sweep {
            sweep_candidate(&removal_path).unwrap();
        } else {
            drop(lease);
        }
    });
    arrived(&removing); // Old handle closed, path still exists, coordinator held.
    assert!(sidecar.exists());
    let (blocked, retry, contender_hook) = pause_once(Event::CoordinatorBlocked);
    let (owned, ownership) = mpsc::sync_channel(0);
    let (release, released) = mpsc::sync_channel(0);
    let contender_path = path.clone();
    let contender = thread::spawn(move || {
        HOOK.with_borrow_mut(|hook| *hook = Some(contender_hook));
        let lease = RootSessionLease::acquire(&contender_path).unwrap();
        owned.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(lease);
    });
    arrived(&blocked); // Deterministically attempted acquisition before unlink.
    remove.send(()).unwrap();
    remover.join().unwrap();
    assert!(!sidecar.exists());
    retry.send(()).unwrap();
    arrived(&ownership);
    assert!(sidecar.exists());
    // If the contender opened before coordination, it would own the removed
    // inode and this second acquire would incorrectly succeed on a new one.
    assert!(RootSessionLease::acquire(&path).is_err());
    sweep_inactive(directory.path());
    assert!(sidecar.exists());
    release.send(()).unwrap();
    contender.join().unwrap();
    assert!(!sidecar.exists());
}

#[test]
fn acquisition_racing_final_release_cannot_own_a_deleted_identity() {
    removal_cannot_strand_a_contender_on_the_old_identity(false);
}

#[test]
fn acquisition_racing_sweep_cannot_own_a_deleted_identity() {
    removal_cannot_strand_a_contender_on_the_old_identity(true);
}

#[test]
fn failed_contender_closes_before_release_or_sweep_can_delete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("race.jsonl");
    let sidecar = path.with_extension("jsonl.lock");
    let owner = RootSessionLease::acquire(&path).unwrap();
    let (closed, finish, hook) = pause_once(Event::ContenderClosed);
    let contender_path = path.clone();
    let contender = thread::spawn(move || {
        HOOK.with_borrow_mut(|installed| *installed = Some(hook));
        assert!(RootSessionLease::acquire(&contender_path).is_err());
    });
    arrived(&closed);
    assert!(sidecar.exists());
    drop(owner); // Contender still holds coordinator, so cleanup must defer.
    assert!(sidecar.exists());
    finish.send(()).unwrap();
    contender.join().unwrap();
    sweep_inactive(directory.path());
    assert!(!sidecar.exists());
    let new_owner = RootSessionLease::acquire(&path).unwrap();
    assert!(RootSessionLease::acquire(&path).is_err());
    drop(new_owner);
    assert!(!sidecar.exists());
}
