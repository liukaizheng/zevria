# Release packages and maintainer procedure

[Release packages](../.github/workflows/release.yml) builds, tests, and packages
three native targets. A version-tag push can create a **draft** GitHub Release
only after its own complete build/test matrix and bundle assembly succeed.
Publication is always a separate, manual maintainer action. An independent green
`test.yml` run cannot substitute for these release checks.

## Downloads and platform scope

For package version `<version>`, the release has exactly four workflow-managed
assets:

| Download | Native build/test baseline | Contents at archive root |
| --- | --- | --- |
| `zevria-v<version>-x86_64-pc-windows-msvc.zip` | Windows x64, MSVC, `windows-2022` | `zevria.exe` |
| `zevria-v<version>-x86_64-unknown-linux-gnu.tar.gz` | Linux x64, GNU, `ubuntu-22.04` | `zevria` |
| `zevria-v<version>-aarch64-apple-darwin.tar.gz` | Apple Silicon macOS, `macos-15` | `zevria` |
| `SHA256SUMS` | SHA-256 of the three archives, sorted by basename | Plain text |

These runner labels define the acceptance baseline, not a promise of compatibility
with every OS release. Linux is validated against Ubuntu 22.04's GNU environment;
Alpine/musl and older distributions are not claimed. There is no Intel macOS,
Linux ARM64, or Windows ARM64 package. These are **unsigned and unnotarized**
standalone executables, not installers. There is no package-manager publication.
OS security policy may block an unsigned executable; use your organization's
review process rather than assuming a checksum is a signature.

Builds use default production features. In particular, neither `cache-diagnostics`
nor `--all-features` is enabled for the distributed executable. See the
[diagnostic-feature warning](cache-diagnostics.md#enable-explicitly). Versioned
runners and one exact Rust version per run improve consistency but do not promise
bit-for-bit reproducibility. Runner images and downloaded dependencies/tools can
still change between runs.

### Verify and install a download

Download the archive for your native environment and `SHA256SUMS` from the same
published release. GitHub's automatically generated source archives are not the
executable packages. Verify before extracting or running the binary.

For Linux or macOS, in the download directory (replace this example version):

```sh
version=0.0.1
# Linux x64; use aarch64-apple-darwin on Apple Silicon macOS.
archive="zevria-v${version}-x86_64-unknown-linux-gnu.tar.gz"
awk -v name="$archive" '$2 == name' SHA256SUMS > selected.sha256
# On macOS, replace sha256sum --check with shasum -a 256 --check.
# Chaining prevents extraction/execution after a failed checksum check.
test "$(wc -l < selected.sha256 | tr -d ' ')" = 1 &&
  sha256sum --check selected.sha256 &&
  tar -xzf "$archive" &&
  ./zevria --version
```

For Windows, in PowerShell:

```powershell
$version = '0.0.1'
$archive = "zevria-v$version-x86_64-pc-windows-msvc.zip"
$lines = @(Get-Content SHA256SUMS | Where-Object { $_.EndsWith("  $archive") })
if ($lines.Count -ne 1) { throw 'Missing or duplicate checksum entry' }
$expected = ($lines[0] -split '  ', 2)[0]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash
if ($actual -ine $expected) { throw 'Checksum mismatch' }
Expand-Archive -LiteralPath $archive -DestinationPath zevria-package -ErrorAction Stop
.\zevria-package\zevria.exe --version
```

If you downloaded all three archives, `sha256sum --check SHA256SUMS` (Linux) or
`shasum -a 256 --check SHA256SUMS` (macOS) verifies the entire bundle. Checksums
detect corruption or mismatched downloads; they are not independent proof of
publisher identity. After verification, place the executable in a directory on
your user `PATH`. Unix tar archives preserve executable permission.

### Dependencies are not bundled

- Keep **RTK** on the selected runtime's `PATH`; Zevria's built-in
  [command conventions](instructions/command-conventions.md) require it for
  model-driven commands. The unrelated crates.io package named `rtk` is not the
  required tool.
- Native Windows model-driven commands require **Git for Windows Bash**, even
  when Zevria is launched from PowerShell. `ZEVRIA_GIT_BASH` can select a verified
  Git Bash executable. Neither Bash nor RTK is bundled with these downloads.
- Windows automatic mode prefers a ready WSL environment. WSL needs a **separate,
  same-version Linux Zevria** plus Linux RTK and its other prerequisites; downloading
  `zevria.exe` does not provide that installation. Use the Linux x64 archive only
  in a compatible GNU distribution. See [Windows setup](windows.md) for the
  two-install procedure, configuration separation, and real-machine acceptance
  checklist. Use `--runtime native` to explicitly select the Windows package.
- Interactive/model-driven use still needs a suitable terminal and configured
  provider/model roles. See [Getting started](../README.md#getting-started).
  Help, version, and the native offline skills listing need no live provider.

## Maintainer prerequisites and release preparation

1. Ensure GitHub Actions is enabled and repository policy allows the release job's
   `GITHUB_TOKEN` to write repository contents. No personal access token or model
   provider credentials are needed. All other jobs have read-only contents
   permission, and actions are pinned to full commit SHAs with version comments.
2. Review and **commit the entire intended source and `Cargo.lock`**. Include the
   workflow and all required platform/launcher changes; an untracked local file
   or dirty worktree is not part of a tag build. This workflow does not stage,
   commit, bump versions, or repair the lockfile for you.
3. Align the `zevria` Cargo package version (currently inherited from
   `[workspace.package]`) and the desired tag. The tag must be exactly `v` plus
   the metadata version: for example, `0.0.1` requires `v0.0.1` and
   `0.2.0-rc.1` requires `v0.2.0-rc.1`. Cargo-valid SemVer prereleases are supported
   and marked prerelease, but still remain drafts. A pushed `v*` tag with a
   different or malformed version fails before the native matrix.
4. Prefer a successful manual build-only run of that exact source first. Ensure
   the [Windows real-machine checklist](windows.md#troubleshooting-and-acceptance)
   has appropriate evidence; automated native tests and mocked WSL probes do not
   establish interactive Windows terminal, clipboard, or real WSL acceptance.
5. After review and authorization, create and push a version tag pointing at the
   intended committed source. Both lightweight and annotated tags are supported.
   For example, run these yourself after confirming version and `HEAD`:

   ```sh
   git tag -a v0.0.1 -m 'Zevria v0.0.1'
   git push origin v0.0.1
   ```

   Pushing the tag is the release mutation authorization. Do not move it to
   repair a failure; see retry guidance below.

## What the workflow gates

The dependency chain is `prepare → native build/test matrix → assemble → draft`.

- **Prepare:** checks out the triggering commit, reads locked Cargo metadata,
  validates the identity, resolves stable Rust once, and records the immutable
  source SHA, version, and exact Rust toolchain version. Every native job checks
  out that SHA and uses that exact toolchain and explicit target.
- **Native matrix:** sets up Node 22, Python 3.12, the RTK revision shared with
  `test.yml`, and native C/C++ tools (including MSVC and verified Git Bash on
  Windows). It runs locked workspace/all-target checks, builds the real debug
  executable required by ACP process fixtures, and runs locked workspace tests.
  Caches are separated by OS, target, Rust version, and lockfile.
- **Additional Linux gates:** formatting, the five explicit default-feature
  [provider-prefix release gates](transcript-performance.md#permanent-provider-prefix-release-gates),
  with nonempty test-filter checks, and a separate provider/app
  `cache-diagnostics` regression test invocation. These checks never enable the
  diagnostic feature on the subsequent production build.
- **Package validation:** builds only the production `zevria` executable with
  `--locked --release` and default features. Both the original and extracted
  executable must pass `--help`, the exact expected `--version`, and
  `--runtime native skills list --json`. Each smoke invocation set uses disposable
  HOME/USERPROFILE and workspace directories with inherited Zevria overrides
  removed. No interactive session, live model, or destructive cleanup is used.
- **Assembly:** downloads only this run's target artifacts, retains their separate
  directories to detect duplicate/unexpected files, requires all three exact
  archives and sidecar checksums, verifies every digest, and assembles a sorted
  `SHA256SUMS`. The final `zevria-v<version>-bundle` Actions artifact contains only
  the three archives plus that manifest. Target artifacts and the bundle have
  14-day retention (subject to repository policy).
- **Draft:** only a matching version-tag **push** can enter the write job. It
  rechecks the four-file bundle and remote tag's resolved commit before writing.
  Creation uses the existing tag with `--verify-tag`, an explicit title,
  generated release notes, and the source marker described below. It never
  creates a tag or publishes the draft.

The matrix does not fail fast: a failed platform does not cancel the other
platforms, but **any failure blocks release creation**. Per-ref concurrency uses
`cancel-in-progress: false`; a newer run must not interrupt an in-progress draft
upload. This is not a guarantee that every pending run will be queued forever.

### Windows linker selection and preflight

Git for Windows includes an unrelated `usr\bin\link.exe`. Release commands run
in Git Bash, whose `PATH` can find that utility instead of Visual Studio's MSVC
linker. Release run `36301066140`, job `108568689470`, failed this way while
linking dependency build scripts during RTK installation, before Windows workspace
builds or tests had started.

The Windows setup initializes the native x64 Visual Studio developer shell,
derives `bin\Hostx64\x64` from `VCToolsInstallDir`, and requires both `cl.exe` and
`link.exe` there. It exports the resolved, unquoted absolute linker path as
`CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` through `GITHUB_ENV` for subsequent
steps, while retaining the compiler/library environment and Git Bash verification.
It does not rely on PATH ordering, a fixed Visual Studio edition/toolset version,
global `RUSTFLAGS`, or nightly Rust options.

Before the pinned RTK installation, a Windows-only Bash preflight:

- Validates the inherited linker setting against the expected MSVC file and tests
  rejection of missing, empty, relative, quoted, nonexistent, directory, and Git
  utility paths. It reports the configured linker and PATH-resolved `link.exe`
  separately; finding Git's utility on PATH is not itself a failure.
- Builds a temporary dependency-free executable with a real `build.rs` twice:
  once in Cargo's implicit-host mode (as used by RTK installation), and once with
  `--target x86_64-pc-windows-msvc` (as used by workspace release commands). Both
  builds are offline with separate clean output directories. Each executable must
  print a known marker supplied by the build script.
- Puts Git's `usr\bin` first on PATH **only in the probe subprocess environment**
  to exercise the original collision. Subprocesses have timeouts and preserve
  compiler diagnostics on failure; temporary files are cleaned up without touching
  repository manifests or `Cargo.lock`.

This preflight does not replace RTK installation/behavior probes or any workspace,
provider-prefix, production-build, extracted-binary, or checksum release gates.

## Manual build-only dry run

Once `release.yml` is present on the repository's default branch, open **Actions →
Release packages → Run workflow**, select the intended ref, and run it. Alternatively,
with an authenticated GitHub CLI, explicitly dispatch a branch or tag:

```sh
gh workflow run release.yml --ref YOUR_BRANCH_OR_TAG
```

Dispatch is **always build-only**, including `--ref v0.0.1`. It runs the same native
checks, builds, extraction smoke tests, and bundle assembly, but cannot create or
update any GitHub Release. Download `zevria-v<version>-bundle` from that run's
Actions artifacts and inspect its four files/checksums. A dry run does not promote
its artifacts into a later tag run; the tag run independently builds its source.

Do not treat a workflow merely being added or statically linted as hosted
acceptance. Require all three native jobs and assembly to succeed, including the
Windows linker preflight, RTK installation, workspace tests, production build,
extracted-binary smoke tests, and checksum verification. Confirm the draft job was
skipped and dispatch created no release. Record the tested source commit and run
ID; local linting or mocked probes do not establish Windows acceptance. Real draft
creation needs a separately authorized version-tag push.

## Retry and recovery

- A failure before the draft job creates no release. Inspect the failing job;
  do not drop the platform, bypass tests, or weaken provider-prefix gates to make
  it pass. Native dependency or existing platform-test failures are release
  blockers, not permission for an unrelated application rewrite.
- For a transient failure, use GitHub's **Re-run failed jobs** or **Re-run all
  jobs** on the original run. The same run can reuse already successful target
  artifacts; rerun targets replace only their target-specific artifact. Assembly
  revalidates the complete inventory. If artifacts have expired, rerun all jobs.
  Reruns retain the original commit SHA and ref: rerunning a failed tag workflow
  does **not** pick up a later workflow commit. To test corrected source/workflow
  code, use a new build-only dispatch selecting the corrected ref after separate
  push/dispatch authorization, and verify its recorded source SHA.
- A partial create/upload failure may leave an **incomplete, unpublished draft**.
  Rerun the failed draft job after fixing the operational problem and while the
  bundle is retained. A full rerun may resolve a newer stable Rust version;
  record the new run summary. Exact Rust consistency is per run, not across runs.
- A retry requires the existing release to be a draft, have the expected
  prerelease flag, and contain exactly one matching source marker in its notes:

  ```text
  <!-- zevria-release-source: FULL_40_CHARACTER_SOURCE_SHA -->
  ```

  Preserve this marker when editing notes. Retries do **not** regenerate or
  overwrite maintainer-edited notes/title, and do not remove unrelated assets.
  They replace only the three expected archive filenames and `SHA256SUMS`.
- Published releases, missing/conflicting source markers, mismatched prerelease
  flags, moved/deleted tags, duplicate tag releases, and API/authentication failures
  stop the job. Only a successful paginated release listing with no matching
  release permits creation; a failed lookup is never treated as absence.
- Uploads are not atomic. `gh release upload --clobber` deletes an existing asset
  before uploading its replacement. A failed retry can therefore leave missing
  files. **Never publish while a run/retry is in progress or after a failed
  upload.** Wait for the final complete-asset verification and a successful job.
- If source or workflow code must change, commit the fix and choose a new aligned
  package version/tag. Do not move `v0.0.1` or another existing release tag to repair
  a failure, or reuse a published version. Version selection, tagging, and
  publication are separate maintainer decisions, not part of a workflow fix.
  An unmarked/conflicting draft requires explicit maintainer investigation; the
  workflow will not claim ownership or silently overwrite it. Manually dispatching
  the tag is not a release-repair shortcut because dispatch cannot write releases.

## Review and manually publish

After the entire tag workflow succeeds:

1. Open the draft linked in the run summary. Confirm its source SHA and intended
   tag against the committed release source, and inspect the exact Rust version,
   target matrix, and checksums in the summaries.
2. Verify all four assets, download/check their hashes, and review the generated
   release notes. Add compatibility caveats and any manual platform acceptance
   evidence. Keep the source marker intact for any remaining retry.
3. Check that the draft/prerelease status is appropriate and that no release run
   or retry is active. Resolve incomplete uploads before proceeding.
4. Publish manually in GitHub only after that review. The workflow never performs
   this final step and refuses to alter the release once published.

For workflow changes, run `actionlint .github/workflows/release.yml` (with ShellCheck
available), inspect permissions/action pins and dependencies, and test identity,
archive inventory/checksum, and draft retry branches using local fixtures or a
mocked CLI. Never create a real tag or release solely to test failure handling
without explicit authorization.
