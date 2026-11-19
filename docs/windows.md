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

| Input or state | Windows | Linux/macOS |
| --- | --- | --- |
| Project guidance | `<workspace>/AGENTS.md` | Same shared input |
| Project skills | `<workspace>/.zevria/skills` | Same shared input |
| State root | `<workspace>/.zevria/windows/` | `<workspace>/.zevria/` |
| State directories | `sessions`, `subsessions`, `plans`, `agent-runs`, `ensemble-sessions` | Same layout under its root |

Continue, resume, ACP listing, leases, recovery, and `clean` use only the local
platform's workspace state. Windows `zevria clean` leaves prior Linux history
and shared project inputs untouched. No previous configuration or history is
imported or migrated. Stop active sessions and workers before cleanup. Startup
reports the platform and effective configuration and state paths on stderr,
not in ACP stdout or the provider's stable application prefix.

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
  Windows does not search or copy earlier Linux state or configuration.

Windows CI covers noninteractive regressions for Git Bash validation, native
home and state isolation, protected inputs, and ACP/process cleanup. Automated
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
- [ ] Verify Windows continue/resume/list/clean isolation, protected reads and
  reparse/hard-link rejection, resource pagination, atomic settings, and leases.
