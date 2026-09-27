# Windows runtime setup

Initial acceptance targets are **Windows x86_64 MSVC and WSL 2**. The launcher,
native fallback, and test lanes are implemented; a build or mocked probe is not
proof of real Windows terminal/WSL acceptance. Use the checklist below before
claiming a release is validated. WSL 1 and other Windows architectures are not
part of the initial acceptance claim.

## Select the environment once

| Invocation from Windows | Selected runtime |
| --- | --- |
| `zevria` or `zevria --runtime auto` | Ready default WSL distribution; otherwise native, with an explanatory stderr notice |
| `zevria --runtime wsl` | Require ready WSL; never fall back |
| `zevria --runtime native` | Native; do not probe WSL |
| `zevria --acp` (including `--ensemble-worker`) | Native by default |
| `zevria --runtime wsl --acp` | WSL; the client must already understand Linux paths |
| Direct Linux/macOS launch | Local process, without Windows selection |

`--wsl-distro NAME` chooses exactly that distribution to probe. It does not scan
other distributions or change the user's default. In automatic mode, a missing
or unready selected distribution produces a diagnostic and native fallback;
combine it with `--runtime wsl` to require that distribution instead. Invalid
option syntax is always an error. With ACP, a distribution override requires
`--runtime wsl`. `--runtime native --wsl-distro NAME` is invalid.

Help, version, and the private machine-readable compatibility probe run before
configuration/logging/terminal initialization. Offline management operations need
no model/provider; native ones do not need Git Bash or RTK either.

Readiness requires more than `wsl.exe`: a usable selected distribution, an
accessible workspace, a compatible Linux executable, and the dependencies for
the requested operation. Probes use a trusted system `wsl.exe`, fixed bootstrap
source, bounded output and deadlines, and positional arguments. They perform no
installation or application configuration. The Linux bootstrap needs `/bin/sh`,
`timeout`, `readlink`, `od`, `tr`, `head`, `wc`, `grep`, and (for Windows drive
paths) `wslpath`. It never sources shell profiles.

**After handoff there is no native retry.** The Linux child's stdout, stderr,
terminal handles and exit status belong to that invocation. First-run config,
provider/network errors, and unsuccessful offline commands are not fallback
signals. The launcher never shuts down a distribution or copies credentials.

## WSL-first setup: two installations

1. Install the Windows binary in PowerShell, using a **published pinned version**
   (replace the example `0.0.1`). Inspect the downloaded script before running it:

   ```powershell
   Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/liukaizheng/zevria/main/install.ps1 -OutFile install.ps1
   Get-Content -LiteralPath ./install.ps1
   ./install.ps1 -Version 0.0.1
   ```

   Quick latest-stable installation is `irm https://raw.githubusercontent.com/liukaizheng/zevria/main/install.ps1 | iex`.
   The raw URL becomes available after the script reaches `main`. Review the
   [installer trust, source-pinning and PATH details](releases.md#standalone-installers).
   Set absolute `$env:ZEVRIA_INSTALL` for a custom Windows executable root, and
   use `-NoPathUpdate` to opt out of user/process PATH changes. No elevation is
   required. New terminals may still see a machine-PATH copy first; use the
   printed absolute path and repair shadowing explicitly.

2. Inspect WSL yourself (`wsl --list --verbose`). If it is not installed or has
   no distribution, follow Microsoft's WSL installation instructions. Zevria
   will not install a distribution, upgrade WSL, or change its default for you.
3. Enter the desired **x64 GNU/glibc** distribution (`wsl`, or
   `wsl --distribution NAME`). Install **the same binary version** there:

   ```sh
   curl -fsSL --proto '=https' --proto-redir '=https' -o install.sh https://raw.githubusercontent.com/liukaizheng/zevria/main/install.sh
   less install.sh
   bash install.sh 0.0.1
   # Custom root alternative (recorded even when PATH changes are disabled):
   # ZEVRIA_INSTALL="$HOME/Applications/Zevria" bash install.sh 0.0.1 --no-path-update
   ```

   Install Linux RTK separately using its upstream instructions. If you already
   have Rust, `cargo install --git https://github.com/rtk-ai/rtk --locked` is one
   option; the unrelated crates.io package named `rtk` is not the required tool.
   The installer does not install dependencies or configure providers.

   The launcher rejects Windows executables and shell wrappers as Linux Zevria
   companions. Application version and launcher compatibility revision must
   match. A discovery-only change does not bump that revision. **The install-root
   discovery below requires a newly built Windows launcher containing this
   change. Installing an older published binary does not retrofit its launcher.**
4. Configure providers/models in the Linux runtime's home. You may run Linux
   Zevria directly first to generate its setup templates. Windows credentials are
   not imported. Re-run explicitly if first-run setup exits after a handoff.
5. From a Windows terminal in your project, run `zevria --runtime wsl`. Once this
   works, ordinary `zevria` may use automatic selection.

Source-build alternative: use matching compatible checkouts and native Rust/build
prerequisites in both environments, then `cargo install --path crates/zevria --locked`
in each. No installer or launcher copies/upgrades the other runtime. If a locator
from an earlier binary installation remains, repair/remove it before expecting
Cargo discovery to win.

### Linux install-root discovery and repair

Every Linux installer run records its absolute root in the Linux home's
`.zevria/install-root`, including default roots and `--no-path-update` installs.
This file is bounded **data**, not shell source. A valid record selects precisely
`<recorded-root>/bin/zevria`; that bin precedes Cargo and other fallbacks. The
handoff stays bound to the exact executable selected by the successful probe.
Windows `ZEVRIA_INSTALL` is never a Linux discovery input.

Without a locator, search order is `~/.zevria/bin`, `~/.cargo/bin`, `~/.local/bin`,
`/usr/local/bin`, `/usr/bin`, `/bin`, then inherited PATH. Custom roots do not
require profile edits: WSL readiness and handoff never source Bash, zsh, or fish
profiles or evaluate locator text.

A malformed, unreadable, nonregular, linked/dangling or stale locator **fails WSL
readiness**, rather than silently running another Linux Zevria. Automatic mode
may still use its existing native fallback; explicit `--runtime wsl` fails. Repair
inside the selected distribution by rerunning the matching Linux installer with
the intended `ZEVRIA_INSTALL`. Or, after inspecting the record, remove only
`~/.zevria/install-root` to deliberately restore fallback discovery. Do not delete
the shared `.zevria` directory or credentials. An unavailable/non-ELF recorded
executable also requires repair; a version mismatch requires matching Windows and
Linux installations. These failures never trigger a native retry after handoff.

Normal drive paths, canonical verbatim drive paths, and `\\wsl.localhost\NAME\...`
(or `\\wsl$\NAME\...`) are recognized by the launcher. It deliberately translates
the startup directory, `skills validate <path>`, and an explicit `ZEVRIA_CONFIG`.
A WSL share for another distribution is rejected. General UNC/device paths and
alternate data streams are not mapped. Arguments are not interpolated into shell
source; spaces, Unicode, quotes, and metacharacters remain data.

Windows HOME/SHELL/ZEVRIA_INSTALL are not forwarded as Linux defaults, including
case-insensitive WSLENV entries with flags. Config and shell-injection overrides
are filtered too. Other intentional WSLENV bridges remain the user's responsibility.

## Native fallback: Git for Windows Bash and native RTK

Install **Git for Windows**, not the legacy Windows/WSL Bash launcher. Install RTK
with its documented Windows package or binary:

```powershell
winget install rtk-ai.rtk
```

Alternatively, use the upstream Windows MSVC release of `rtk.exe`, put it on your
user PATH, and restart the launching terminal. The unrelated crates.io package
named `rtk` is not the required tool. Zevria does not install these dependencies.

Discovery checks Git for Windows installation metadata, ordinary installation
locations and PATH. An explicit executable override wins and fails clearly if
invalid:

```powershell
$env:ZEVRIA_GIT_BASH = 'C:\Program Files\Git\bin\bash.exe'
zevria --runtime native
```

Starting from PowerShell, cmd, Windows Terminal or an ACP editor is supported by
the selection contract; **model commands still use Bash**. Native command tools
freeze one shell specification per process, shared by root, Explore and Build
children. Bash is noninteractive (`--noprofile --norc -c`); BASH_ENV/ENV and shell
option injection are removed. Zevria prepends Git's Unix utilities and Git binaries
to each command child's PATH and retains the user's native development-tool PATH.
It does not modify the machine's PATH. RTK readiness is checked in that actual
command environment before model-driven command execution.

Every call starts in its configured workspace; a prior `cd` does not persist.
Use relative Bash paths wherever possible. For an absolute shell path:

```bash
rtk proxy cygpath -u 'C:\Users\you\Project with spaces'
rtk read README.md
rtk sed -n '1,40p' README.md
rtk git status
```

Use native Windows paths for structured file tools. Command strings and arbitrary
configuration contents are never rewritten. Git Bash's normal argument conversion
for native development tools still applies; choose explicit `cygpath` conversion
or per-command MSYS settings when a particular tool requires it.

## Configuration, instructions and histories

A valid absolute HOME override wins. Without one, native Windows uses USERPROFILE,
then HOMEDRIVE/HOMEPATH. Configuration, logging, global guidance, themes, global
skills, Claude handoff and Git Bash HOME use that same resolver. WSL uses its own
Linux home. `ZEVRIA_CONFIG` remains intentional; its sibling `models.jsonc` is still
the selected model catalog. Embedded executable paths, log directories and
provider addresses must be valid **in the selected environment**. In particular,
Windows and Linux localhost/provider reachability are not assumed equivalent.

| Input/state | Windows native | Linux/macOS/WSL |
| --- | --- | --- |
| Project guidance | `<workspace>/AGENTS.md` | same shared input |
| Project skills | `<workspace>/.zevria/skills` | same shared input |
| Runtime state root | `<workspace>/.zevria/windows` | `<workspace>/.zevria` |
| State children | `sessions`, `subsessions`, `plans`, `agent-runs`, `ensemble-sessions` | same layout under its root |

Continue/resume, ACP listing, leases, recovery and `clean` use only their runtime's
state. There is no cross-runtime history import. `zevria --runtime native clean`
does not purge WSL history; `zevria --runtime wsl clean` does not purge native
history. Stop sessions/workers in the selected runtime before cleanup. Startup
session diagnostics report runtime/distribution and effective config/state paths
on stderr, never inside the provider's stable application prefix or ACP stdout.

### Protected-read boundaries

Windows guidance, skill manifests/resources and artifact snapshots use owned
handles and component-relative `NtCreateFile` opens, not canonicalize-then-open.
Startup and artifact paths retain lexical drive-prefix normalization so later
opens can still reject a concurrently substituted reparse point; a prior path
check is not permission to follow aliases. Identity, size/version and hard-link
checks come from file handles. The backend
accepts ordinary local drive paths and verbatim disk paths on NTFS/ReFS, and
rejects reparse traversal (including junctions/aliases), network/mapped-network
roots, device paths, streams, and ambiguous components. Unsupported filesystem
arrangements fail explicitly; there is no unsafe read fallback. Unix contained
alias behavior is unchanged. File versions detect observable changes, not every
possible same-size/timestamp-restored concurrent rewrite.

Settings and leases use no-follow handle opens and OS locking. Atomic replacement
uses a same-directory staged file. Windows does not claim Unix directory-fsync
crash durability. This protected-input boundary is **not an OS sandbox** for
commands or ordinary structured editing tools.

## ACP and process lifetime

Windows ACP defaults to native to preserve the client's Windows paths. Built-in
native ensemble workers are explicitly pinned to `--runtime native`. Configured
ACP agents run through a private native job helper, including `npx`/Node launch
shims under Git Bash. The helper bypasses selection and preserves ACP stdio.
Use a Bash-resolvable launcher such as `npx`, or an explicit native executable and
argument vector; do not configure a PowerShell agent-command backend.

Native command/ACP workloads start suspended, acquire a kill-on-close Job Object,
and only then resume. Failed job setup terminates the suspended child. Shell
leader status is independent of job lifetime; ordinary shell exit also kills
background descendants before capture drains. Timeout, turn cancellation and
worker-task abort preserve invocation-scoped cleanup. Unix process groups remain
unchanged. A Windows job is **not** treated as Linux/WSL process-tree ownership.
No cleanup shuts down a distribution.

For explicit WSL ACP, the client must supply Linux paths for session cwd, file
operations and other protocol paths. Only launch-time paths are mapped; Zevria
does not rewrite ACP messages. See [ACP agent setup](acp-agent.md).

## Troubleshooting and acceptance

- No distribution / companion / RTK / timeout: read the readiness stderr notice;
  use `--runtime wsl` to see a required-runtime error instead of automatic fallback.
- Wrong companion: install the matching Linux version; do not point at zevria.exe.
- Stale/custom root: repair the selected distro's `~/.zevria/install-root` as above;
  do not assume a Cargo copy or profile PATH will override it.
- Installer lock: close the running Windows Zevria and rerun; the installer does
  not terminate user processes. Rerun with an older explicit version to roll back.
- PATH refresh: source the printed Unix profile or open a new shell. PowerShell
  child processes cannot update the parent's PATH; restart terminals from a
  refreshed environment. Check the installed absolute path for shadowing.
- Native Git Bash missing: install Git for Windows or correct ZEVRIA_GIT_BASH.
- Native RTK missing: verify `rtk --version` in the command environment; restart
  after changing user PATH. Offline commands remain usable.
- Skills/guidance unavailable: move protected inputs off unsupported network or
  reparse-backed arrangements, or deliberately select WSL. Do not relax containment.
- Missing history: check the startup runtime and state root before trying resume
  or cleanup; configuration/provider errors never switch runtimes after handoff.

Automated lanes are configured to run locked workspace builds/tests on Windows,
Linux and macOS. Mocked launcher tests are separate from the opt-in real WSL test. On an actual
Windows machine, also record:

- [ ] native launch from a non-Bash terminal; cwd, inherited tools, RTK read/sed,
  stdout/stderr/nonzero status, timeouts, cancellation and aborted workers;
- [ ] default/explicit/stopped/absent WSL distribution; missing/incompatible Linux
  companion; bounded readiness; no native retry after first-run/provider failure;
- [ ] matching installer-produced Windows/Linux versions, both default and custom
  Linux roots, a competing Cargo copy, stale-locator repair, and no profile sourcing;
- [ ] local workspace with spaces/Unicode and a WSL-hosted checkout;
- [ ] terminal input/submission, paste and image clipboard, Ctrl-C and terminal
  restoration, with unrelated WSL workloads left running;
- [ ] native ACP and built-in/external worker shutdown, Node/npm descendants,
  and explicit WSL-aware ACP using Linux paths;
- [ ] independent continue/resume/list/clean, protected reads, reparse rejection,
  hard-link aliases, resource pagination, atomic settings and cross-process leases.

The opt-in WSL workflow requires an already-provisioned distro, the absolute
installer-produced Windows executable path, and a matching **custom** Linux
installation root already recorded by `install.sh`. Its ignored integration tests
check the recorded root, matching versions, and real application handoff without
installing or reconfiguring WSL. Also record default-root and interactive results
using the checklist above. These real-machine items are not established by mocked
tests or cross-compilation.
