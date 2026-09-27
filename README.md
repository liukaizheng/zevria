# Zevria

Zevria is a terminal coding agent written in Rust. It can investigate a codebase,
prepare a plan for approval, edit files, run commands, and delegate independent
work—all from an interactive terminal UI. It also runs as a headless
Agent Client Protocol (ACP) agent for editors and other clients.

<video src="https://github.com/user-attachments/assets/fab67791-6691-473a-b342-975c9924bc7f" controls width="100%" aria-label="Zevria working on a coding task in an interactive terminal"></video>

> **Under active development.** APIs, configuration, protocols, and saved-session
> formats may change without backward compatibility.

## Highlights

- **Collaborative Plan writing:** `/ensemble-plan <prompt>` gathers independent
  proposals from configured ACP agents—including Claude Code and Codex in the
  default setup—and synthesizes a Plan after you review and confirm the proposals.
  Workers must be installed and authenticated.
- **Vim-style transcript navigation:** switch between Normal and Insert modes,
  move quickly through messages, copy message or tool content, and fold or unfold
  transcript items and turns with familiar Vim-inspired controls.

## Getting started

### 1. Install Zevria

**Linux x64 GNU / Apple Silicon macOS** (including Bash under Rosetta):

```sh
curl -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/liukaizheng/zevria/main/install.sh | bash
```

**Windows x64**, in Windows PowerShell 5.1 or PowerShell 7 (not Git Bash):

```powershell
irm https://raw.githubusercontent.com/liukaizheng/zevria/main/install.ps1 | iex
```

These commands execute downloaded installer source. Prefer to download and inspect
it first if required by your security policy; see [installation details](docs/releases.md#standalone-installers)
for that alternative, source pinning, manual downloads, and unsigned-release limits.
The moving URLs work after these files reach `main`; a missing stable release is
an error, not permission to install an unverified substitute.

The installers verify the release checksum and executable version, then install
only Zevria into the resolved home's `.zevria/bin`, with automatic **user-level**
PATH setup. Rust is not needed. Linux needs Bash 3.2+, curl, tar and sha256sum or
shasum; macOS has the supported system Bash and utilities. Intel macOS, Linux
ARM64/musl and Windows ARM64 are not packaged.

- Set an absolute `ZEVRIA_INSTALL` to change the executable root (not configuration
  or history locations). Spaces and Unicode are supported.
- Pin a binary version with `bash -s -- v0.0.1` at the end of the Bash pipeline,
  or download `install.ps1` and run `./install.ps1 -Version v0.0.1`. Examples use
  placeholder versions, not a claim about the newest release.
- Use `--no-path-update` / `-NoPathUpdate` to leave PATH and profiles untouched.
- Open a new shell or use the printed refresh instructions. A piped/child installer
  cannot change its parent shell; Windows machine PATH may still shadow user PATH.
- Reruns replace the selected root, including same-version repairs and downgrades.
  To roll back, rerun with the desired version. Other roots and provider files are
  left alone. Linux installs also record the root for Windows-to-WSL discovery.

Install runtime prerequisites separately: **RTK** for command tools, **Git for
Windows Bash** for native Windows commands, a terminal that distinguishes
`Ctrl+Enter`, and a model endpoint implementing the **OpenAI Responses wire
protocol** (not Chat Completions alone). The installer does not install RTK, Git
Bash, WSL, Rust, or provider credentials. See [command conventions](docs/instructions/command-conventions.md).

**Source-build alternative:** with Rust/Cargo (Rust 2024 support), Git, network
access, and native build tools, run this from a checkout:

```sh
cargo install --path crates/zevria --locked
```

For source builds, ensure Cargo's bin directory is on PATH. Then launch Zevria
from the project you want to work on, not necessarily from this checkout:

```sh
cd /path/to/your/project
zevria
```

The startup working directory determines the workspace, project guidance, skills,
and session storage. Starting in a repository subdirectory does not automatically
select the Git root.

### Windows: WSL first, Git Bash fallback

On Windows, `zevria.exe` prefers a ready WSL installation containing a compatible
**Linux Zevria** and the tools required by the operation. It hands off the entire
application, not individual commands. Otherwise it silently falls back to native
Windows with **Git for Windows Bash**, without a WSL warning or captured probe
help/error output. Launching from PowerShell is fine; PowerShell is not the
agent-command backend.

Use `--runtime native` to skip WSL probing, `--runtime wsl` to require WSL and see
detailed readiness errors, and `--wsl-distro <name>` to select a distribution
without changing the default. Automatic mode also falls back silently when that
selected distribution is missing or unready. Normal runtime/configuration/state
startup diagnostics and native Git Bash/RTK errors remain visible.
Windows `--acp` defaults to native. Native history lives under `.zevria/windows/`;
WSL history and global configuration remain separate. Nothing is installed,
upgraded, copied, or migrated by the launcher, and an invocation is **never
retried natively after handoff**, even if Linux first-run configuration fails.

Use matching pinned binary versions for the Windows and Linux installers. A new
Windows launcher discovers the Linux installer's `.zevria/install-root` record,
including custom roots, without sourcing profiles; a stale record must be repaired.
This discovery requires a Windows binary built with the change—installing an older
published version does not retrofit its launcher.

See [Windows setup and acceptance checklist](docs/windows.md) for the two-install
setup, native RTK/Git Bash, supported filesystem boundaries, ACP, and validation
limitations. Initial acceptance targets Windows x86_64 MSVC and WSL 2; other
architectures and WSL 1 are not claimed.

### 2. Configure a provider and model roles

On first run, Zevria creates whichever of these files are missing, then exits
before opening a session:

| File | Purpose |
| --- | --- |
| `~/.zevria/config.toml` | Model-role assignments and session, skills, ACP, ensemble, command, theme, and logging settings. |
| `~/.zevria/models.jsonc` | Provider endpoints, credentials, model IDs, and model capabilities. |

The generated files are commented setup templates, **not a working default
configuration**. You must configure at least one provider/model and all five
model roles.

Here is a minimal configuration shape. **Replace the example endpoint, API key,
model ID, token limits, and reasoning capabilities with values supported by your
provider.** The numbers and capabilities below are illustrative, not detected
from an endpoint.

`~/.zevria/models.jsonc`:

```jsonc
{
  "providers": {
    "primary": {
      "base_url": "https://provider.example/v1/responses",
      "api_key": "replace-with-api-key",
      "supports_websockets": false,
      "models": {
        "your-model-id": {
          "context_window_tokens": 128000,
          "retained_user_tokens": 10000,
          "reasoning_levels": ["low", "medium", "high"],
          "reasoning_summary_level": "detailed"
        }
      }
    }
  }
}
```

Uncomment and replace the `[modes]` block in `~/.zevria/config.toml`:

```toml
[modes]
plan = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
build = { provider = "primary", model = "your-model-id", reasoning_level = "medium" }
review = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
explore = { provider = "primary", model = "your-model-id", reasoning_level = "low" }
builder = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
```

All five assignments are required, even if they use the same model. Explicit
orchestration uses `build`; there is no orchestration mode or sixth model role.

Important configuration details:

- `base_url` is the **full Responses endpoint**, not a host root.
- A model's object key is its exact upstream model ID, not a local alias.
- API keys are literal values in `models.jsonc`; there is no environment-variable
  interpolation or API-key environment override. Keep this file private.
- JSONC supports comments and trailing commas. Provider compatibility flags,
  WebSocket support, token counting, and hosted search are endpoint-specific.
- `ZEVRIA_CONFIG=/path/to/config.toml` selects that TOML file **and the sibling
  `/path/to/models.jsonc`**. It does not change the workspace or skill locations.
- `session.max_model_calls` has been removed. Delete it from existing configuration
  files; strict parsing rejects it as an unknown field. There is no replacement
  call-count cap. Repeated tool continuations can increase runtime and cost until
  completion, cancellation, or another failure; transport retries and context
  capacity remain independently bounded.

See [Responses-compatible providers](docs/responses-compatible.md) for the full
configuration contract and gateway compatibility settings. After configuration,
run `zevria` again from your project workspace.

## Vim-style transcript navigation

Zevria has Vim-inspired keyboard modes for moving around the conversation, copying
content, and folding transcript entries. It is **not a full Vim editor**: these
keys control Zevria's terminal interface, and the same key can do something
different depending on whether you are browsing, writing, or selecting.

### Three modes to remember

- **Normal — browse.** This is the usual mode when Zevria opens and after a
  prompt is submitted. Use `j` / `k` to scroll, `gg` to jump to the top, and
  `G` to jump to the bottom. Press `i` to start writing or `v` to select transcript
  content.
- **Insert — write.** Type and edit your prompt in the composer. Outside an active
  completion, `Enter` adds a new line and `Ctrl+Enter` submits the draft. Press
  `Esc` to return to Normal mode.
- **Select — act on the transcript.** Press `v` in Normal mode to select a
  message, or press `Esc` twice quickly. Selection starts on a visible message
  without scrolling the conversation. In Message scope, `j` / `k` move between
  messages; `Enter` enters Block scope to move among that message's parts. `Esc`
  moves back one scope at a time, eventually returning to Normal mode.

`v` is a Normal-mode shortcut: in Insert mode, `v` and `z` are ordinary text. If
`v` appears to do nothing, make sure you are in Normal mode and transcript content
is visible; it will not scroll the conversation to find an item for you.

### Move through the conversation

In **Normal** mode:

- `j` / `k`: scroll down / up.
- `gg` / `G`: jump to the top / bottom. Type `g` twice for `gg`.
- `PageDown` / `PageUp` (or `Ctrl+F` / `Ctrl+B`): move down / up one page.
- `Ctrl+D` / `Ctrl+U`: move down / up half a page.

In **Select** mode, `j` / `k` move the selection instead of scrolling. In Message
scope they choose the next / previous message; press `Enter` to enter Block scope,
where they move among that message's content blocks. Press `Esc` once to return
to Message scope, then again to leave Select mode. `Ctrl+D` / `Ctrl+U` in Select
mode jump to the next / previous user message; unlike Normal mode, they do not
scroll by half a page.

### Copy a message or tool output

1. In Normal mode, bring the content you want into view and press `v`.
2. Use `j` / `k` to select the message. Press `y` to copy its message content
   to the clipboard.
3. To copy just one part, press `Enter` to enter Block scope, then use `j` / `k`
   to select a block. Press `y` to copy that block's text or tool parameters.
4. To copy a tool's output, select its tool block and press `y` twice quickly:
   `yy`. In Message scope, `yy` is the same as `y`—it does not mean tool output.

### Fold and unfold transcript content

First select the message or block you want to act on. Then press the two keys in
each chord in sequence—for example, `z` followed by `a`:

- `za`: toggle the selected message (Message scope) or block/item (Block scope).
- `zc`: fold the selected message or block/item.
- `zo`: unfold the selected message or block/item.
- `zm`: fold earlier content in each conversation turn while leaving its latest
  eligible entry open.
- `zM`: apply `zm` and also fold the final eligible entry in each older turn;
  the most recent turn's final eligible entry stays open.
- `zR`: unfold all folded transcript content.

`za`, `zc`, and `zo` act on a specific selection, so use them in Select mode.
`zm`, `zM`, and `zR` also work in Normal mode. Folding only changes your view:
it does not edit the transcript or change what Zevria sends to the model.

### A short practice run

After Zevria replies, you are back in Normal mode. Try this sequence:

1. Press `G` to go to the bottom of the transcript, then `v` to select the
   visible message.
2. Use `k` to move to the previous message, or `j` to move forward again.
3. Press `Enter` to inspect that message's blocks. Move with `j` / `k`, then
   press `y` to copy a block—or `yy` on a tool block to copy its output.
4. Press `Esc` to return to Message scope and `za` to fold or unfold that message.
   Press `Esc` again to leave Select mode.
5. Press `i` to start another prompt. Write it, submit with `Ctrl+Enter`, and
   Zevria returns to Normal mode while the response arrives.

Use `y` to copy; `Ctrl+C` is not the copy command and may cancel active work.
The [terminal UI controls](#using-the-terminal-ui) below list other useful keys.

## Collaborative Plan writing

Use `/ensemble-plan` when you want multiple agents to explore a task and help
shape one concrete Plan before implementation. Zevria starts independent ACP
Plan workers and gathers their proposals. Once every participating worker's
current proposal is explicitly confirmed, Zevria publishes a final Plan. With
multiple workers, Zevria verifies the evidence and synthesizes their proposals.
This is different from `/orchestrate`, which delegates implementation subtasks
for a Build request. Ensemble workers propose and discuss a plan; they do not
implement it.

### 1. Prepare the agents

Zevria needs its own working provider/model configuration for the root planning
and synthesis step. The workers also need to be configured and ready to launch.
New configurations include Codex, Claude Code, and Zevria in the Plan-worker list.
The default Codex and Claude Code entries start ACP adapters through `npx`; make
sure `npx` is available and authenticate each agent using its normal CLI setup
before starting an ensemble. Zevria cannot log in to an external agent for you.

To use **just Codex and Claude Code** as workers, set `plan_agents` in the
`[ensemble]` section of `~/.zevria/config.toml`. If the table already exists,
replace its `plan_agents` value rather than adding a second `[ensemble]` table:

```toml
[ensemble]
plan_agents = ["codex", "claude"]
```

The names are configuration IDs (`codex` and `claude`); the displayed worker
labels are **Codex** and **Claude Code**. This selects the workers, not Zevria's
root synthesis model, which still uses its configured Plan role. See the
[ensemble configuration guide](docs/ensemble.md#configuration-and-resource-lifetime)
for custom agents and setup details.

### 2. Start a planning run

In the root composer, press `i` and enter `/ensemble-plan` followed by a useful
task description. State the goal, constraints, important existing behavior, and
what you want the final Plan to cover. For example:

```text
/ensemble-plan Design a resumable upload feature for large files.
Compare a simple implementation with a chunked approach. Preserve existing
cancellation behavior, identify likely files and tests, and call out assumptions
that need my decision. Return a step-by-step implementation Plan; do not edit code.
```

Submit with `Ctrl+Enter`. Zevria starts the configured workers, and their
progress and proposals appear in the ensemble transcript.

### 3. Review proposals and ask for revisions

In the root transcript, press `v` to select the ensemble message, then `Enter`
to enter Block scope. Use `j` / `k` to move to a worker row and press `Enter` to
open that worker's pane. Read its proposal and, if useful, compare it with another
worker's pane; press `Ctrl+O` to return to the root ensemble. Each worker has its
own conversation and proposal.

A worker finishing is **not** the same as confirming its proposal. To ask for a
change, press `i` in that worker's pane, type ordinary feedback (for example,
“include a migration path and tests for interrupted uploads”), then press
`Ctrl+Enter`. Accepted feedback withdraws that worker's previous confirmation.
Wait for the worker to publish a fresh, complete Markdown Plan, review that version,
and confirm the new version.

When a proposal is ready, press `i` in that worker's pane, type `/confirm`, and
press `Ctrl+Enter`. This confirms the exact displayed revision
for inclusion in the ensemble's final Plan; it does **not** authorize code changes.
Confirm the current proposal from each participating worker. Zevria will not
silently treat an unconfirmed or failed worker as approved.

### 4. Get the combined Plan—and decide whether to implement

After every remaining participating worker's current proposal is explicitly
confirmed, Zevria completes the Plan workflow. With multiple workers, Zevria
checks the supporting evidence and synthesizes their proposals; if an important
preference is still unresolved, it may ask you a question. If the run originally
used exactly one worker, Zevria publishes that worker's confirmed Markdown as-is
instead of asking a second model to rewrite it.

The published Plan is the result of planning, **not an automatic implementation**.
When you are ready to make the changes, explicitly choose `/implement` to continue
in the current session or `/implement-fresh` to start implementation in a fresh
session. For more worker controls, recovery, and edge cases, see the full
[ensemble guide](docs/ensemble.md).

## Using the terminal UI

The [Vim-style transcript navigation](#vim-style-transcript-navigation) guide
above covers modes, prompt submission, movement, copying, and folding. The
[Collaborative Plan writing](#collaborative-plan-writing) guide explains
`/ensemble-plan`. This section covers other terminal UI controls and popups.

Type `/` in Insert mode to browse built-in command completions. `Enter` accepts
the highlighted row and runs it only when the whole draft is a valid,
parameterless built-in command. Commands needing a prompt, skills, and file
references only complete text; a suffix or attached image also prevents automatic
execution. Enter with no matching result does nothing.

Root and live Plan-worker drafts remain editable while work is pending or running;
submitting another work item is restricted independently. Acknowledgements never
clear a newer draft. Historical and frozen panes are read-only.

| Key | Action |
| --- | --- |
| `Shift+Tab` | Switch between Build and Plan. |
| `Ctrl+V` | Paste a native clipboard image, or text when no image is available, in input mode. |
| `Home` / `End` | Navigate to the start / end of the focused editor, list, transcript, or dialog. |
| `Tab` / `Ctrl+I` | Open the latest child when pane navigation is available; while completion is active, accept highlighted text only. |
| `r` | In Normal mode with an empty composer, recover a retained rejected draft. |
| `Ctrl+C` | Act on the focused surface only: a composer clears its draft first; a transcript cancels eligible local work. Only an idle root may quit. Dialogs dismiss/cancel locally. |

Plan review defaults to **Revise**. Arrows, numbers and `n` select; `Enter`
activates the selected eligible choice. `Esc` and `Ctrl+C` hide review without
approving or revising; `p` reopens it. A hidden Ready Plan still requires an
explicit decision before new work can be submitted. See the
[TUI interaction contract](docs/tui-interaction.md) for cancellation and ownership
details.

Clipboard image input depends on desktop clipboard access and terminal key
forwarding. See [image input](docs/image-input.md) for limits and headless/SSH
considerations.

### Reference workspace files

In Insert mode, type `@` at the start of the draft or after whitespace, then search
by filename or relative path. For example, `Explain @app` can complete to
`Explain @crates/tui/src/app.rs `. References also work in multiline prompts,
multiple times in one prompt, and within skill/ensemble arguments. Email addresses
and escaped `\@` stay ordinary text.

The **Files** popup shows full paths. Up/Down, PageUp/PageDown, and Home/End select;
Enter, Tab, and Ctrl+I insert without submitting. Loading/empty rows cannot be
accepted. Esc dismisses without removing the query; changing the query or caret
can reopen it. Ctrl+Enter always submits the actual draft, including unresolved
references, without accepting the highlighted result.

A selection is **only text, not attached file contents**. Paths with spaces, quotes,
or backslashes use a reversible quoted form, such as `@"docs/my file.md"`;
inside quotes, `\"` represents a quote and `\\` represents a backslash. Existing
images and surrounding draft text are preserved; one undo restores the query.

Search uses the captured startup workspace, not the Git root. It respects
`.gitignore`, `.ignore`, and applicable parent/Git exclude rules, including outside
Git repositories. Non-ignored hidden/configuration and binary files are included;
`.git` metadata, directories, outside-root/broken symlinks, and unsafe names are
not selectable. Directory symlinks are never traversed. The index builds lazily
in the background and refreshes when the popup reopens; cached suggestions may
remain visible during refresh. Limits or unreadable/omitted entries produce a
**Partial index** status rather than implying a complete search. There are no
filesystem watchers, and a reference is not a snapshot or a guarantee that the
path still exists. See the [file-reference contract](docs/tui-interaction.md#workspace-file-references)
for limits and lifecycle details.

### Choose a workflow

| Command | Workflow |
| --- | --- |
| `/build` | Implement in the current workspace, with optional read-only Explore subtasks. |
| `/plan` | Investigate and prepare a submitted plan for approval, without implementing it. |

Ordinary Build may use Explore but cannot launch Builders. `/orchestrate <prompt>`
requires at least two distinct accepted children together in one `launch_subtasks`
batch; it is not a persistent mode. The parent can also investigate and implement
directly, then integrate and validate the result. The scheduler must be configured
with `session.max_concurrent_subtasks >= 2`. Separate single-child calls do not
qualify, and unsatisfied delegation fails after at most one corrective continuation.
The next prompt is ordinary Build again.

A bare `/orchestrate` retains the draft and asks for a prompt. Neither `/build`
nor `/orchestrate <prompt>` approves or revises a pending Plan; resolve it explicitly.
Legacy transcripts containing the removed Orchestrate mode are rejected without
rewriting them; start a fresh Build session instead.

Other useful commands:

| Command | Purpose |
| --- | --- |
| `/new` | Start an empty session in this workspace. |
| `/resume` | Choose a previous workspace session. |
| `/compact` | Summarize the active model context into a saved checkpoint. |
| `/model` | Select this role's model and reasoning level; save to the session and global configuration. |
| `/model-session` | Select a model and reasoning level for this session without changing global defaults. |
| `/skills` | Inspect, enable, disable, and reload local skills. |
| `/ensemble-review <prompt>` | Collect independent ACP reviews and synthesize a read-only review. |

### Resume from the command line

```sh
zevria --continue
# Short form:
zevria -c
```

Both resume the most recent session in the current workspace. Use `/resume` in
the UI to choose an older one.

## Project guidance and skills

Put shared project instructions in `<workspace>/AGENTS.md`, and personal defaults
in `~/.zevria/AGENTS.md`. Zevria captures them at session opening or resume;
project guidance takes precedence over conflicting global guidance. There is no
ancestor or nested-directory discovery, and edits require opening or resuming a
session to take effect. See [guidance](docs/guidance.md) for platform and file-safety
constraints.

Skills package reusable instructions in either of these locations:

- `~/.zevria/skills/` for global skills.
- `<workspace>/.zevria/skills/` for project skills.

Use `/skills` in the UI or `zevria skills list` from the command line to inspect
them. Invoke a skill with `$name` rather than `/name`. See the
[skills guide](docs/skills.md) for flat Markdown files, `SKILL.md` packages, metadata,
and management commands.

## Editor integration

An ACP client can launch Zevria without the terminal UI:

```sh
zevria --acp
```

Complete provider configuration first. This serves newline-delimited JSON-RPC on
stdin/stdout; it is an ACP server, not a one-shot prompt CLI. See the
[ACP agent guide](docs/acp-agent.md) for client integration, session handling,
capabilities, and the separate ensemble-worker profile.

## Storage and safety

Zevria executes tools with the filesystem and process permissions of its user.
**It does not provide an operating-system sandbox.** Plan/Explore restrictions
and delegated workspace ownership are not security isolation. The ACP frontend
does not request client permission before running tools. Use an appropriately
restricted OS account, container, VM, or sandbox for untrusted work.

Workspace history and generated artifacts live under `.zevria/`, including
`sessions/`, `subsessions/`, `ensemble-sessions/`, `agent-runs/`, and `plans/`.
Logs default to `~/.zevria/logs/`. Conversations, tool output, and submitted images
can contain sensitive source material; images persist as embedded base64 in
plaintext history. Protect these files, keep credentials out of version control,
and remember that prompts and relevant tool output go to the configured provider.

`zevria clean` **deletes all five workspace storage directories listed above
without confirmation**. Stop every session and worker using that workspace first.
It leaves skills and other unlisted files alone; it is not a complete erasure of
global logs or provider-held data.

## Development

From the repository root:

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
```

The main areas of the workspace are:

| Area | Responsibility |
| --- | --- |
| `crates/zevria` | CLI entry point and terminal startup. |
| `crates/app` | Configuration, runtime composition, and application services. |
| `crates/core`, `crates/session-api` | Session engine and frontend/provider contracts. |
| `crates/provider`, `crates/responses` | Model routing and Responses transport/replay. |
| `crates/acp`, `crates/ensemble` | ACP frontend and multi-agent workflows. |
| `crates/tui`, `crates/tui-input`, `crates/tui-widgets`, `crates/theme` | Terminal interface, input, and presentation. |
| Other workspace crates | Shared values, instructions, models, workflows, tools, and transcripts. |

See [architecture](docs/architecture.md) for crate boundaries and data flow.
Follow [AGENTS.md](AGENTS.md) when contributing; changes must preserve the
provider's cacheable prompt prefix.

## Further reading

- [Provider configuration and compatibility](docs/responses-compatible.md)
- [ACP-based planning and review](docs/ensemble.md)
- [Running Zevria as an ACP agent](docs/acp-agent.md)
- [Automatic project guidance](docs/guidance.md)
- [Local skills](docs/skills.md)
- [Image input and clipboard behavior](docs/image-input.md)
- [Terminal themes](docs/themes.md)
- [Inspection policy and its limits](docs/instructions/inspection-policy.md)
- [Architecture](docs/architecture.md)
