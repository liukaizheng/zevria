# Offline installer fixtures

Run from the repository root:

```sh
/bin/bash -n install.sh tests/install/transport.sh
shellcheck install.sh tests/install/transport.sh
INSTALL_TEST_BASH=/bin/bash python3 tests/install/test_unix.py
python3 tests/install/test_release_contract.py
```

On Windows, run **each** PowerShell edition separately:

```powershell
powershell -NoProfile -File tests/install/test_helpers.ps1
powershell -NoProfile -File tests/install/test_windows.ps1
pwsh -NoProfile -File tests/install/test_helpers.ps1
pwsh -NoProfile -File tests/install/test_windows.ps1
```

`test_helpers.ps1` also runs on non-Windows PowerShell: parser, SemVer, actual
HTTP redirect/retry/length policy with injected response objects, SHA-256, ZIP
inventory and CRC/length corruption tests. This does **not** establish Windows
acceptance. The native suite uses Windows ACLs and file locks, the OS architecture
query, home/path resolution, a real .NET Framework fixture executable, and mocked
registry writes. It requires the Windows .NET Framework `csc.exe` shipped with the
CI host; this is a test prerequisite, not an installer prerequisite.

The Unix suite runs actual installer helpers, tar/checksum verification,
transaction/rollback logic, and profile editing in disposable homes. It substitutes
fixture transport and, for cross-platform Linux metadata scenarios, target
selection. Target-detection helpers are independently tested with uname/libc/
Rosetta fixtures. Actual zsh/fish profile execution runs when those shells are
available; CI installs fish explicitly. The macOS lane uses system Bash 3.2.

`transport.sh` and `transport.ps1` assert exact GitHub tag-specific download URLs,
then serve local fixtures. Production installers have no fixture URL, insecure
mirror, checksum bypass, or test-mode flags. Sourcing/dot-sourcing the production
script defines helpers but does not install anything. Low-level helpers are test
seams, not supported public installation APIs.

Fresh-package release acceptance uses:

```sh
python3 tests/install/smoke.py ARCHIVE ARCHIVE.sha256 VERSION
```

This verifies the job's sidecar, serves it as `SHA256SUMS`, installs the **current
job's archive**, and runs exact `--version`, `--help`, and native offline skills
JSON smoke checks in temporary homes/workspaces. Windows runs both PowerShell
editions; no registry/profile writes occur. The release-contract fixture executes
the workflow's actual assembly code with synthetic data to enforce exactly three
archives plus `SHA256SUMS`; it is not native package or publication evidence.

Linux launcher tests execute `launcher_linux.sh`, the actual shared POSIX source,
against isolated homes and real Linux ELF fixture programs. macOS/Windows do not
run those Linux-only tests. `launcher_mock_tests.rs` remains separate transport
coverage. Real WSL acceptance stays in the opt-in provisioned-host workflow:
provide an existing distro, an installer-produced Windows executable at the tested
version, and a matching installed **custom** Linux root with its locator already
published. The lane checks those inputs and real handoff; it never installs,
upgrades, shuts down, or reconfigures WSL. Also complete the default-root and
interactive checks in `docs/windows.md` before claiming full Windows/WSL acceptance.
