# Offline installer fixtures

Run from the repository root:

```sh
/bin/bash -n install.sh
/bin/bash -n tests/install/transport.sh
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
fixture transport and target selection; target detection is independently tested
with uname/libc/Rosetta fixtures. Linux scenarios cover default and custom roots,
unchanged legacy locators, executable replacement rollback, and retained backups
after failed recovery. Actual zsh/fish profile execution runs when those shells are
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
job's archive**, and runs exact `--version`, `--help`, and `skills list --json`
smoke checks in temporary homes/workspaces. Windows runs both PowerShell
editions; no registry/profile writes occur. The release-contract fixture executes
the workflow's actual assembly code with synthetic data to enforce exactly three
archives plus `SHA256SUMS`; it is not native package or publication evidence.

These installer checks are noninteractive. Use the [Windows checklist](../../docs/windows.md#troubleshooting-and-acceptance)
for real terminal and clipboard validation.
