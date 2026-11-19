# Windows setup

`zevria.exe` always runs as a native Windows process. **Git for Windows Bash** is
its only command-tool backend, whether you launch from PowerShell, cmd, Git Bash,
or an ACP client. Zevria does not start WSL or translate paths into Linux paths.
The packaged Windows target is **x86_64 MSVC**. Linux and macOS builds still run
directly with their existing shell behavior.

Remove `--runtime` (including `--runtime native`) and `--wsl-distro` from older
scripts and ACP configurations. Run `zevria` for the terminal UI or `zevria --acp`
for ACP. Help, version, and offline operations like `skills list --json`,
`theme`, and `clean` do not require Git Bash, RTK, or a configured model.

## Install Zevria

Use Windows PowerShell 5.1 or PowerShell 7. Download and inspect the installer
before running it, and substitute a published version for the example:

```powershell
Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/liukaizheng/zevria/main/install.ps1 -OutFile install.ps1
Get-Content -LiteralPath ./install.ps1
./install.ps1 -Version 0.0.1
```

See [installer trust, source pinning, and PATH setup](releases.md#standalone-installers).
Set an absolute `$env:ZEVRIA_INSTALL` for a custom executable root, or use
`-NoPathUpdate` to opt out of user and process PATH changes. No elevation is
required. A machine-PATH copy can still take priority in new terminals; verify
with the printed absolute path and resolve the conflicting entry yourself.

## Install Git Bash and RTK

Install **Git for Windows**, not Windows System32 Bash. Install native RTK with
its Windows package or binary:

```powershell
winget install rtk-ai.rtk
```

Alternatively, put the upstream Windows MSVC `rtk.exe` on your user PATH and
restart your terminal or editor. The unrelated crates.io package named `rtk` is
not the required tool. Zevria and its installer do not install these dependencies.

Discovery checks Git for Windows installation metadata, ordinary installation
locations, and PATH. An explicit executable override takes precedence and fails
clearly if invalid:

```powershell
$env:ZEVRIA_GIT_BASH = 'C:\Program Files\Git\bin\bash.exe'
Set-Location -LiteralPath 'C:\Users\you\Project with spaces'
zevria
```

Interactive and ACP startup validate Git Bash and RTK before loading model
configuration. The check uses the actual command environment. Zevria freezes one
shell specification per process, shared by the root and Explore and Build
children. Bash is noninteractive (`--noprofile --norc -c`); `BASH_ENV`, `ENV`, and
shell-option overrides are removed. Zevria prepends Git's Unix utilities and
binaries to the command child's PATH and retains the user's native development
tools without modifying the machine's PATH.

Each call starts in its configured workspace; a previous `cd` does not persist.
Use relative Bash paths when possible. For an absolute shell path, convert it
explicitly as needed:

```bash
rtk proxy cygpath -u 'C:\Users\you\Project with spaces'
rtk read README.md
rtk sed -n '1,40p' README.md
rtk git status
```

Use native Windows paths for structured file tools and ACP messages. Zevria does
not rewrite command strings or arbitrary configuration contents. Git Bash's
normal argument conversion for native tools still applies; use explicit `cygpath`
conversion or per-command MSYS settings when a particular tool requires it.

## Configuration, instructions, and history

A valid absolute `HOME` override wins. Without one, Windows uses `USERPROFILE`,
then `HOMEDRIVE`/`HOMEPATH`. Configuration, logging, global guidance, themes,
global skills, Claude handoff, and Git Bash `HOME` use that same resolver.
Configure providers in the Windows home. `ZEVRIA_CONFIG` can select a different
configuration file and its sibling `models.jsonc`. Embedded executable paths,
log directories, and provider addresses must be usable from Windows.

| Input or state | Location on Windows, Linux, and macOS |
| --- | --- |
| Project guidance | `<workspace>/AGENTS.md` |
| Project skills | `<workspace>/.zevria/skills/` |
| State root | `<workspace>/.zevria/` |
| State directories | `sessions/`, `subsessions/`, `plans/`, `agent-runs/`, `ensemble-sessions/` directly under the state root |

Continue, resume, ACP listing, leases, and crash recovery use the unified paths,
including history already present there. Sharing a directory layout does not
make simultaneous access across platforms or filesystems safe; it requires
compatible filesystem locking. Startup reports the platform and effective
configuration and state paths on stderr, not in ACP stdout or the provider's
stable application prefix.

`zevria clean` deletes the five directories above without confirmation, including
any data Linux/WSL or macOS stored in them. It leaves `.zevria/windows/`, project
skills and guidance, and other unlisted files alone. Stop all sessions and
workers using the workspace and back up any state you want to keep before
cleaning. See [offline workspace cleanup](architecture.md#offline-workspace-cleanup).

### Optional offline transfer of legacy Windows state

Earlier native Windows versions used `<workspace>/.zevria/windows/`. Zevria
no longer looks there for history or recovery, and does not migrate, merge, or
delete it. To use that state in the unified layout, transfer it manually:

1. Stop all sessions and workers using the workspace, including those in other
   frontends or running from another OS. Back up both layouts: the old
   `.zevria/windows/` subtree and any state directories already directly under
   `.zevria/`.
2. Check all five destinations before moving anything. Only if they are all
   absent, move whichever of `sessions/`, `subsessions/`, `plans/`, `agent-runs/`,
   and `ensemble-sessions/` exist under `.zevria/windows/` as one set to the same
   names directly under `.zevria/`. Leave other files and directories in place.
3. If any destination exists, keep both copies and choose which set to use. If
   you choose the legacy set, set the existing top-level state directories
   aside intact before transferring. Do not overwrite or merge directories.

Moving these directories does not convert old transcript formats or rewrite
OS-specific paths embedded in saved state. It does not change home configuration
or logs.

### Protected-read boundaries

Windows guidance, skill manifests/resources, and artifact snapshots use owned
handles and component-relative `NtCreateFile` opens, not canonicalize-then-open.
Startup and artifact paths retain lexical drive-prefix normalization so later
opens can still reject a concurrently substituted reparse point; a prior path
check is not permission to follow aliases. Identity, size/version, and hard-link
checks come from file handles. The backend accepts ordinary local drive paths and
verbatim disk paths on NTFS/ReFS, and rejects reparse traversal (including junctions
and aliases), network and mapped-network roots, device paths, streams, and
ambiguous components. Unsupported arrangements fail explicitly; there is no
unsafe read fallback. Unix contained-alias behavior is unchanged. File versions
detect observable changes, not every possible same-size/timestamp-restored
concurrent rewrite.

Settings and leases use no-follow handle opens and OS locking. Atomic replacement
uses a same-directory staged file. Windows does not claim Unix directory-fsync
crash durability. This protected-input boundary is **not an OS sandbox** for
commands or ordinary structured editing tools.

## ACP and process lifetime

Use `zevria --acp` with native Windows paths for session working directories and
file operations. Built-in ensemble workers run the current Zevria executable
with `--acp --ensemble-worker`. Configured ACP agents run through a private job
helper that preserves stdio and exit status, including Node and `npx` launchers
under Git Bash. Use a Bash-resolvable launcher or an explicit native executable
and argument vector. See [ACP agent setup](acp-agent.md).

Windows command and ACP workloads start suspended, acquire a kill-on-close Job
Object, and only then resume. Failed job setup terminates the suspended child.
Shell leader status is independent of job lifetime; ordinary shell exit also
kills background descendants before capture drains. Timeout, turn cancellation,
and worker-task abort preserve invocation-scoped cleanup. Unix process groups
remain unchanged.

## Troubleshooting and acceptance

- **Git Bash missing or invalid:** install Git for Windows or correct
  `ZEVRIA_GIT_BASH`. Do not use `Windows\System32\bash.exe`.
- **RTK missing:** verify `rtk --version` from Git Bash and restart the launcher
  after changing user PATH. Offline commands remain usable.
- **Install or PATH issue:** close a running Zevria if the executable is locked.
  Restart terminals if the installer ran in a child PowerShell process. Invoke
  the printed absolute path to bypass a shadowing command. To roll back, rerun
  the installer at the same root with an older, explicit version.
- **Skills or guidance unavailable:** move protected inputs to a supported local
  NTFS/ReFS path without junctions or other reparse points. Do not relax containment.
- **Missing history or settings:** check the effective paths printed at startup.
  Older Windows history may still be under `.zevria/windows/`; see the
  [optional offline transfer](#optional-offline-transfer-of-legacy-windows-state).
  Home configuration is not imported or copied.

Windows CI covers noninteractive regressions for Git Bash validation, native
home resolution, unified workspace paths, protected inputs, and ACP/process cleanup. Automated
checks do not exercise a live terminal or clipboard. On an actual Windows
machine, record these end-to-end checks before claiming interactive acceptance:

- [ ] Launch from a non-Bash terminal in a local workspace with spaces and
  Unicode; verify the working directory, native tools on PATH, and RTK
  `read`/`sed`, stdout/stderr, and nonzero command status.
- [ ] Verify terminal input and submission, text paste and image clipboard,
  Ctrl+C cancellation, and terminal restoration after exit.
- [ ] Open an ACP session using native paths; verify clean protocol stdout,
  built-in and external worker shutdown, and Node/npm descendant cleanup after
  exit, timeout, or cancellation.
- [ ] Verify continue/resume/list use unified workspace paths, and `clean`
  leaves legacy Windows data and other siblings untouched. Check protected reads
  and reparse/hard-link rejection, resource pagination, atomic settings, and leases.
