//! Private environment exporter and one bounded Unix login-shell capture.
use super::ShellEnvironment;
use std::{borrow::Cow, ffi::OsStr, io::Write};

#[cfg(unix)]
use super::ShellSpec;
#[cfg(unix)]
use std::{ffi::OsString, path::Path};
#[cfg(unix)]
use tokio::sync::OnceCell;

#[cfg(unix)]
static PREPARED: OnceCell<Option<ShellSpec>> = OnceCell::const_new();

fn bytes(value: &OsStr) -> Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(value.as_bytes())
    }
    #[cfg(not(unix))]
    {
        Cow::Owned(value.to_string_lossy().as_bytes().to_vec())
    }
}

fn write_environment(
    writer: &mut impl Write,
    marker: &OsStr,
    values: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> std::io::Result<()> {
    let marker = bytes(marker);
    if marker.is_empty() || marker.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid environment marker",
        ));
    }
    writer.write_all(&marker)?;
    writer.write_all(&[0])?;
    for (key, value) in values {
        writer.write_all(&bytes(&key))?;
        writer.write_all(b"=")?;
        writer.write_all(&bytes(&value))?;
        writer.write_all(&[0])?;
    }
    Ok(())
}

/// Private `--__print-env <marker>` entry point. Run before application setup;
/// Unix keys/values retain native bytes, including non-UTF-8 values.
pub fn print_environment(marker: &OsStr) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());
    write_environment(&mut writer, marker, std::env::vars_os())?;
    writer.flush()
}

/// Prepare once before `frozen()`. Failure is deliberately nonfatal and logged
/// only once; the unchanged per-call login shell remains the fallback.
pub async fn prepare_environment(mode: ShellEnvironment) {
    #[cfg(unix)]
    prepare_once(&PREPARED, mode, || async {
        anyhow::ensure!(
            super::FROZEN_SHELL.get().is_none(),
            "command shell was frozen before environment capture"
        );
        capture(&super::login_shell_path(), &std::env::current_exe()?).await
    })
    .await;
    #[cfg(not(unix))]
    let _ = mode;
}

#[cfg(unix)]
pub(super) fn captured() -> Option<ShellSpec> {
    PREPARED.get().and_then(Option::as_ref).cloned()
}

#[cfg(unix)]
async fn prepare_once<F, Fut>(
    state: &OnceCell<Option<ShellSpec>>,
    mode: ShellEnvironment,
    capture: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ShellSpec>>,
{
    state.get_or_init(|| async {
        if mode == ShellEnvironment::Login { return None; }
        match capture().await {
            Ok(shell) => Some(shell),
            Err(error) => {
                // Never log captured stdout/stderr or environment values.
                tracing::warn!(error = %error, "login environment capture failed; retaining the per-call login shell");
                None
            }
        }
    }).await;
}

#[cfg(unix)]
async fn capture(executable: &Path, exporter: &Path) -> anyhow::Result<ShellSpec> {
    let marker = format!("__zevria_environment_{}", uuid::Uuid::new_v4());
    let source = if executable.file_name().is_some_and(|name| name == "fish") {
        "exec \"$argv[1]\" --__print-env \"$argv[2]\""
    } else {
        "exec \"$0\" --__print-env \"$1\""
    };
    let output = crate::process::bounded_output(
        tokio::process::Command::new(executable)
            .args(["-lc", source])
            .arg(exporter)
            .arg(&marker),
        std::time::Duration::from_secs(8),
        1024 * 1024,
    )
    .await?;
    anyhow::ensure!(
        output.status.success(),
        "login environment exporter exited with {}",
        output.status
    );
    Ok(ShellSpec {
        executable: executable.to_path_buf(),
        args: vec!["-c".into()],
        env: parse(&output.stdout, marker.as_bytes())?,
        remove_env: vec![],
        clear_env: true,
    })
}

#[cfg(unix)]
fn parse(output: &[u8], marker: &[u8]) -> anyhow::Result<Vec<(OsString, OsString)>> {
    use std::os::unix::ffi::OsStringExt;
    let mut boundary = marker.to_vec();
    boundary.push(0);
    let start = output
        .windows(boundary.len())
        .position(|bytes| bytes == boundary)
        .ok_or_else(|| anyhow::anyhow!("login environment exporter marker was missing"))?;
    let payload = &output[start + boundary.len()..];
    anyhow::ensure!(
        payload.is_empty() || payload.ends_with(&[0]),
        "incomplete login environment export"
    );
    let mut values = Vec::new();
    for entry in payload
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let separator = entry
            .iter()
            .position(|byte| *byte == b'=')
            .filter(|position| *position > 0)
            .ok_or_else(|| anyhow::anyhow!("malformed login environment entry"))?;
        let (key, value) = (&entry[..separator], &entry[separator + 1..]);
        if [b"PWD".as_slice(), b"OLDPWD", b"SHLVL", b"_"].contains(&key) {
            continue;
        }
        values.push((
            OsString::from_vec(key.to_vec()),
            OsString::from_vec(value.to_vec()),
        ));
    }
    Ok(values)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        os::unix::{ffi::OsStringExt, fs::PermissionsExt},
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn script(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn exporter_preserves_native_values_and_parser_discards_noise_and_shell_bookkeeping() {
        let values = vec![
            ("A".into(), "value=with\nnewlines".into()),
            ("B".into(), OsString::from_vec(vec![0xff])),
            ("EMPTY".into(), "".into()),
            ("PWD".into(), "wrong".into()),
            ("OLDPWD".into(), "wrong".into()),
            ("SHLVL".into(), "99".into()),
            ("_".into(), "wrong".into()),
        ];
        let mut output = b"noisy profile without a newline".to_vec();
        write_environment(&mut output, OsStr::new("marker"), values.clone()).unwrap();
        assert_eq!(parse(&output, b"marker").unwrap(), values[..3]);
        for bad in [
            b"no marker".as_slice(),
            b"marker\0A=no terminator",
            b"marker\0invalid\0",
            b"marker\0=value\0",
        ] {
            assert!(parse(bad, b"marker").is_err());
        }
    }

    #[tokio::test]
    async fn noisy_profile_is_captured_once_and_commands_use_a_cleared_nonlogin_environment() {
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("fake shell");
        let exporter = directory.path().join("exporter's path with spaces");
        let counter = directory.path().join("counter");
        let quoted_counter = format!("'{}'", counter.to_str().unwrap().replace('\'', "'\\''"));
        script(
            &shell,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = '-lc' ]; then\n  printf 'profile noise without newline'\n  printf x >> {quoted_counter}\n  export CAPTURE_TEST='profile value'\n  shift\n  exec /bin/sh -c \"$@\"\nfi\nexec /bin/sh \"$@\"\n"
            ),
        );
        script(
            &exporter,
            "#!/bin/sh\n[ \"$1\" = '--__print-env' ] || exit 2\nprintf '%s\\000' \"$2\" \"CAPTURE_TEST=$CAPTURE_TEST\" 'PATH=/usr/bin:/bin' 'PWD=wrong' 'OLDPWD=wrong' 'SHLVL=99' '_=wrong'\n",
        );
        let state = OnceCell::new();
        for _ in 0..3 {
            prepare_once(&state, ShellEnvironment::Captured, || {
                capture(&shell, &exporter)
            })
            .await;
        }
        let spec = state.get().unwrap().as_ref().unwrap();
        for _ in 0..3 {
            let mut command = tokio::process::Command::new(&shell);
            command.env("UNINHERITED_CAPTURE_TEST", "must not leak");
            spec.apply(&mut command);
            command.arg("printf '%s:%s' \"$CAPTURE_TEST\" \"${UNINHERITED_CAPTURE_TEST-unset}\"");
            let output = crate::process::bounded_output(
                &mut command,
                std::time::Duration::from_secs(2),
                8192,
            )
            .await
            .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"profile value:unset");
        }
        assert_eq!(std::fs::read(&counter).unwrap(), b"x");
        assert!(spec.clear_env);
        assert_eq!(spec.args, vec![OsString::from("-c")]);
        assert!(
            !format!("{spec:?}").contains("profile value"),
            "Debug must not expose environment values"
        );
    }

    #[tokio::test]
    async fn login_mode_skips_capture_and_failure_is_not_retried() {
        let state = OnceCell::new();
        prepare_once(&state, ShellEnvironment::Login, || async {
            panic!("login must not capture")
        })
        .await;
        assert!(state.get().unwrap().is_none());
        let failed = OnceCell::new();
        let calls = AtomicUsize::new(0);
        for _ in 0..3 {
            prepare_once(&failed, ShellEnvironment::Captured, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("synthetic failure")
            })
            .await;
        }
        assert!(failed.get().unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
