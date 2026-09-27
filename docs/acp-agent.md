# Zevria as an ACP agent

Zevria can run as a headless [Agent Client Protocol (ACP)] agent over standard input and output:

```sh
zevria --acp
```

An editor or other ACP client should launch that command with the desired workspace as the process working directory. Zevria serves stable ACP protocol V1 as newline-delimited JSON-RPC on stdin/stdout. This is the opposite direction from Zevria's ensemble support: `--acp` lets a client launch Zevria, while `/ensemble-plan` and `/ensemble-review` make Zevria act as an ACP client that launches external workers.

## Windows runtime selection

Windows `zevria --acp` defaults to **native execution**, not automatic WSL handoff.
Use `zevria --runtime native --acp` to pin this explicitly. Git Bash runs model
commands even when the editor or launching terminal uses PowerShell. Native
sessions require Git for Windows and native RTK; no PowerShell backend is added.

`zevria --runtime wsl --wsl-distro NAME --acp` is opt-in and requires a WSL-aware
client supplying **Linux paths**. The launcher maps its startup directory and an
explicit `ZEVRIA_CONFIG`, but never rewrites ACP messages. No native retry occurs
after handoff. Runtime/config/state diagnostics are stderr-only.

Native Windows history and leases are under `.zevria/windows/`, including
`ensemble-sessions` and `agent-runs`; Unix/WSL use the `.zevria/` paths shown below.
Listing, resume and cleanup never import the other runtime's history. Built-in
Windows workers are explicitly pinned to native. Configured agent launches use
a private Job Object helper so SDK direct-child shutdown also stops descendants,
including Node/npm launchers resolved under Git Bash. See [Windows setup](windows.md)
for configuration boundaries, filesystem limitations and real-machine acceptance.

## Explicit ensemble worker profile

For independent ensemble analysis, launch:

```sh
zevria --acp --ensemble-worker
```

`--ensemble-worker` is valid only with `--acp` and cannot be combined with `--continue` or CLI management commands. Invalid combinations fail before configuration setup or protocol stdout. The profile is explicit: it is not inferred from a client name, prompt, or configuration key. **All ordinary ACP behavior described below remains unchanged when this flag is absent.**

Worker-specific differences:

- Available modes remain `plan` and `review`, never `build` or `orchestrate`. New sessions start in source-read-only Review; the supervisor selects Plan for planning. These route to `ModelRole::Plan` and `ModelRole::Review` using ordinary provider configuration and inherited `ZEVRIA_CONFIG`. Review uses core Build turn mechanics internally, not a new global mode or root Build/orchestration capabilities.
- Plan exposes only the local tools `command`, `question`, and `submit_plan`; Review exposes only `command` and `question`. Both can use provider-hosted `web_search` when enabled in provider configuration. Skills are disabled in policy and request materialization. No skill/model mutation services, structured mutation tools, delegation, nested ensembles, or peer-report reconciliation are installed. Skill extensions are not advertised and requests are rejected.
- The composed [worker policy](instructions/ensemble-worker.md) uses the [shared inspection and scratch contract](instructions/inspection-policy.md): source-read-only with temporary investigative execution. Task-relevant reads outside the workspace (including `~/.zevria/logs/zevria.log`) are permitted under OS permissions. Downloads, preparation, scripts, builds/tests/formatters and package installation are conditional on all side effects remaining in the worker's newly created, unique, private OS-temp scratch directory. This is **not an OS sandbox**. The worker still does not issue ACP permission requests.
- Worker Plan requires the client's `plan` capability (Plan operations), including for restored Planning/Ready sessions. Unsupported activation is rejected with runtime/lease cleanup. Canonical Ready Markdown is sent exactly as `plan_update` with `type = "markdown"` and a stable artifact-derived Plan ID, before the successful prompt response. A successful `submit_plan` result alone is not completion proof.
- Plans are proposals for parent review: no implementation instructions or approval form is emitted. `/implement` and implementation choices are rejected in workers. Only an explicit parent worker-control receipt (`/confirm`, `/baseline`, or the existing confirm shortcut) confirms a proposal. Optional host `/baseline` selects one exact confirmed revision as synthesis foundation/preference authority; `/unbaseline` clears the selection but retains confirmation. These are parent controls, not ACP protocol extensions or provider commands; worker output cannot select itself. Transfers preserve previous confirmations and feedback/withdrawal clears both confirmation and selection. Explicit requirements, captured choices and factual correction outrank baseline preferences. Once all participating workers are confirmed, a run originally selecting multiple workers uses parent verification and synthesis. A Plan run originally selecting exactly one worker instead publishes that exact confirmed Markdown directly, without root inspection, rewriting, synthesis tools or root model calls. Abandoning workers from a multi-worker run does not enable this shortcut. Both paths publish without another approval or an automatic Build transition; implementation still requires a later explicit version-checked command. Questions about unresolved consequential preferences still use normal forms and dismissal/cancellation handling.
- Ready load/resume restores `plan` and republishes the retained artifact without a model request. Replay never grants freshness or confirmation. Genuine nonempty follow-up feedback goes through revision and model work in the same session; it does not merely retransmit Ready proof. Prose-only discussion is valid but requires a fresh complete `submit_plan` publication before parent confirmation. A new ensemble invocation creates fresh sessions; discussion does not.
- Native worker transcripts and `.jsonl.lock` leases live exclusively in `<workspace>/.zevria/ensemble-sessions/`. New/list/load/resume/exact-ID lookup and cleanup share this namespace, with no root fallback. A create-only `.gitignore` containing `*` preserves existing user guards. Workers do not create `.zevria/plans` Markdown projections and cannot displace the parent in ordinary `/resume` or `--continue` discovery.

Normalized parent evidence remains in `.zevria/agent-runs/...`. Native transcripts and parent logs can contain source material, diagnostics and accepted non-secret decisions; ignore guards are not confidentiality or retention controls. See [ensemble.md](ensemble.md) for default participation, opt-out, and recovery contracts.

## Provider-hosted search and citations

Native Zevria ACP sessions and native ensemble workers use the ordinary
[per-provider search configuration](responses-compatible.md#provider-hosted-web-search).
External ACP agents manage their own capabilities; Zevria does not rewrite their
provider transports. Hosted search has automatic tool selection unless a
permitted provider override is configured, and remains unavailable to internal
summaries/compaction and empty task allow-lists.

Live activity projects as ACP tool-call/update notifications with
`hosted-search:<attempt-id>:<output-index>` IDs, separate from local functions.
Search and find-in-page use `ToolKind::Search`; open-page uses `ToolKind::Fetch`.
Interruption is a failed status with an explanatory label; already completed
activity retains completed status if its attempt later fails. Raw input retains
hosted origin/attempt/action data through native-worker normalization so these
updates do not split or erase text previews.

For search-advertising requests, provisional answer text streams before completion,
including when no search action occurs. Usable annotations add inline source-title
links while the answer grows; completed native output reconciles the final text.
Ordinary growth emits suffixes, while a genuinely non-prefix citation correction
starts a separate authoritative segment. Generic append-only clients cannot
retract the earlier segment; Zevria's metadata-aware views reconcile the same
indexed answer without duplicating it. Reasoning remains live. Usable links survive
model switching, copying, child final reports and transcript replay; unknown
annotations remain in native history. Separate display-attempt records retain
observed partial answers at retry, failure and cancellation boundaries without
becoming model input. Client/terminal URL activation is client-dependent, not an
in-app opener or promised shortcut.

## Security: tools execute directly

> **Warning:** the ACP frontend does not request client permission before executing tools, and Zevria does not provide an operating-system sandbox.

`command`, `edit`, `write`, `delete`, and delegated work run with the ambient filesystem and process permissions of the `zevria --acp` process, just as they do in the ordinary TUI. The server never sends `session/request_permission`. A malicious prompt, compromised client, unsafe model response, or tool mistake can therefore read or mutate anything the Zevria process can access. Run Zevria under an appropriately restricted OS account, container, VM, or sandbox when the workspace or client is not fully trusted.

Ordinary Plan, including ordinary ACP Plan sessions, Explore, native worker Plan/Review, and ensemble Plan/Review synthesis share the [inspection policy](instructions/inspection-policy.md). Their command restrictions are behavioral rather than an OS security boundary. The configured workspace remains the default cwd for every invocation, not a read boundary. Reads through absolute/home paths, parent-relative paths and external symlinks are permitted. A read does not grant write access; evidence can enter model requests and persisted logs, so keep it task-relevant and protect sensitive data.

Investigative writes require an explicitly created, agent-owned private directory in the actual OS temporary location, recorded in normal command results. Shared temp roots, other workers' directories, project `tmp` directories and original source trees even under `/tmp` are not scratch. Independent copies (not hard links or write-through symlinks) are required when tools write alongside source. Inspect scripts/configuration first and direct dependencies, caches, outputs, logs and subprocess temporary files into owned scratch. If side effects cannot be established sufficiently, skip execution and report the limit; cwd and environment settings are not isolation. Uploads, deployment, global configuration/toolchain changes and remote-state changes remain unauthorized.

Scratch files are disposable, not canonical Plan publications, confirmation, or implementation approval. Clean up only owned scratch before publication when practical; hard termination can leave files behind, and resume does not guarantee they still exist. Tool registries, skill restrictions, mode and approval gates are unchanged.

These are compiled role and capability modules, not a hot reload or transcript migration. Restart/reopen native workers to adopt changed policies; restored sessions reconstruct their instruction set under the engine protocol's authority boundary. A fresh ensemble reliably receives the full revised launch envelope. Third-party ACP agents retain their own policies and sandboxes; Zevria cannot promise their support for external reads or scratch execution.

## Configuration

The ACP server reads ordinary settings and all five complete `[modes]` assignments from `~/.zevria/config.toml` (or `$ZEVRIA_CONFIG`), and provider/model capabilities from its sibling `models.jsonc`. Every assignment requires `provider`, `model`, and a supported `reasoning_level`; models have supported `reasoning_levels`, not a selected default. For example, `ZEVRIA_CONFIG=/x/work.toml` selects that file and `/x/models.jsonc`. Provider API keys are literal JSONC values; Zevria performs no `ZEVRIA_API_KEY` override, interpolation, or per-provider environment lookup. On first run Zevria creates whichever commented skeletons are missing and exits before starting the ACP server. The `[acp]` section remains in TOML.

```toml
[acp]
max_sessions = 4
expose_session_list = true
```

- `max_sessions` bounds simultaneously active provider connections, transcript writers, question brokers, ensemble supervisors, and subtask supervisors. It must be greater than zero.
- `expose_session_list = false` stops advertising and accepting `session/list`. Explicit `session/load` and `session/resume` by exact session ID remain available.

Each active ACP session owns an independent provider connection and runtime. Per-session delegated work remains subject to the ordinary `[session]` subtask limits.

Every Zevria-owned new/load/resume captures [AGENTS.md guidance](guidance.md)
from `~/.zevria/AGENTS.md` and the **workspace supplied for that session**, not
the server process working directory. Project guidance overrides global defaults.
`ZEVRIA_CONFIG` does not move either source. Explore and Build children inherit
their parent's opening snapshot; native workers capture independently with their restricted tools and
disabled skills unchanged. Ordinary prompts and model/mode changes do not reload
files. Skipped-file startup diagnostics use existing ACP diagnostic updates,
while the rendered instruction set stays outside conversation replay.
Resume clears a prior contribution when its file is missing, empty, or rejected.
External ACP agents retain their own guidance behavior.

New sessions (including fresh implementation handoffs) use current global model
assignments from `config.toml`, validated against the sibling `models.jsonc`, at the
configuration path captured at server startup (including `ZEVRIA_CONFIG`). `session/load` and `session/resume` restore the root's last
successfully selected **Build and Plan** exact provider/model identities and
reasoning levels together from its version-1 metadata header. Request-local orchestration
uses that same saved Build identity; there is no sixth role or `modes.orchestrate` assignment. Current catalog
limits/settings still apply; an unavailable saved model or unsupported saved level
rejects resume with guidance to restore the catalog entry/level or start a new session.
No fallback, provider traffic, or header rewrite is performed. Review, Explore, and Builder use current
globals; external ensemble agents are unchanged.
Resume never rewrites global assignments or infers selections from native replay.
Every new/load/resume also uses the current application prompt from freshly
parsed configuration, including an explicit empty value. Captured guidance renders
in the complete top-level instruction set, not as ordered developer input. Within
one workflow it remains byte-identical across ordinary turns and skill activation. This applies to Zevria-owned
native workers too, not external ACP agents' independent provider clients.
Active sessions and retries are not hot-reloaded. The TUI's `/model-session` changes only the
composer's captured model role, saved for resume with config unchanged. Build targets
Build; Plan targets Plan. `/model` also saves that role's complete assignment in
`config.toml`. Orchestration does not change model selection or reasoning.
Both pickers require a profile choice followed by explicit reasoning confirmation;
the first stage does not save or apply anything. ACP
loading/resuming that same root restores either command's saved local choices, including a switch acknowledged
just before closing without another assistant response. Fresh sessions and
`/implement-fresh` handoffs still use globals. Missing saved catalog identities
still block resume; the header is not a historical configuration snapshot.

Both TUI commands require an idle writable interactive root and preserve explicit
confirmation for conversion, which costs tokens and can change shared context for
both root modes. A session-only header-save failure leaves active and durable selections
unchanged, never modifies config, and returns no new global revision; an already
saved conversion checkpoint is reported and reusable on retry. `/model` retains
its distinct partial-save outcome: a global save may succeed before a header
failure, returning the committed revision while the session selection stays put.

There is no separate reasoning picker: `/model` and `/model-session` are the only
interactive model/reasoning configuration commands. `/reasoning` and
`/reasoning-session` are removed without aliases. Compatible same-profile reasoning
edits change only request properties, never the prompt prefix, tools, input, socket,
or cache key; on WebSocket they need one full-input resend, not a reconnect.
`models.jsonc` is never rewritten by management. Old configuration locations and
version-1 session-model headers are rejected rather than automatically migrated.

There is **no ACP model/reasoning-selection API**; ordinary ACP text is not model-management
traffic. Management events, selection metadata, instruction sets and skill directives
never become assistant/tool content or history replay updates. Load still replays
visible history; resume still does not.

Root and native-worker resumes require valid saved Build and Plan selections;
ordinary Explore/Build child logs do not require root model metadata. Every owned
session renders its current instruction set, without saved prefix metadata.
Missing required or unsupported metadata rejects startup instead of inferring
old instruction overlays or current defaults.
The offline `sessions recover-models` command is removed. There is no metadata
initialization, migration, or new backup on open. Start a fresh session or use a
matching older binary for older data. An unavailable saved profile can still be
restored under its exact catalog keys.

Unsupported root, child, or referenced worker history fails through the ACP
factory error channel before session activation, history publication, provider
calls, or supervisor startup. The requested session is not replaced with another
session, and its original bytes are preserved. Missing referenced worker logs
remain a valid interruption boundary.

A separate `.jsonl.lock` lease protects ownership through startup, transcript
replacement, and shutdown/cleanup. Runtime and engine share the lease; dropping a
frontend or timing out its task observer cannot release a surviving engine's
ownership. Empty files and valid legacy metadata-only roots without a saved mode
are omitted from listings and removed on orderly shutdown. A mode-bearing root,
even a canonical Build header without messages, remains available for reopening.
Damaged/substantive logs remain visible.

Per-session sidecars are transient. Final lease release automatically removes
regular, zero-byte sidecars best-effort; the next new/load/resume startup sweeps
inactive leftovers, including crash files with or without matching transcripts.
An actual nonblocking exclusive OS lock probe determines inactivity, not file
presence, transcript absence, modification time, or a PID. Active locks are kept,
even before transcript creation. Transcript bytes, customized/nonempty files,
symlinks, and unrelated entries are not removed by this sweep.

Each namespace has one **permanent** `.leases.lock` coordinator: root sessions in
`.zevria/sessions`, native workers independently in `.zevria/ensemble-sessions`.
Short coordinated open/lock and close-before-delete sections protect per-session
locking identities across removal. Scanning and bounded coordinator waits run
off the async executor. Final release never sleeps for the coordinator; filesystem
or coordination failures defer cleanup to a later startup. Residual sidecars alone
do not block later ownership. Maintenance diagnostics never go to ACP stdout.

**Before deploying this cleanup protocol, stop all pre-fix Zevria processes and
workers in the workspace, then start the new binary.** Old binaries do not honor
the coordinator; safe mixed-version deletion is not guaranteed. Never manually
delete or replace locks during concurrent use, especially the permanent
coordinator. Advisory locking must work on the underlying filesystem. External
mutation and concurrent `zevria clean` remain unsupported: that command is still
an explicitly destructive offline purge, not this automatic cleanup mechanism.
Stop all sessions/workers before using it. Configuration `.skills.lock` files are
outside this protocol. Older binaries may also reject the new metadata schema.

Model changes commit checkpoint (if required), global default, then session header
before installing the route/acknowledging success. A header failure after the global
save leaves active and saved session selections unchanged, but reports that the
global default was saved and supplies its new revision for retry. A crash after
header commit restores the selection even without acknowledgement.

## Supported ACP V1 surface

Zevria advertises and implements:

- initialization at `ProtocolVersion::V1`;
- `session/new`;
- optional `session/list`;
- `session/load` with durable history replay;
- `session/resume` without history replay;
- `session/close`;
- `session/set_mode` for the root mode IDs `build` and `plan`;
- one ordered text/image `session/prompt` at a time per session;
- `session/cancel`, including cancellation before Zevria has announced a concrete turn ID;
- streamed agent messages and thoughts;
- tool-call creation and correlated completion/failure updates;
- task-list snapshots as ACP Plan updates;
- usage/context-window updates;
- structured form elicitation when the client advertises it; and
- durable Plan-artifact approval and revision workflows.

Both root and worker profiles advertise image prompt support. Ordered text and embedded PNG, JPEG, WebP, and GIF blocks use the shared validated image layer; image-only prompts are valid. Original bytes and MIME types survive live projection and session replay. No URI is fetched. See [image input](image-input.md) for byte, pixel, count, animation, persistence, and model limitations.

Zevria does **not** support audio prompts, embedded resources, resource-link prompts, MCP servers, additional workspace roots, client filesystem or terminal delegation, interactive authentication, session deletion, fresh-session Plan implementation, or ACP permission requests. Blank complete prompts and unsupported content blocks are rejected.

The requested `cwd` must be an existing absolute directory and is canonicalized before startup. Additional roots and MCP inputs are rejected rather than silently ignored.

## Tool titles and status updates

Initial tool calls carry a descriptive title such as `command: rtk cargo test`.
The detail keeps its existing 120-character bound; the full arguments remain in
`rawInput`. Completion and failure updates omit `title`, conveying the outcome
through `status` rather than replacing the description with `command completed`.
Correlated IDs, kind, input/output, and file-change content are preserved. The
same projection is used for live calls and newly loaded durable-session replay.
ACP clients should continue honoring explicit title updates from other agents.

## Build and Plan modes; request-local orchestration

An ordinary new session starts in Build mode. Clients may select `build` and
`plan` through `session/set_mode` while idle; `orchestrate` is rejected as an
obsolete mode ID. A mode change is rejected while a prompt or conflicting
management is active, or while a Ready Plan awaits a decision. Native ensemble
workers retain only their existing `plan` and `review` modes.

| Root mode ID | Parent policy | Native Explore children | Native Build children | Model role |
| --- | --- | --- | --- | --- |
| `build` | Full ordinary implementation tools | Available | Only for an explicitly orchestrated request | Build |
| `plan` | Restricted inspection, questions, and `submit_plan`; configurable skills | When Plan subtasks are enabled | Forbidden | Plan |

Root initialization advertises `agentCapabilities._meta["zevria.orchestration"]`
as `{ "version": 1, "request": "session/prompt", "buildOnly": true,
"minimumBatchSize": 2 }`. Workers do not advertise this capability. Opt in on one
prompt using metadata, not slash-command text:

```json
{"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":"SESSION_ID","prompt":[{"type":"text","text":"Implement two independent components and integrate them"}],"_meta":{"zevria.orchestration":{"version":1,"enabled":true}}}}
```

Absence or `enabled: false` means Standard. The extension requires exactly the
version-1 `version` and boolean `enabled` fields; malformed shapes, unknown fields,
and unsupported versions are invalid parameters. Unrelated client metadata is
ignored. The normal ordered text/image prompt, including image-only input, is
unchanged. Literal `/orchestrate` text does not activate this behavior.

Enabled requests require root Build, no Planning/Ready Plan workflow, a permitted
launcher, and `session.max_concurrent_subtasks >= 2`; rejection occurs before
history mutation. This does not change the selected mode, approve/revise a Plan,
or enable orchestration for workers, skills, handoffs, or subsequent requests.
A successful request must receive at least two distinct accepted child identities
in one correlated `launch_subtasks` result. Separate single-child calls do not
qualify. The engine permits at most one durable corrective continuation, then
fails on a second premature final response if the obligation is still unmet.
Accepted children may subsequently fail; the parent must handle their reports.
This is concurrent submission, not a guarantee of wall-clock overlap.

The parent retains direct implementation tools. Builders use one owned,
non-overlapping child workspace each; the unsandboxed shell and cooperative
write boundaries are not an OS sandbox. Child artifacts remain after completion,
failure, or cancellation without automatic rollback or cleanup. Explore and
Builder retain dedicated child model roles and restricted toolsets. See
[architecture.md](architecture.md#subtasks-independent-explore-and-build-subsessions).
Legacy transcripts containing persisted Orchestrate mode fail load/resume with
an actionable diagnostic and are not rewritten.

Mode selection is a session-local durable control, not model input, a model tool,
plan submission, or approval. Accepted changes survive an immediate close followed
by load/resume, even without another prompt or assistant response, and never write
a global mode default. Ready restoration forces Plan but does not approve or start
implementation. A retained revision artifact or ensemble Published artifact does
not erase a later explicit mode selection. New sessions still start Build.

The response and `CurrentModeUpdate` follow the engine's correlated durable
acknowledgement; the frontend never switches optimistically. Pending selection
blocks prompt admission and conflicting management. Cancelling the RPC cannot
undo an already-enqueued atomic save: it resolves the caller's wait, but admission
stays locked until the matching engine result reconciles the saved selection.
Timeouts also keep admission locked rather than guessing whether a commit occurred.
Shutdown and background failure resolve waiting requests without admitting new work.

The TUI exposes zero-argument `/build` and `/plan` mode controls; `Shift+Tab`
toggles those modes. `/orchestrate <prompt>` instead submits one Build request.
ACP uses `session/set_mode` for modes and the metadata above for orchestration,
not generic text as either API.

When a live Plan turn successfully submits an artifact, Zevria publishes the canonical Markdown and keeps the original ACP prompt pending until the authoritative Ready state arrives.

If the client supports form elicitation, Zevria asks:

- **Implement in this session** — sends the version-checked current-session handoff, keeps the same ACP prompt pending through the ensuing ordinary Standard Build turn, and resolves it only when implementation reaches a terminal event.
- **Revise** — returns the durable workflow to Planning and ends the current ACP prompt so the client can send revision instructions.

Declining or cancelling the decision leaves the Ready artifact unchanged and ends the current prompt.

If form elicitation is unavailable, Zevria publishes fallback instructions, leaves the artifact Ready, and ends the Plan prompt. A later text-only prompt containing exactly `/implement` approves implementation in the current session without recording that slash command as model input. Image-bearing implementation controls are rejected. Any other complete prompt while Ready first selects Revise, waits for the Planning state, and submits the same ordered text/image input in Plan mode.

`ImplementFresh` is intentionally unavailable through ACP V1. Keeping one stable ACP session ID while rotating to a new durable Zevria transcript requires a persistent alias design. The TUI retains its fresh-session implementation option; both TUI handoff paths likewise start Standard Build without orchestration. The ACP server never silently maps that choice to current-session implementation.

Loading or resuming an ordinary Ready artifact forces Plan and publishes the artifact and fallback instructions, but never starts unsolicited implementation work. A restored ensemble `Published` artifact is displayed without an approval form or Ready fallback instructions and does not override an explicit mode selection. An explicit `/implement` may authorize its exact version later in ordinary Build only. The Ready approval behavior above applies to ordinary Plan, not worker proposals or canonical ensemble publication.

## Questions and form elicitation

When a model calls Zevria's native `question` tool and the client advertises ACP form elicitation, Zevria maps text, single-select, and multi-select prompts into one form. Required and optional fields, text lengths, selection bounds, descriptions, defaults, and predefined choices are preserved. On the wire, `allow_other` adds the synthetic `__zevria_other__` option and an optional companion string. Each companion identifies its actual primary property through native metadata:

```json
"_meta": {
  "zevria": {
    "questionId": "question_0",
    "isOtherAnswer": true,
    "otherValue": "__zevria_other__"
  }
}
```

Metadata-aware clients fold this pair into one select with an integrated **Other** editor, hiding the wire-only option and companion prompt. A predefined answer returns its original option token and omits the companion. A custom single answer returns both the primary `otherValue` token and nonblank companion text; a custom multi answer includes that token exactly once alongside any predefined tokens and returns the text in the companion. This preserves required primary selects. Skipping an optional question omits both properties. A required primary cannot be satisfied by companion text alone.

A primary default selecting `otherValue` is paired with the companion's nonblank default text and folded into the existing Other editor. Multi-select defaults retain ordinary selections and count the custom default once. Selecting Other on a single-select default opens its prefilled editor; it does not auto-submit. Accepted values are validated and merged back into the native answer.

Clients unaware of `_meta.zevria` can still display the explicit fields, with the companion described as “Custom answer used when Other is selected.” Both the Zevria producer and client must be updated for the integrated experience. This native token-plus-text contract is distinct from the Codex and Claude Code companion conventions described in [ensemble.md](ensemble.md).

Accepting a form answers the existing tool call. Declining or cancelling dismisses the question, which is a successful tool result rather than cancellation of the whole turn. If the client cannot display forms, Zevria immediately dismisses the question and emits a thought explaining the fallback.

Prompt cancellation, `QuestionClosed`, session close, and connection loss cancel the exact outstanding elicitation request.

## Sessions, replay, and TUI interoperability

An ACP `SessionId` is the file stem of the corresponding root transcript under:

```text
<workspace>/.zevria/sessions/<session-id>.jsonl
```

ACP and TUI sessions use the same persistence. A session created through ACP can later be opened with `zevria --continue`, and an existing TUI session can be listed, loaded, or resumed by an ACP client.

Session IDs are resolved only by exact matches returned from Zevria's transcript listing. The server never joins an arbitrary client-supplied ID into a filesystem path, so path-like IDs such as `../other` cannot traverse outside the session directory.

`session/list` is newest-first, omits empty files and valid legacy metadata-only roots without a saved mode, uses bounded preview titles, and returns bounded cursor pages. Mode-bearing roots remain listed even without messages. `session/load` sends visible replay updates before its response; `session/resume` starts the same durable state without replaying old messages.

Replay includes ordered user text and images, assistant text, supported reasoning, tool calls and correlated results, captured file changes, task snapshots, provider-backed canonical messages, compact direct skill invocations, typed Plan handoffs, and visible transcript errors. It intentionally omits session mode/model selection metadata, embedded skill application snapshots, ordered skill directives, compaction internals, non-handoff Plan metadata, hidden ensemble synthesis payloads, and raw provider replay envelopes. Structured direct invocations own their full typed activation pins; tool-result records carry hidden skill applications, never frontend-facing result metadata. Compaction checkpoints are v1 and store neither instruction snapshots nor historical pin identities; provider replay remains v1. Acknowledgement wording has no lifecycle authority. There are no standalone pin records or pin-sidecar adjacency requirements. Only incomplete current-format trailing JSONL appends are automatically recoverable, including skill directives that can be derived again from pins and policy; pin-bearing lifecycle tails and completed invalid inner envelopes are never discarded. Invalid, tampered, or unsupported records reject the entire startup, not just the affected row, and preserve the original bytes. No partial or read-only session is published for such history. Engine protocol, application/file guidance, workflow JSON and role text, capability modules, and eligible skills render as the complete top-level instruction set. Skill bodies and revocations alone remain typed ordered model input, hidden from ACP conversation rows. The instruction set is not a transcript record; directives persist as hidden `zevria_skill_directive` records containing only version 1 and a semantic payload, with text re-rendered on load. Full skill activation bodies, digests, metadata and provenance intentionally persist in pins. Resume reproduces historical directive positions and validates them against pins before dispatch, while rebuilding current guidance and reconciling policy changes without repeating unchanged directives. Immediate maintenance uses captured current application/file guidance without activating skills. Instructions remain byte-identical within a workflow across turns and skill activation; workflow switches and catalog-management mutations intentionally change them. Unchanged instructions, tools and policy preserve the pre-shutdown input prefix across resume; changed guidance and server-side cache retention can still prevent cache reuse. Retired instruction-bearing histories, including unsupported skill directive versions, are rejected unchanged before session publication or tail repair, with no migration; start a fresh session. Directive records live in the JSONL, not a separate instruction-state file. This does not erase old files, quoted conversation/summary text, or provider retention. Ordinary requests receive the same complete eligible catalog with bounded descriptions and model-led selection contract as TUI requests; no ACP extension or text rewriting is required for automatic selection. Versioned `_zevria/skills/*` extensions provide metadata management and typed invocation without replaying bodies. Ordinary ACP text retains its existing semantics and cannot configure skill roots.

Provider replay uses destination-specific preflight for ACP generation as well
as TUI turns, Plan handoff, and ensemble synthesis. Exact configured profiles
retain native replay; foreign/untagged assistants project visible text and
correlated function exchanges without foreign reasoning, opaque data, or native
IDs. Destination-incompatible opaque checkpoints remain inspectable and do not
spend tokens on startup. Open the session in the TUI to select its configured
source or explicitly confirm portable conversion, or start a fresh session.
This current-format profile-conversion requirement is distinct from a corrupt
or unsupported replay schema, which rejects startup entirely. Neither case
silently migrates or discards original transcript records.

These projection changes do not rewrite historical worker JSONL. Newly projected durable ACP sessions use stable tool titles, but replay of already-recorded worker events retains their recorded title updates and decision fields.

Only one process may activate a persisted session ID at a time. Concurrent prompts on one session are rejected rather than ambiguously correlated.

## Local skill extensions

`initialize` advertises `agentCapabilities._meta["zevria.skills"]` with
`version: 1`, the request/notification names below and `fixedRoots: true`.
These are Zevria extensions, not standard ACP methods. Each request requires
`version: 1` and `sessionId` for an already-active session. All other extension
versions are rejected; no compatibility adapter is provided.
Unknown fields (including root/workspace overrides) and unsupported versions
are invalid parameters. The pinned SDK is dispatched through exact typed
underscore-prefixed methods rather than its generic extension fallback.

| Method | Additional request fields |
|---|---|
| `_zevria/skills/list` | optional `query` |
| `_zevria/skills/inspect` | validated `name` |
| `_zevria/skills/reload` | `expectedRevision` |
| `_zevria/skills/config/write` | `expectedRevision`, `name`, `enabled` |
| `_zevria/skills/invoke` | validated `name`, optional `args` (ordered ACP text/image content-block array) |

Management results use `{ "version": 1, "result": ... }`. The nested result
has one of these shapes:

- `{ "kind": "view", "view": { ... } }`: the complete filtered metadata view,
  including fixed locations, revision, counts, entries/invalid entries, bounded
  diagnostics and authoritative explicit-invocation `completions`. There are no
  management cursors, page limits or public source selectors. Per-field shortening
  and discovery bounds remain; no main instruction body is returned. Inspect
  returns every candidate and historical pin matching the validated name.
- `{ "kind": "changed", "revision": "...", "counts": { ... }, "unchanged": false }`:
  the prepared catalog is already installed. Unchanged revisions are no-ops.
- `{ "kind": "error", "code": "...", "message": "..." }`: a bounded engine
  rejection such as `busy`, `stale_revision`, `read_only`, or `update_failed`.
  Invalid protocol parameters, missing sessions, and frontend busy rejections
  use ordinary JSON-RPC errors instead.

For example:

```json
{"jsonrpc":"2.0","id":7,"method":"_zevria/skills/list","params":{"version":1,"sessionId":"SESSION_ID","query":"review"}}
```

Invoke by name; the engine resolves that name at admission, preferring the
session's existing pin over installed definitions. No revision or source binding
is submitted. Completion metadata is the shared explicit-resolution projection:
it includes eligible explicit-only names and enabled historical pins, not every
`enabled` candidate row. Management view fields use the core's snake_case;
outer ACP parameters use camelCase.

```json
{"jsonrpc":"2.0","id":8,"method":"_zevria/skills/invoke","params":{"version":1,"sessionId":"SESSION_ID","name":"review","args":[{"type":"text","text":"inspect the patch"}]}}
```

Invocation returns `{ "version": 1, "stopReason": "end_turn" }` (or another
ACP stop reason) only when the tracked prompt completes. It uses the session's
actual Build/Plan mode with Standard request behavior; no request mode override exists. With a Ready Plan, the
engine uses `RevisePlanWithSkill { expected, name, args }` and validates the
expected Ready version, skill, permissions and complete prompt capacity **before**
atomically recording `RevisionRequested`, the invocation and its directives.
Missing/disabled skills, stale versions, capacity failure or cancellation before
commit leave the Plan Ready with no invocation or pin. This is revision, not
approval or implementation, and does not bypass ensemble/workflow gates. Prompt cancellation, request cancellation, Plan approval,
questions, terminal failures, and session shutdown use the same lifecycle as
ordinary prompts. Pre-acceptance `TurnRejected` emits a correlated rejection
diagnostic and resolves the pending request with an error (or cancellation if
already requested), without creating a transcript row or entering Plan approval.
Blank text prompts use this engine rejection path as well. Argument text is capped
at 64 KiB, independently of shared image limits. For example, `args: [{"type":"text","text":"inspect this"},{"type":"image","mimeType":"image/png","data":"BASE64"}]` supplies structured arguments; image-only arrays are also valid. `session/prompt` never treats `$name` text as typed skill syntax.

Metadata queries remain available during a prompt or model maintenance, using
the captured projection. Reload and config writes are idle-only and rejected
rather than queued while busy. At the engine boundary, prompt/turn commands
queue FIFO behind either active turns or model maintenance; the ACP frontend
continues to allow only one pending prompt request per session. Reload uses the same
two fixed roots captured at session startup and reloads only skill behavior,
not HOME, workspace, provider routing, or Plan permissions. Configuration
writes preserve unrelated settings, secrets, comments, and permissions through
the shared locked atomic writer. Other sessions remain unchanged until their
own explicit reload.

After an installed change, `_zevria/skills/changed` carries
`{ "version": 1, "sessionId": "...", "revision": "...", "counts": { ... } }`.
This is a bounded invalidation notification, not a catalog dump. Refresh the
complete view and completion metadata in response. No notification is emitted for a failed or
unchanged update. See [skills.md](skills.md) for discovery and resource rules.

## Streaming and diagnostics

Zevria's engine exposes complete, coalesced stream snapshots while ACP is append-only. Within one stable segment the server emits only the unseen suffix. If a later complete snapshot diverges, Zevria starts a new message ID and emits the replacement as a new authoritative segment; ACP has no retraction operation. Terminal output appends the final suffix when possible, or emits a final authoritative replacement segment after divergence.

Tool updates include raw JSON input, inferred file locations, status, text/raw output, and captured file changes. Readable UTF-8 additions and deletions use ACP structured diffs with the complete text. Readable write-overwrites preserve one complete full-context unified patch; that patch and any move destination are emitted as fenced diff text because the ACP V1 diff shape cannot losslessly represent them. Zevria does not locally truncate these readable `write`/`delete` payloads, although an external client or transport may impose its own limits. Historical or unreadable changes that were captured as `Omitted` remain explanatory text. Child-subtask conversations remain durable in their own transcript files and are not flattened into the root ACP stream.

Provider retries, compaction, persistence degradation/recovery, Plan projection warnings, malformed transcript recovery, and interrupted ensemble recovery are emitted as concise thought/status diagnostics rather than fabricated conversation text.

## Ordered response display metadata

Zevria attaches an optional `zevria.response_display` object to **notification-level**
`SessionNotification._meta`. Version 1 of this extension carries a version-1
`WebSearchAttemptRecord` and explicit bindings to the ordinary ACP projections:

```json
{
  "_meta": {
    "zevria.response_display": {
      "version": 1,
      "attempt": {
        "version": 1,
        "id": "attempt-id",
        "profile": { "provider": "provider", "model": "model" },
        "response_id": "response-id",
        "outcome": "in_progress",
        "revision": 3,
        "activity": [],
        "terminal": {},
        "presentation": [{
          "source": {
            "output_index": 0,
            "part": { "summary": 0 },
            "item_id": "reasoning-item"
          },
          "content": { "type": "reasoning", "text": "Readable thought" }
        }]
      },
      "bindings": [{
        "type": "text",
        "kind": "thought",
        "message_id": "zevria-turn-1-thought-0",
        "start": 0,
        "end": 16,
        "sources": [{
          "output_index": 0,
          "part": { "summary": 0 },
          "item_id": "reasoning-item"
        }]
      }]
    }
  }
}
```

The example shows the metadata portion only. A text binding addresses a UTF-8
**byte range in the accumulated standard message**, not a range in one token
chunk. `kind` is `message` or `thought`. `sources` explicitly lists the covered
source parts in projection order; their text joined with newlines must equal
that range. Thus one flattened standard message ID may cover several distinct
output/summary/content parts without asserting that those parts were adjacent
in provider output. Part identities are `{ "summary": n }`, `{ "content": n }`,
or `"tool"`. Native-tool presentation stores only a `call_id`, never executable
arguments. Tool bindings carry `tool_call_id`, `output_index`, and `native`;
hosted IDs remain `hosted-search:<attempt-id>:<output-index>`.

Metadata is emitted for live snapshots, status-only changes, attempt closure,
authoritative final reconciliation and transcript replay. An otherwise empty
`session_info_update` can carry metadata when ordinary text has no new suffix.
The host does not turn that empty carrier into a text boundary. Existing ACP
visible text, tool IDs, titles, Search/Fetch kinds and standard statuses remain
available to external clients; the richer unconfirmed/completed distinction is
not a redefinition of standard ACP tool status.

The host normalizes valid metadata into persisted, display-only
`AgentRunEvent::ResponseDisplay`. Validation checks schema versions, nonblank
bounded identities, source addresses, unique outputs/parts, terminal evidence,
projection references and non-overlapping ranges. IDs are limited to 1 KiB,
actions to 4,096, readable parts and bindings to 16,384, individual readable/action
payloads to 4 MiB, and the complete extension to 16 MiB. The TUI additionally
checks coverage against received standard text before suppressing it. Delayed
metadata can reconcile existing rows; duplicate and older revisions cannot
append another response. An attempt's content is immutable within a revision;
conflicting same-revision payloads are ignored, while additional verified
bindings may enrich its displayed coverage. Unsupported or malformed metadata
leaves ordinary updates intact. The host retains latest coverage evidence per
emitted segment/range (not every token revision), so a late preview of a superseded
segment can still be matched after a citation correction. A range never suppresses
unrelated text outside its verified coverage. Same-revision ordinary suffixes can
enrich a matching binding without duplicating indexed content. Terminal report
repair preserves indexed answers in the current prompt rather than flattening or
promoting provisional/failed content; an earlier prompt's indexed answer does not
disable repair of a later ordinary preview.

All Zevria panes use the same inline action presentation: full readable queries,
URLs and find patterns, counted adjacent detail-free actions, one incomplete
marker per failed/interrupted attempt, and **completion unconfirmed** where no
provider terminal evidence exists. Readable reasoning runs have one heading;
opaque/empty reasoning is hidden. Search-enabled answer parts update in place as
text and citations arrive, with completed native replay authoritative at success.
Finality bookkeeping stays internal; the version-1 display contract is unchanged.
Metadata-free hosted activity uses received order:
a new action separates surrounding text, but sparse updates to an existing
action do not. Exact source ordering is not inferred from flattened channels.

Native restoration and ACP replay share a display-copy reconstruction pass.
Only current version-1 attempts with required `revision`, `presentation`, and
`terminal` fields are accepted. Explicit `zevria_display_attempt` links authorize
restoration from native replay; matching profile, item IDs, text or proximity do
not. Unlinked attempts retain their saved presentation and terminal evidence,
without a version-based ordering notice. Missing text and terminal outcomes are
not invented, and action status alone is not provider terminal evidence. The extension and its normalized events are never provider replay,
model input, native tool execution, permission evidence, report-extraction
sources, captured answers/choices, or Plan/ensemble authority.

## Completed response and projected request usage

ACP standard usage represents the next fully prepared request, not the latest
completed response. `UsageUpdate.used` is `projectedInputTokens` and
`UsageUpdate.size` is that profile's hard `inputTokenLimit`. A separate physical
`contextWindowTokens` value remains in Zevria metadata. Switching Build ↔ Plan
or running Review/Explore can therefore change both limits without opening a
new root session.

`ContextUsageUpdated` supplies the projected count and its authority:
`exact`, `usage+delta`, or `conservative estimate`. The
metadata also includes `projectedProvider`, `projectedModel`, and
`projectedRole`. `UsageUpdated` remains accounting for the latest completed
provider response. Its existing `inputTokens`, `cachedTokens`, `outputTokens`,
`totalTokens`, `provider`, `model`, and `role` fields are preserved, with
`responseInputTokenLimit` and `responseContextWindowTokens` identifying that
response's profile.

The ACP frontend retains the latest response and projected-request snapshots
independently and combines them when either event arrives, so a later failure
or reversed event order does not erase one quantity. Synthetic local-summary
compaction inference remains suppressed and does not fabricate completed
response usage.

## Stdio and logging

In ACP mode, stdout belongs exclusively to newline-delimited JSON-RPC. Zevria branches before Ratatui or crossterm initialization and does not emit terminal escape sequences, status frames, or tracing output there.

Tracing continues to the configured file-only logger, normally:

```text
~/.zevria/logs/zevria.log
```

Set `[log].directory` to change that location. Stderr remains available for process-level startup or fatal diagnostics; ACP clients should treat stdout as the protocol transport.

[Agent Client Protocol (ACP)]: https://agentclientprotocol.com/
