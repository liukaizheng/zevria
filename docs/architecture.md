# Provider-Routed Responses Architecture

ACP-backed planning and review orchestration is described in [ensemble.md](ensemble.md). The editor-facing ACP agent frontend is described in [acp-agent.md](acp-agent.md). Automatic supplemental file instructions are described in [guidance.md](guidance.md).

Zevria is a provider-neutral session engine assembled with a Responses wire
adapter at compile time and a validated provider/model catalog at startup. It
deliberately does not perform dynamic plugin loading: every configured provider
is one OpenAI Responses-compatible endpoint, not a protocol adapter choice.

## Hosted search boundary

The provider's typed, opt-in `web_search` settings and effective task allow-list
meet at `prepare_turn_request`; every inference transport/replay and exact count
uses that snapshot. Hosted calls never enter Rig function-tool dispatch. The
adapter inspects raw search lifecycle, text/refusal deltas, added/done parts,
annotations and terminal events independently of the native output ledger.
Search-enabled answers stream as soon as readable text arrives, including when
search is advertised but unused. The shared HTTP/WebSocket reducer separates raw
provisional text from annotation-bearing completed text and sanitized display
copies. Finality is internal reducer state, not a new transcript format. Text-done
fences text deltas without fencing later annotations; completed parts/items and
validated native output have higher authority. Duplicate sequences and stale
lower-authority events cannot extend a finalized part. Usable annotation additions
are rendered against original character offsets; future ranges wait for text.
Only changed parts are re-rendered, and incomplete citation tokens never gate the
surrounding answer.

`zevria-content` owns shared safe citation rendering and display-only version-1
`WebSearchAttemptRecord` values; `zevria-transcript` owns their persisted history. Every actual search-enabled transmission has an
independent attempt ID, including HTTP retries, stale continuation recovery and
WebSocket-to-HTTP fallback, not socket handshakes. The monotonic attempt revision
is independent of channel transport revisions. Indexed presentation retains the
output index, summary/content-part identity, available item ID, and native-tool
call binding. Readable reasoning and provisional answer/refusal text enter this
trail; citations can revise a growing part without changing its source address.
The revision advances when visible presentation changes. Completed native output
reconciles successful answers authoritatively, including removal or correction of
provisional parts. Encrypted/redacted reasoning remains exclusively in native
replay.

`AssistantStreamSnapshot` can contain an ordinary message, an ordered attempt,
or both. An activity-only snapshot is not a stream clear. All root and child
relays retain this distinction. Attempt-backed content is reconciled in place,
not duplicated in a generic streaming tail. Committed assistant records carry
`zevria_display_attempt`, an explicit display-only join independent of message
IDs or text equality. Successful canonical messages still come from
`ProviderReplay`, never from the display trail. Both ordinary and indexed answer
projections use the same readable display text. The ordinary stream and attempt
collector retain latest state, not a token-event history; slow consumers may skip
unpainted intermediate previews without delaying text until response completion.

Raw event ingestion and collection happen synchronously before cancellable
publication. Core uses an immutable `OwnedModelRequestItem` request snapshot so
it can persist display checkpoints while the provider future is borrowed. The
finalized-attempt handoff has its own acknowledgement channel: acknowledgement
follows the completed-record write and never waits for frontend capacity. A
retry waits for this result before clearing content or transmitting again. Its
finalized display publication also precedes the next lossy preview. On
cancellation core releases the provider future, drains the remaining collector,
and skips revisions already checkpointed. Failed writes retain memory and report
degraded persistence, never durable success. These checkpoints do not commit a
deferred canonical answer or workflow result. An uncheckpointed active attempt
can still be lost on process termination; cancellation is not process recovery.
This includes provisional answers: no per-token checkpoint is added. Saved
in-progress attempts restore as interrupted display copies, retaining their
partial answers without promoting them into canonical model history.
The owned request adds one owned model-input snapshot per active completion;
checkpoint rewriting uses the existing completed-batch persistence path.

Checkpoints retain the full presentation until their successful native replay
commits. At that exact-ID link commit, one atomic completed-batch rewrite stores
both the replay and the compacted version-1 attempt: `presentation` becomes `[]`
and `presentation_elided: true` records that its readable content lives in the
replay. This storage-only change leaves the revision, identity/profile/response
ID, outcome, ordering evidence, activity and terminal evidence unchanged,
including activity observed live but absent from the final ledger. Failed,
interrupted, retried and unlinked attempts stay full; plain assistant bindings
without a native ledger also keep their presentation. Failed link rewrites leave
the full checkpoint on disk and retain the completed pair in degraded memory.

TUI/ACP restoration re-derives the presentation from the linked native ledger
on a display copy and clears `presentation_elided`. Loading rejects an elided
attempt without its later exact-ID native replay before any writable tail repair.
A torn later append remains recoverable while the complete pair is intact;
truncating the linked replay itself is corruption, not recoverable append debris.
Attempts remain excluded from model input, and neither the replay envelope nor
the provider's cacheable prompt prefix changes.

Replay-backed request items borrow or own the same private-field `ReplayMessage`
pair. Its constructor derives the canonical message from the native ledger once;
trusted cloning never decodes that ledger again. A completed `ModelResponse`
privately owns a validated `MessageRecord`, which moves into transcript admission
without rebuilding the message. Display binding changes only metadata. Native
output validation still precedes installing provider continuation state.

The feature-gated Criterion harness measures these ownership boundaries,
serialization, loading, validation and transactions independently. The opt-in
`owned_request_snapshot_memory_probe` supplements it with isolated RSS
observations; neither RSS nor cumulative allocation bytes are exact live-heap
measurements. See [transcript-performance.md](transcript-performance.md) for the
pre-refactor baseline, staged results, fixture dimensions, toolchain and exact
RTK-routed reproduction commands. The owned snapshot, frontend event messages
and provider continuation state remain real ownership costs.

Main, native-subtask and ACP panes share inline web-action blocks. Queries keep
the supplied array order and repeated fields are deduplicated within an action.
URLs, patterns and queries wrap without copy truncation or raw JSON. Terminal
escapes and directional controls are removed while readable line breaks remain.
Adjacent detail-free actions within an attempt form a counted row; late details
split that row and mixed terminal outcomes remain counted. Known action and
part identities preserve block layouts, selection and viewport anchors. Native
tool-result correlations use block/call identity rather than stale array indexes.
Only consecutive visible reasoning shares one `reasoning` heading, including
committed/streamed boundaries; empty, whitespace-only and opaque-only reasoning
has no heading or selectable row.

Attempt outcome and action terminal evidence are separate. An explicit completed
action stays completed even when its response fails. An action without provider
terminal evidence shows `•` (completion unconfirmed) after closure, rather than
continuing to run or being labelled a provider-confirmed failure. Each retained
failed/interrupted attempt has one **Incomplete response · failed/interrupted**
marker. Advertising search without observing an action creates no action row;
readable abandoned content can be retained without manufacturing an action.

Only current version-1 attempts are accepted, with required `revision`,
`presentation`, and `terminal` fields. Shared native/ACP reconstruction operates
on a display copy without rewriting the log and requires an explicit
`zevria_display_attempt` link to restore from native replay. Item IDs, matching
text and proximity do not authorize a join. Unlinked attempts retain their saved
presentation and terminal evidence; action status alone never proves completion.
Assistant records without bindings remain ordinary assistant records. There is
no legacy upgrade or version-based ordering notice.

These standalone records are absent from model input, compaction source and
capacity accounting. The successful native Responses ledger remains exact;
foreign-profile projection renders source-title/URL links and removes hosted or
opaque items. Destination-aware estimates count that expanded portable text.
ACP keeps separate hosted IDs and standard Search/Fetch/status projections;
versioned notification metadata supplies richer ordering/evidence to the host.
Ordinary answer growth emits ACP suffixes. A non-prefix citation correction starts
a new authoritative segment because generic ACP clients cannot retract text.
Zevria's metadata-aware reducer retains the latest verified coverage evidence per
emitted segment/range, allowing delayed or coalesced previews of superseded
segments to be covered without duplicating the indexed answer or hiding unbound
suffixes. Native source identities preserve selection and viewport anchors while
text and citations grow. A newly received metadata-free action separates text, while a sparse update of
an existing action does not. Display events have no report, permission, captured
choice or workflow authority. See [ACP metadata](acp-agent.md#ordered-response-display-metadata)
and [configuration and controls](responses-compatible.md#provider-hosted-web-search).

## Dependency direction

The workspace has nineteen packages. Arrows below mean production dependencies;
“lower values” means only the foundation/content/instructions/model/workflow
owners a package actually uses. All consumers import the owner directly: core
is an engine, not a shared-contract umbrella.

```text
foundation
content       -> foundation
instructions  -> foundation + content
model         -> foundation + content + instructions
workflow      -> foundation + content
transcript    -> lower values
session-api   -> lower values
core          -> session-api + transcript + lower values
responses     -> foundation + content + instructions + model
provider      -> responses + session-api + lower values
tools         -> session-api + lower values
acp           -> session-api + transcript + lower values
theme         -> external rendering/color dependencies only
tui-widgets   -> theme + lower values
tui-input     -> tui-widgets + theme + lower values
tui           -> tui-input + tui-widgets + theme + session-api + transcript
                 + lower values
ensemble      -> acp + session-api + transcript + workflow + lower values
app           -> core + provider + tools + acp + ensemble + theme + lower owners
zevria        -> app + selected frontends + directly used public domain APIs
```

- **Foundation/content:** identities and policy values, generic filesystem/config
  helpers, tool/question/subtask data, prompts/images, citations and display values.
- **Instructions/model/workflow:** instruction discovery/pins/directives and unchanged
  prompt bytes; validated model/replay/request values, compaction and estimates;
  pure Plan/ensemble/review state and reducers. Rig messages remain shared model data.
- **Transcript/session API/core:** storage, journals and shared replay validation;
  storage-independent commands/events/provider interfaces and execution channels;
  the private session state machine, admission, capacity and durable workflows.
- **Responses/provider:** deterministic wire conversion, parsing and accumulation;
  transport, routing, retries, attempt publication/acknowledgements and continuation.
- **Theme/widgets/input/TUI:** one process-wide theme; reusable rendering primitives;
  composer/commands/questions; the coupled App reducer, frame policy and terminal loop.
- **Ensemble/app/binary:** ACP worker supervision and its configuration; frontend-neutral
  application configuration, runtime construction, registries, leases and services;
  CLI dispatch, terminal setup and frontend selection.

Only app depends directly on core in production. Adapters have no transitive
engine dependency. Session API has no storage dependency, Responses has no
runtime/storage dependency, and app has no terminal frontend/widgets/input
dependency. Integration tests may depend upward through explicit dev-dependencies.
The binary's manifest-boundary tests enforce these rules without invoking Cargo.
See [the crate-split report](crate-split/README.md) for exact resolved edges,
source/test accounting and validation evidence.

## Captured file guidance

The instructions crate's `guidance` loader captures only `~/.zevria/AGENTS.md` and the supplied
startup workspace's `AGENTS.md` at session opening/resume, using shared contained
regular-file reads. `GuidanceState` is independent of `RootCapabilities`:
roots and native workers capture independently; Explore and Build children inherit
the parent's immutable snapshot without child-root discovery, skill inheritance,
I/O, or repeated warnings. Cached values flow into the deterministic instruction
set used by admission, previews, dispatch and accounting.
`refresh_application_guidance` only drains opening diagnostics; it is not a
filesystem reload API.
Normal resumed dispatch rebuilds instructions from captured guidance. Maintenance
has a separate preparation path so immediate post-resume compaction is guided
even before that first normal dispatch.

The application preamble and nonempty `guidance:global`/`guidance:project`
components render as Application guidance and File guidance in the stable prefix
of top-level `instructions`. File wrappers contain only scope, source, and explicit
user-controlled-body boundaries. The engine protocol alone states project-over-global
precedence, workflow constraints, and the inability of complementary guidance to
grant capabilities.
Empty resume captures contribute no obsolete body. Existing startup notices
deliver warnings. Frontend events and persisted JSONL bytes are unchanged; the
transcript stores neither instruction sets nor checkpoint instruction state; only
skill bodies/revocations persist as ordered directives. See [guidance.md](guidance.md) for the byte cap, Unix/Windows handle-relative safe-reader
backends, capture timing, trust boundaries, and persistence/privacy rules.

## Session contract

Frontends send three explicit `SessionCommand` families:

- `Turn(TurnCommand)`: submissions, transcript edits, skill invocations, manual
  compaction, ensembles, versioned Plan decisions, and typed handoffs. Each
  dispatched command gets a monotonic `TurnId`; `TurnCommand::mode()` is total.
- `Control(ControlCommand)`: question answers, targeted/untargeted cancellation,
  run/turn/worker-scoped Plan feedback, confirmation and optional baseline selection, and shutdown. Control
  traffic never allocates a root turn ID or enters the work queue. Worker controls
  have their own correlated request IDs and a bounded, nonblocking router.
- `Manage(ManagementCommand)`: correlated root mode selection (`SetMode`), model selection, and skill management.
  Mutations are idle-only; skill queries can use the captured live projection.

`handle_command` dispatches the same families directly, including question
answers through the responder. `handle_turn` runs a child under its parent's
`TurnContext` (identity and cancellation) without allocating another ID. Each
context's `build_subtasks` capability is copied from its effective policy rather
than inferred from nominal mode. `TurnPolicy` also owns `WorkspaceContract` and
an optional `WorkspaceBinding { root, startup }`; request-shape fingerprints include
all three fields.
`ResumeEnsemble` is private `TurnWork`, reconstructed only from durable records.

Ensemble Plan `/baseline` confirms an exact eligible revision and selects one
worker as synthesis foundation/preference authority; `/unbaseline` retains its
confirmation. A shared run reducer atomically derives displacement from the one
accepted root marking event. Original confirmation and later marking receipts
retain independent turn identities. Root review v1 and worker journal v1 use
additive optional metadata; missing means no baseline and unmarked synthesis
bytes stay unchanged. Root-derived worker IDs drive live/historical badges,
never worker audit text. The final confirming control immediately freezes the
entire participating set. Synthesis precedence is subordinate to explicit
requirements, captured choices and repository facts. Canonical publication
remains `PlanRecord::Published`, without Ready approval or a Build transition.

The persisted `EnsembleStart` selects direct publication only for Plan with one
original worker. After the same explicit revision seal, worker finalization and
`ReportsReady`, the host publishes the sealed snapshot's exact Markdown without
root model/tool calls or provider token counting. Multi-worker runs still synthesize
even if abandonment leaves one survivor; Review is unchanged. `PlanRecord::Published`
carries typed synthesized or confirmed-worker provenance (run, worker, host revision).
Only the latter permits noncanonical sections/title; nonemptiness and the 128 KiB
artifact ceiling remain, with eligibility checked before sealing. Replay cross-checks
the original selection, seal, exact Markdown and ordering. Projection and both
implementation handoffs preserve direct artifact bytes, including newline behavior.
Recovery after reports never reconstructs synthesis for direct runs; recovery after
publication appends only missing completion state. A shared retained-item completion
tail commits the engine notice, artifact and ensemble completion, then emits Plan
state and projection warnings. Explicit version-checked implementation remains a
separate authorization. These choices live in typed state/transcript-tail evidence,
not dynamic stable instructions or tool schemas.

`zevria_content::prompt::UserPrompt` is the narrow ordered text/image user-input contract.
Immutable `PromptImage` values validate encoded bytes, MIME, dimensions and decode
budgets; Debug is metadata-only. Session commands, skill arguments, worker inputs,
and ensemble starts retain these values rather than reconstructing transport
from text projections. Canonical user messages contain embedded Rig image blocks.
Root and worker record IO is bounded at 64 MiB, including mirrored inputs.

The TUI composer tracks registered token ranges and draft generations. Ctrl-V
uses an explicit single-operation native clipboard service off the event loop;
results address the originating pane/draft, not the visible pane. Optimistic
submission stages complete snapshots across acceptance/rejection. ACP adapters
preserve ordered embedded content, and provider accounting replaces payloads only
in temporary estimates while charging the shared approximate image allowance.
See [image input](image-input.md) for user behavior, limits, and privacy.

`SessionEngine<P>` is the single owner of history and emits:

- `TurnStarted { turn_id, message, mode }` after the user message is durably
  accepted into local history;
- coalesced `StreamUpdated`/`StreamCleared` state tagged with `turn_id`;
- tagged `Intermediate` and `ToolResults` lifecycle events;
- tagged `TurnCompleted`, `TurnRecovered`, `TurnRejected`, `TurnFailed`, or
  `TurnCancelled` terminal events; manual compaction and non-inference Plan
  decisions retain their domain-specific completion acknowledgements;
- `SubtaskLaunched`, `SubtaskSession`, and `SubtaskStatus` for child-subsession
  lifecycle and forwarded child events (see "Subtasks" below); and
- authoritative `PlanStateChanged` snapshots for live transitions (not startup
  readiness), typed handoff events, and
  non-blocking `PlanProjectionWarning` events; and
- persistence-health events for frontend-visible transcript failures; and
- `UsageUpdated` events carrying the latest completed response usage, exact
  active provider/model profile, `ModelRole`, input ceiling, and physical
  context window; and
- `ContextUsageUpdated` events carrying the authoritative projected size,
  count source, trigger, input ceiling, and physical window for the next
  prepared request.

Replay and execution availability are explicit engine-owned enum states:

- `SessionReplayState::Valid { skills, plan }` contains both successful reducers.
  `Failed(SessionReplayError)` replaces the entire bundle on an unexpected
  committed replay failure, retaining the failing domain(s) and diagnostics but
  no stale skills or fake Idle Plan. It is a terminal latch, not a recovery mode.
- `EnginePhase` is `Idle` or `Turn { skill_queries }`. `RootCapabilities` owns
  optional model management, Plan projections, questions, and the ensemble
  launcher. Model management's service/revision/generation/session identity is
  independent of phase. A confirmation preview requires idle management; entering
  a turn invalidates it without moving or losing the management context.
- `ContextState` groups per-profile usage, per-role snapshots and compaction
  arming, model-count failures, and `TurnInputCountState`. Prepared exact counts
  are one-use and bound to turn and role; same-turn count failures suppress retry.
  History/catalog invalidation drops prepared counts, not failure suppression;
  model replacement resets the group. `SkillState` owns the installed immutable
  registry/catalog, management service, and separately captured mode permissions.
- `AcceptedTurn` carries request-local first-dispatch compaction state; it is not
  a loose engine field. `DispatchState` tracks subsequent model/tool rounds.

`with_transcript_items` and `with_history` validate both replay domains before
adopting history and return `Result<Self, SessionReplayError>`. Restored context
telemetry and skill/Plan/context-token accessors are fallible too. Invalid
sessions cannot be restored, even if an edit would discard the malformed tail.
A live committed replay failure stops dispatch, cleans up turn resources, and
returns the original typed error from `run` or either direct command API. It
emits no recoverable terminal event or synthetic transcript error, performs no
shutdown durability repair, and cannot be reseeded into service. Repair must
happen outside that engine. Provider errors, conversion requirements, capacity
rejection, and recoverable persistence degradation remain nonfatal.

`run` checks the installed replay latch at startup and idle dispatch rather
than re-reducing the conversation. Construction/restoration installs valid
state, and replay-affecting mutations keep it current; direct command APIs
still perform their own refreshes. The runtime then discovers writable
unfinished ensemble recovery and dispatches it before prequeued frontend
commands, supervises active work and control traffic, and attempts best-effort
transcript durability repair only after normal shutdown or channel exhaustion.
Read-only sessions neither queue automatic recovery nor repair the transcript.
One supervisor handles both turns and model maintenance. Incoming turns queue
FIFO during either kind of work. Management mutations are rejected as busy,
while skill queries read the captured projection. `CancelTurn { None }` cancels
whichever work is active; a specific ID only targets a matching turn, never
maintenance. A matching model-management Cancel cancels its own request.
Shutdown cancels and drains active work, then discards queued turns.

The engine implementation lives in `core/src/session/`: prompt admission,
capacity, compaction, model-loop finalization, Plan, ensemble, records, and tools
have private modules. Public command/event/provider and execution contracts live
in `session-api`; pure policy identities live in foundation. Transcript replay
validation is shared by storage and the engine, while the terminal replay-failure
latch remains private to core.
Prompt phases are `admit_prompt` → `measure_prompt` → optional checkpoint →
re-measure → `accept_prompt`. One `CapacityAssessment` classifies payload,
conservative, usage-based, and exact counts at the trigger/input-limit bands.
A rebuild discards the old usage candidate and recounts a previously exact
request. Each prepared request attempts automatic compaction at most once.

Before acceptance, `Rejection` can only produce an event: validation, capacity,
configuration, failed required commits, and persistence-gate refusals emit
`TurnRejected` without appending an Error row. Pre-commit cancellation emits
`TurnCancelled` without a row. An append checkpoint may already be durable and
survives a later prompt rejection; an edited checkpoint is folded into the single
atomic tail rewrite. After acceptance, `Failure` retains completed provider/tool
work and records terminal Error rows (plus ensemble terminal records where
applicable). `finish_rejected`/`finish_accepted` own failure terminals, and
`finalize_turn` enforces model submission gates; `commit_turn_completion` shares
successful model/direct Plan persistence, projection, and ordered
completion → Plan state → projection-warning publication.

Runtime owners observe returned errors separately from clean completion and
task panics, using the existing runtime-exit channel. ACP prioritizes available
runtime exits, resolves pending requests, rejects later work as unavailable,
and shuts down once. A failed child of either kind reports one failed child outcome
without terminating its healthy parent.

Lifecycle traffic uses a bounded MPSC channel (256 entries by default), while
streaming uses a private Tokio `watch` snapshot: each root or child update
replaces that target's previous complete value, so token-rate deltas cannot
grow an event backlog. These are implementation details behind one public
`SessionEventReceiver`, which yields `SessionUpdate::Lifecycle(SessionEvent)`
or `SessionUpdate::Streams(SessionStreamBatch)`. Consumers cannot split or
borrow the internal sources.

Every publication receives a checked, channel-local `u64` sequence shared by
lifecycle and stream publishers. A lifecycle sender reserves bounded queue
capacity before taking a sequence, then assigns and publishes synchronously;
a failed `try_send` therefore consumes no sequence. Child stream revisions
are local to the child channel, so forwarding one assigns a fresh sequence on
the parent channel.

The receiver prioritizes queued lifecycle events and rechecks them after every
stream wake. Each delivered lifecycle sequence becomes a causal fence for the
root or named subtask pane it targets. A stream state is emitted only when it
is newer than both that pane's fence and its last emitted state, and each batch
contains only targets that passed those checks. Thus an unread `Intermediate`,
tool-result, retry, or terminal event structurally discards an older preview,
while a preview already consumed before that lifecycle event is cleared by the
authoritative `App` reducer. Root and child fences are independent. Empty
batches are skipped, and receiver closure is reported only after both internal
sources have closed and all eligible updates have drained.

## TUI state and pending/active contract

The Ratatui frontend keeps `App` as its public façade and input/event
coordinator, but its logical state is a product of private state machines:

- `SessionState` owns the next idle mode, pending/active operation lifecycle,
  immutable in-flight mode and exact `ModelRole`, one event-authoritative
  profile/usage/context telemetry slot per role, correlated mode selection, and persistence health;
- `WorkflowState` owns the authoritative `PlanWorkflowState` snapshot separately
  from local review visibility and choice; identical snapshots preserve dismissal;
- `ConversationState` owns committed history, native tool correlation,
  subtask attachment/status, coherent ensemble worker rows, selection walking,
  and transcript/ACP projection;
- `ComposerState` owns the grapheme-safe text editor, command/skill registry,
  and completion highlight;
- `InteractionState` owns Normal, Insert, or Selecting focus, with Message and
  Block selection scopes, persistent selection-reveal intent, Normal-mode `v`
  entry and its timed `Esc Esc` alias, block-scope `yy`, and `gg` / `z` chords;
- `FoldState` owns pane-local explicit folding intent: inclusive history-range
  Span keys sit above Message keys, which sit above stable presentation block
  identities and selectable non-conversation item indices. Unfolding an outer
  layer preserves the inner folds;
- `EditState` owns either a recalled draft or an edit waiting for durable
  acceptance;
- `DraftSubmissions` owns immutable staged/rejected drafts and replacement-generation
  checks; `WorkerReviewUiState` owns the bound worker, ordinary/urgent controls and
  the accepted-result-to-snapshot handoff;
- `PaneState` explicitly distinguishes Root, LivePlanWorker, HistoricalAgent and
  SubtaskInspect. Appearance, titles, diagnostics, model-role overrides and external
  ACP context accounting do not decide composition capability; and
- `ViewState` owns application-surface layout caches, viewport/follow intent,
  completion and Plan-choice viewport state, and rendered composer and
  conversation-window measurements.

`workspace::SessionViews` owns stable pane identities and global workspace chrome
in addition to the root and child panes. `OverlayController` owns skills, sessions,
models and questions. One immutable surface snapshot supplies both overlay paint
order and input precedence; captured input cannot fall through to a pane. A question
above a picker owns input while the picker retains its selection. Model management
has explicit loading, model/reasoning selection, conversion preview, pending-change,
cancelling, error and settled phases, with request/mode/scope/revision validation.
Cancellation retains the submitted target until settlement.

`runtime` owns the terminal loop, channels, clocks and clipboard execution rather
than pane collections. The surface policy scopes pane navigation and cursor
ownership; repeats/releases and unsupported modified keys are ignored before
activation. Independent capabilities govern draft editing, work submission,
transcript editing and session management. Normal focus is not an editing lock.
See [TUI interaction](tui-interaction.md) for the input/cancellation matrix and the
remaining architectural migration work.

The workspace header retains the absolute startup directory,
discovers the nearest ordinary/linked-worktree/submodule Git directory, and
refreshes the directly parsed `HEAD` label after root tool results and terminal
focus regain. Pane navigation and modal state therefore reuse one workspace
identity rather than copying repository metadata into each `App`. On the canvas,
a bold `` home icon, the bold path leaf, and the bold right-aligned Git label
(` main` or ` detached@01234567`) share the visible pane's status accent
(Build-role mint, Plan blue); parent directories are muted and a leading
home-directory prefix is displayed as `~`. The home icon (`U+F015`) and
Powerline branch icon (`U+E0A0`) require a compatible font, such as a Nerd Font.
Fitting uses terminal display widths and grapheme-safe clipping. Only the
display path is abbreviated; Git discovery retains the absolute startup path.
Workspace-header presentation uses `/` separators on Windows, Linux, and macOS
(for example, `C:/work/zevria` or `~/work/zevria`). Home-prefix matching happens
on native path components before display conversion.

Generated workspace-relative file references use the same `/` representation
through discovery, ranking, and completion insertion. Native `Path`/`PathBuf`
values still own workspace identity, Git discovery, and filesystem traversal.
Only native Windows separators are normalized: literal backslashes in Unix
filenames remain filename characters, including during basename ranking, and
completion quotes/escapes them without rewriting manually typed references.

The crate-private `FrameLayout` owns terminal-frame geometry: outer padding,
fixed-row allocation, the single flexible transcript region, the optional
protected first-row workspace header, protected modal bounds, external
transcript chrome, and deterministic short-terminal degradation. At 21 rows
and above the header occupies the existing top padding and the spacious gap
renders a border-colored `─` rule across the same inset width on the canvas
before the transcript, without allocating another row. In compact frames of at
least six rows the gap collapses and the header consumes one flexible
transcript row. `ViewState` continues to own cached content, viewport intent,
and rendered composer measurements; frame geometry is not persisted
application state.

A crate-private `Viewport` and half-open `RowRange` model all overflow in
wrapped terminal-row coordinates. `ViewState` owns the conversation, composer,
command/skill completion, and Plan-choice instances; `SessionPicker` and each
question prompt own their modal instances. Model lists, reasoning choices,
conversion details and skill views use the same primitive; selection is revealed
within measured local bounds, even in short terminals. The primitive owns only geometry
(reconciliation, panning, jumps, target revelation, visible ranges, and
scrollbar projection), while transcript follow/re-pin policy and question
manual-pan/reveal intent remain with their views. The transcript projects its
scrollbar into the terminal's outer-right gutter, with the adjacent gutter cell
reserved for selection brackets. Composer, command/skill menu, picker,
question, and Plan surfaces retain local right-edge scrollbars. No scrollbar
reduces the width used to measure or wrap its content.

Every engine-backed frontend action—submit, transcript edit, skill, ensemble,
manual compact, and each versioned Plan decision—must enter
`SessionActivity::Pending` before its `UiAction` is returned. The pending and
active records carry `ModelRole` independently from `SessionMode`: ordinary
Build requests (Standard or orchestrated) use Build, ordinary Plan uses Plan, and
Plan and Review ensembles use Plan and Review. Typed ensemble edits preserve
their workflow role, and child panes override nominal Build turns and compaction
with Explore or Builder. Orchestration is request behavior, not a mode or model role. An
authoritative `EnsembleStarted` resolves from its workflow; `TurnStarted`
resolves from the pane override or announced mode. This local lock prevents
duplicate commands from racing engine acknowledgement. A pending operation has
no `TurnId`, so `Ctrl+C` sends `CancelTurn { turn_id: None }`; after
`TurnStarted`, `EnsembleStarted`, `PlanHandoffStarted`, or `CompactionStarted`
promotes it, cancellation carries the learned ID. A terminal event is accepted
only for the exact active ID or for the sole pending operation whose ID is not
known yet, but a turn terminal cannot settle correlated model management.
An early `TurnRejected` from `Compact`, `ResolvePlan`, or any other admission
failure unlocks the UI and restores a pending Plan dialog without interrupting
accepted tool rows or pruning a rejected edit's original tail.

Active presentation is also typed: running turns have exactly one waiting,
streaming, or retry tail, while compaction is a separate phase. Accepted
progress replaces a retry notice. Manual compaction completion returns idle;
automatic pre-turn completion waits for the matching start, and automatic
mid-turn completion resumes the running turn. Stale starts, progress, and
terminals are inert. Pure acceptance predicates run before tail/retry mutations.
Late subtask status and correlated worker review/completion evidence use explicit
stable-identity exceptions: they can repair existing rows without erasing a newer
root stream or retry tail. Authoritative review/receipt revocations outrank late
outcome or ReportsReady summaries in both live and restored projections.

Native headers carry pane-local, rebuildable display ordinals: `● You · #3`
and `● Assistant · #(3 - 1)`. The parentheses describe a (turn - call) pair,
not subtraction. `DisplayTurn` and `NativeHeader` belong only to TUI presentation
state; engine `TurnId` and raw dispatch markers remain separate lifecycle guards.
Accepted user prompts (including direct skills), new ensembles, and Plan
handoffs allocate sequential 1-based display turns. Ensemble and approved Plan
handoff cards retain their workflow titles with the same ` · #N` suffix. Pending,
rejected, management, and standalone compaction operations consume no turn.
Engine ID gaps or restarts do not affect the display sequence. Each native root
or child pane owns independent numbering; ACP headers, queued/feedback
annotations, and statuses are unchanged.

An accepted `ModelCallStarted` immediately exposes a numbered, render-only
Assistant header, before any body arrives. Its identity passes unchanged into
streamed content, web-search entries, intermediate tool responses, and final
output (captured before lifecycle settlement). Visible content for that call
suppresses the empty transient copy, including during tool execution. No empty
message, selectable block, or copy payload is manufactured. Header-only tails
use the same wrapping, gutter, separators, scrolling, and bottom-follow geometry
as streamed headers. Native header identity participates in block fingerprints,
so identical content in successive calls receives its new label while unrelated
blocks remain cached. Clock-only frames reuse message and header-only caches.

Live calls count accepted model/tool-loop dispatches, not tools, content blocks,
network attempts, provider retries/continuations, token counting, or compaction
requests. The raw 1-based marker restarts for each runtime loop, but a resumed
ensemble reuses its original display turn and continues after that turn's
observed calls. A dispatch that fails before reaching the network still counts
live. Duplicate, stale, pending, pre-start, idle, and post-terminal markers do
not allocate or revive a header. Missing authoritative call metadata leaves
Assistant output unnumbered rather than inventing call zero. Retries and
automatic compaction preserve the current identity and elapsed timer. Terminal
settlement or restoration drops transient headers, not labels on existing content.

Restore rebuilds numbering from the typed `reconstruct_transcript` display
copy, never text parsing or model-input projection. Actual prompts, skills,
ensemble starts, and Plan handoffs anchor turns. Each canonical assistant
`Message`, `AssistantMessage`, or `ProviderMessage` consumes one call, including
tool-only and opaque-reasoning responses with no readable body. Tool results,
errors, directives, session metadata, compaction checkpoints (including their
replacement histories), Plan artifacts, and worker reports/reviews consume
neither number. Web activity and its revisions consume no call and receive a
canonical response's label only through an explicit `display_attempt_id`
binding. Orphan assistant content and ambiguous/unbound retry activity stay
unnumbered. Failed runtime-only dispatches are not recoverable, so restored call
numbers may differ from the previous live display; restore is not exact runtime
replay. Subsequent live turns continue after the reconstructed display sequence.
Accepted edits truncate the old tail and its numbering ledger before allocating
a replacement; pending or rejected edits preserve all labels. Display numbers
never replace `TranscriptEditTarget` prompt ordinals. None of this metadata is
persisted or included in message text, copy/recall, model history, provider
requests, or the cacheable instruction prefix.

`ConversationTail` is a synthetic, render-only final item in the scrollable
transcript, never a replacement for a real message or a persisted/model-input
entry. Pending and active operations retain a monotonic start from the local
`Pending` lock, before engine acknowledgement; authoritative work first seen
from idle starts at that lifecycle observation. Tools, retries, automatic
pre-turn compaction and its awaiting-start phase, and mid-turn compaction all
preserve this whole-operation time. Settlement or restoration drops the timer.
Elapsed text uses completed seconds (`12s`, `1m 05s`, `1h 02m 03s`).

Operation timing renders as a roleless status block without gutter accents,
including wrapped status rows and retry details. The gutter remains reserved
whitespace, preserving alignment and wrapping. Every timed phase animates with
`◐ ◓ ◑ ◒`, derived from the already observed whole-operation elapsed time in
250 ms frames. Waiting/running stays visible during tools as
`◐ running… · 12s`; streaming shows `◐ streaming · 12s` even if the snapshot
has no displayable message rows; manual compaction shows
`◐ Compacting context… · 12s`. The status bar is also index-free, for example
`Build · Waiting`; only transcript headers carry turn/call pairs.
The running glyph uses `feedback.info` independently of the muted headline and
elapsed text. Real committed and streamed messages retain
their role headers and semantic accents; opaque reasoning remains opaque.

One layout-owned blank row separates visible transcript content from status,
never a horizontal message divider. Status directly after committed history
reuses its trailing gap; streamed content or a header-only pending tail gets
its own blank boundary before status. No leading gap is added when there is no
visible transcript content. Streamed messages and pending Assistant headers
participate in chat-to-chat separators. The gap has no gutter accent or selection highlight. Timing and
its spacing stay outside both committed and streaming caches and outside
persisted/model-input data and selection/copy history.

Retry status uses the same info-colored animated `StatusIcon::Running` glyph,
followed by a warning glyph and warning-colored headline/elapsed text, for example
`◐ ⚠ reconnecting (attempt 2/5) · next attempt in 4s · 12s`.
Plain muted connection-error lines wrap below it. Positive remaining delays
round up to seconds, zero announced delay means `reconnecting now`, and an
expired nonzero delay remains `reconnecting…` until accepted progress replaces
it. Animation continues throughout positive backoff, immediate reconnect, and
expired backoff. No retry history is retained in the status block. Scrolling,
selection/copy history boundaries, and existing status-bar precedence are
unchanged. Persistence and handoff warnings still own the primary status field;
the index-free ordinary title remains in its existing detail field. Child-pane
titles likewise have no progress suffix. Narrow status bars retain width-safe
truncation. Header wrapping and selection geometry include the numeric decoration,
without adding headers, status timing, or blank boundaries to selection ranges.

Idle and pending Plan-decision states have no synthetic tail. Native child
panes time their forwarded session lifecycle; ACP-only agent panes do not
fabricate native turns or timers. `SessionViews` observes the visible app's
clock immediately before each draw. A 250 ms Tokio interval with skipped
missed ticks triggers redraws only while that visible app is busy; hidden busy
roots do not animate an idle or ACP pane. Hidden native panes still observe
lifecycle receipt, then get a fresh clock observation when displayed, so time
spent hidden is included. Clock-only redraws do not force manual scroll back
to the bottom or invalidate unchanged committed layouts. Event-driven redraws
remain independent of this timer cadence.

Hosts capture authoritative restoration state from the configured engine before
spawning it. `App::restore_session(RestorationInput)` installs transcript, workflow,
selected mode, model profiles/contexts, all reasoning levels and known persistence
health after resetting transient state. The transcript-only `restore` path preserves
configured reasoning for inspect-pane inheritance; bootstrap uses the complete
input rather than relying on setter order. Whole projection replacement gets a new
`ProjectionEpoch`, retiring selection anchors even when positions/IDs repeat.
Native, ACP, hosted and child blocks share a pane-projection allocator instead of
reserved bit ranges. Provider call IDs and durable edit ordinals stay separate.
Restoration sends no commands, live starts or clocks. ACP initializes mode and
workflow from the same host snapshot; its initial restored-Ready publication is
independent of engine events.

`PlanStateChanged` always replaces the workflow snapshot after a live
transition; it is not a startup readiness signal. A newly published ordinary Plan
Ready version opens review with **Revise** selected. Arrows clamp; `n` and numbers
select only; Enter activates an eligible exact-version decision. Esc/Ctrl+C hide
review without deciding, and `p` reopens it. Hidden Ready still blocks new work;
identical snapshots neither reopen it nor reset the choice. Ensemble `Published` displays the canonical artifact without a
dialog, implicit approval, Build transition or fresh-session handoff. Matching revision and fresh-session decisions settle their
pending operation; current-session implementation remains pending/active
through `PlanHandoffStarted` and the Build turn. While a decision is pending,
the dialog remains reviewable but confirmation is disabled, and an early
failure restores the initial or recovery dialog for retry.

Interactive Ensemble Plan uses the shared `WorkerReviewState` reducer and a
serialized root coordinator. Root review v1 records own accepted generations,
explicit revision/digest-bound receipts and revocation; worker JSONL v1 owns ACP
evidence and mirrors host input. Irreversible `/abandon` is accepted by the root,
not inferred from worker evidence. The final confirmation or abandonment and
all ordered immutable outcomes share one `WorkersConfirmed` seal; its existing
`final_confirmation` field accepts either control. At least one participant must
remain and every survivor must have an exact receipt. Abandoned outcomes are
sanitized lifecycle metadata, excluded from model input and reconciliation along
with their captured answers. Abandoning the last participant follows ordinary
ensemble cancellation without a seal or synthesis. Only after durable surviving-worker terminal
mirrors does `ReportsReady` unlock root inspection/reconciliation and canonical
publication. Worker proposals, prose and protocol completion never confirm.
Long-lived actors serialize feedback on the same ACP session, release active-work
permits during idle review, and retain healthy connections. Plan has no host
startup/prompt/user-wait deadlines; the renamed configured deadlines are
Review-only. Reliable review snapshots/results are separate from lossy previews.
A Plan worker pane is its own restricted capability, not a root interactive pane.
Root and live-worker drafts remain editable in Insert mode during pending/active
work. Work admission locks immediately; drafting, undo/redo, images and completion
remain available. The root ensemble turn and provider status labels do not decide
worker capability. A request/target/generation handoff blocks another work item
between an accepted `WorkerControlResult` and its review snapshot, in either event
order. Acceptance clears only the exact unchanged submitted draft; content edits,
undo back to identical text, pending paste and target rebinding cannot revive an old
acknowledgement. Cursor-only motion does not create a new draft identity.
Rejection restores into an untouched empty replacement or retains the rejected
draft for advertised Normal-mode `r` recovery, without replacing newer text.
Explicit freeze/rebind retires frontend requests; a terminal snapshot keeps pending
correlated acknowledgements long enough to settle snapshot-before-result ordering.

Workers reuse the root classifier/editor: popup Enter accepts the highlighted
completion and calls the existing submission path only when the completed whole
draft classifies as a parameterless built-in. Tab/Ctrl-I only complete text.
Nonblank suffixes and images are preserved without automatic control execution;
root ensemble commands and skills remain completion-only even with prompt suffixes.
No matching row makes Enter inert. Enter outside completion inserts a newline,
and Ctrl+Enter classifies the actual draft without accepting the highlight first.
Passive selection, text-only completion and protocol completion are not user
activation. Busy, recall, read-only, clipboard and pending-request protections,
exact Plan-version/worker-revision binding, and acknowledgement handling remain
owned by the existing submission path. This routing is frontend-only and does
not alter provider instructions, schemas, request construction or cacheable prefixes.
Typed worker controls retain separate eligibility from feedback/retry admission;
eligible `/cancel` and `/abandon` can be submitted while busy. Ctrl+C clears only a
focused composer's draft first; from transcript/selection it cancels eligible local
work without touching an unfocused draft. Worker cancellation never escalates to
root cancellation or quit. `/abandon` remains irreversible and has no second
confirmation dialog: explicitly pressing Enter on its valid popup completion
can dispatch it, while Tab and passive highlighting cannot. Normal
`c`/Ctrl+Y confirmation requires an empty draft and an eligible exact revision;
Insert Ctrl+Y is redo. Restored controls require an exact coordinator binding;
abandoned, sealed, historical and ended-run panes are inspect-only. Committed
transcript edits retain a stronger draft lock until durable acceptance/rejection.
Root records restore abandonment even without a worker sidecar mirror; existing
logs still receive strict format/identity preflight. Recovery skips abandoned
execution suffixes, startup, queued dispatch and finalization. Actor exclusion is
fire-and-forget after root durability: stop prompt/callback lifetimes and the
driver with bounded cleanup, release active capacity, then best-effort persist
its abandonment mirror and sanitized outcome. Mirror failure cannot undo consent
or block siblings. Scoped cleanup also closes exact questions on hard task abort.
Artifacts and prior transcript evidence are retained, not rolled back. Image-bearing inputs use v1 root-review/worker-journal formats, without migration
shims. Complete unsupported records are never trimmed as crash debris.

### ACP input presentation

ACP panes carry an explicit `TranscriptAppearance::Acp` through live worker,
frozen and historical states. Native adaptation leaves prompt metadata absent;
composer availability, titles and the diagnostics toggle never determine appearance.
The presentation reducer groups an input's existing text/image blocks by the first
block's stable ID. Only that block owns a display-only `PromptAnnotation` with
origin, host generation/request correlation, latest attempt, cancellation intent
and lifecycle. Text remains literal, image order/ordinals and per-content copy
payloads remain unchanged. No header decoration becomes a selectable block.
Plan-scope resets remain independent of origin: all accepted review inputs retain
the review scope, including Initial inputs.

The role header owns one mutable phase badge. Queued (`○`), dispatched (`◐`),
cancelling (`◐ cancelling`), recovering (`◐ recovering N`, retaining the latest
attempt number), failed (`✗`) and interrupted (`◼`) are projections of correlated
host evidence. The running glyph is info-colored; separate cancelling/recovering
explanatory spans are warning-colored.
Successful settlement remains terminal internally but removes the badge. Duplicate
request IDs, mirrored transitions, stale attempts and late terminal updates are
guarded before changing headers or echo queues. Routine lifecycle history lives in
chronological muted `Host review` diagnostics under `d`; transparent metadata does
not split an active assistant text/reasoning segment. Input success is neither a
confirmation receipt nor implementation permission.

Quiet cards use the existing panel surface and user accent, with a small inset
that shrinks at tiny widths. Literal grapheme wrapping uses the effective inner
width. The same cached physical rows measure card surfaces, role accents, headers
and selectable bodies; headers, separators and gaps stay outside body selection.
Selection overrides the panel, and decoration clips to the viewport. ACP role
accents follow each block/group instead of inheriting the first role of a mixed
entry. Cache fingerprints include appearance, inner width and header/group context;
a badge change touches only its header owner, preserving unrelated Markdown and
completed-tool layouts. Native transcript surfaces and gutters are unchanged.

Outcome/audit notices reuse diagnostic blocks with semantic tones and
`BlockVisibility::Always`; visibility, not kind, distinguishes normal notices from
raw diagnostics. Snapshot and mirror paths upsert review notices by control receipt
or generation/outcome identity. Provider, connection and payload errors remain
independent. A surviving non-replay, nonempty Markdown publication suppresses the
fresh-publication notice after successful settlement; replacement/removal and
failure follow host candidate/retained semantics. This bookkeeping never grants
eligibility. Both staged non-review replay commit/rollback and interactive review
echo suppression remain presentation-only. Typed tool-result outcomes and optional
display diagnostics persist outside model messages and travel in ACP
`_meta["zevria.toolResult"]`; malformed or uncorrelated sidecars are ignored.
Tool argument normalization happens at the presentation boundary once, while raw
arguments/output remain available for copying. Error-prefix text is not outcome
evidence. These sidecars intentionally disclose display metadata to ACP peers and
logs, without changing provider-facing result text or the cacheable prompt prefix.

### Shared status and logical tool headers

Tool-call suffixes, hosted web activity (native and ACP), subtask rows,
checklist/task/reconcile markers, prompt badges and the operation tail use the
single-cell `StatusIcon` vocabulary. `StatusIcon::color()` maps meaning to the
existing semantic theme tokens, in generated light/dark themes and the fallback:

| Icon | Meaning | Unselected foreground |
| --- | --- | --- |
| `○` | Pending | `text.muted` |
| `◐◓◑◒` | Running | `feedback.info` |
| `✓` | Done | `feedback.success` |
| `✗` | Failed | `feedback.error` |
| `⊘` | Denied | `feedback.error` |
| `◼` | Interrupted/cancelled | `feedback.warning` |
| `–` | Dismissed | `text.muted` |
| `•` | Successful update | `text.muted` |
| `?` | Unknown/unconfirmed | `text.muted` |

TUI adapters map subtask Starting/Running to Running, Completed to Done, Failed to
Failed, and Cancelled to Interrupted. Ensemble rows and pane badges use the same
semantic mapping; abandoned workers are muted rather than success-colored. Checklist and task items
map Pending/InProgress/Completed to Pending/Running/Done; unknown checklist states
use Unknown. Recorded reconcile decisions map Applied to Done and
ObjectivelyInapplicable to Dismissed. Unavailable decisions map RootQuestionRequired
to Pending and ObjectivelyInapplicable to Dismissed, retaining those disposition
labels. `⚖` identifies a disagreement row type, not an outcome.

Every visible logical tool header begins with display-only `◆`, including
specialized native tools, ordinary ACP tools, hosted activity and child rows.
The prefix and ordinary labels use `roles.tools`; each status glyph has its own
semantic foreground. Wrapped continuations, bodies, list items, diffs and
diagnostics do not acquire extra diamonds. Fully covered launch batches remain
hidden; partial-launch count notices deliberately have no aggregate status.
Completed task item text stays muted. Final `style_selected_line` styling always
overrides semantic icon foregrounds, including folded summaries.

Specialized outcome evidence is preserved: a confirmed task update uses `•`, a
dismissed question uses `–`, and task/reconcile completion without both a result
and success metadata uses `?`, never an unearned `✓`. Hosted activity retains
its per-action confirmation rules, full grouped counts and source ordering.
Error-body visibility and compact diff eligibility are independent of glyph color.
Clock-owned operation tails alone advance running frames every 250 ms; cached
rows and badges use frame zero (`◐`). Clock-only frames do not invalidate completed
content layouts. Fold summaries reserve status space and may omit `◆` only in
the explicit tiny-width status-only fallback; zero-width areas paint nothing.

Status glyphs and colors are presentation-only; they do not grant confirmation,
baseline selection, synthesis or implementation authority. Raw provider messages,
tool execution results and cacheable prompt prefixes remain unchanged. Theme
palette generation and arbitrary assistant Markdown list semantics are not part
of this refactor.

Normal-mode transcript navigation uses `Ctrl-B` / `PageUp` to scroll up one
visible page and `Ctrl-F` / `PageDown` to scroll down one visible page. `Ctrl-U`
and `Ctrl-D` scroll up and down half a page. A full page is
`max(1, visible_rows)` and a half page is `max(1, visible_rows / 2)`, rounded down,
using the last reconciled conversation viewport height, not terminal height or
the cached selectable-content window. Consecutive keys work before another draw;
after resize invalidates measurements, navigation uses the one-row fallback until redraw. Unmeasured and zero-height viewports
use the one-row minimum. Upward movement saturates at the top and detaches live
tail follow; downward movement clamps and re-pins only at the rendered bottom,
including streaming content. Insert pages its measured editing viewport; completion
pages its list. Plan review pages the reviewed transcript while arrows select
choices. Dialog lists clamp and use their own measured viewports; detail views pan
content. Home/End use the same focus-local target. Ctrl page/half-page chords belong
only to Normal and Select; composer editing, dialogs and pickers keep ownership. In inspect
panes, Ctrl-D scrolls while plain `d` still toggles diagnostics.

Lowercase `v` in Normal mode enters transcript Select mode immediately;
double Esc remains an alias and can also leave Insert mode before selecting.
Both enter **Message scope**, highlighting the whole logical message
(`HistoryEntry`) containing the lowest semantic item with any selectable content
row in the current conversation window, not the newest item in history. That item
remains the cursor for subsequent Block scope. Partially visible items count,
including an item surrounding the whole viewport. The cache retains every visible
item's wrapped row range even when unselected, preserving original content indices
through diagnostics filtering. Painting and hit testing share entry extents,
including trailing gaps; ensemble ranges measure each segment once, including
wrapped worker failures but excluding role headers and confirmation summaries.
Conversation role headers,
separators, gaps, compaction dividers, and native streaming/status tails are not
new selection units. Plan artifacts/handoffs and errors retain whole-entry
ranges; empty-content placeholders and visible ACP diagnostics remain eligible.

Entry queries only valid render-derived geometry from the actual
`FrameLayout::conversation_content` rectangle, after workspace-header and composer
allocation and viewport reconciliation. Before a usable render, in zero-area or
invalidated panes, or with only nonselectable rows visible, either shortcut leaves
the pane in Normal mode without changing scroll or follow intent. Geometry
measurements expire on content replacement, restoration, diagnostics toggles,
resizing, composer reallocation, and pane activation without unnecessarily
discarding reusable block layouts. Input validates cached coordinates against
current semantic eligibility;
there is no off-screen or newest-item fallback.

Selection entry uses the shared pane-local input path in root, worker, and
read-only inspection panes, including while work is active; it does not require
edit or submit permission. Insert mode and completion still type `v`, `Ctrl+V`
retains clipboard paste behavior, and Plan review and capturing dialogs keep
input ownership. Pressing `v` while already selecting does not toggle or reset
the selection.

Successful entry detaches follow but persistently suppresses selection-driven
revelation: only the visible portion is highlighted, and repeated renders, copy,
ignored keys, and updates revalidating the same coordinates do not scroll. The
reveal flag is interaction state, not a one-frame marker consumed by rendering.
Explicit `j`/`k` or Up/Down navigation requests revelation even when clamped to
the same first or last target, preserving the directional oversized-item behavior.

Message scope moves between entries with selectable content, placing the cursor
on each entry's first visible block. `Enter` enters **Block scope** at the cursor,
first unfolding the message if necessary. Block navigation stays inside the entry
and clamps at either end; `Enter` on a launch/worker block opens its child pane.
`Esc` in Block scope returns to Message scope on the same entry; `Esc` in Message
scope exits Select mode. Scope changes clear reveal intent and pending `yy`
without moving the cursor or scrolling. If a cursor block becomes hidden, Message
scope retargets the entry's first selectable block; Block scope instead clears an
invalid selection. Structural replacements that explicitly clear selection still
honor that invalidation contract. In ACP inspect panes, one entry is a whole
prompt cycle (prompt card, assistant content, and tools), so Message scope selects
that cycle; Block scope reaches its individual parts.

In Message scope, `y` copies every visible item's primary copy payload, joined by
blank lines; `yy` is the same as `y`, never tool output. `Ctrl-E` recalls via the
first editable block, or the ensemble's prompt row, regardless of the cursor's
block. Existing recallability and pane/busy gates still apply. In Block scope,
`y` copies the selected content or tool parameters and `yy` copies tool output;
`Ctrl-E` targets that exact block. Folding does not alter any copy/edit payload.

Block-scope `Y` is a separate **readable-list** action, advertised as `Y list` in
composer and inspect hints when space allows (compact narrow hints remain short):

| Target/action | Clipboard payload |
| --- | --- |
| Checklist `y` or `Y` | `<icon> text`, retaining `(priority)` when present |
| Subtask `y` | `kind · title <icon>`; no newly copied workspace or private fields |
| Native tool `y` | Unchanged raw primary arguments; submit-plan still copies its Markdown |
| Native tool `yy` | Unchanged result payload |
| Native task/reconcile `Y` | Compact header plus resolved operation icon, then full explanation/list rows in source order |
| Web primary copy | Existing summary text and full grouped outcomes, byte-for-byte unchanged |
| Message-scope `y` | Existing concatenation of primary payloads; only checklist/subtask primary contents change |

Readable native-list copies retain failed/denied/interrupted update context and
full item text/identifiers (including reconcile supporting text), not viewport
truncation, ANSI styles, selection decoration, fold summaries or `◆`. Rendering
and copying use shared presentation projections, never stripped screen lines.
Malformed recognized task/reconcile arguments fall back to their existing raw
primary copy; unsupported block types have no `Y` action. `Y` clears pending yank
and fold chords, so `y → Y → y` ends with primary copy, not result copy. It is not
a third yank press and does not change Message-scope copy, Insert typing, `Ctrl+Y`
or dialog key ownership. Underlying arguments, results and transcript data are
never mutated by these projections.

In Select mode, `Ctrl-U` / `Ctrl-D` jump to previous / next user targets. Message
scope skips all targets in the current entry and finds the next entry holding a
user target. Block scope searches from the cursor, including same-entry targets,
then returns to Message scope. Native text/image blocks form one user target;
mixed-role ACP entries use displayed user-role groups, including read-only
prompts. Roleless diagnostics do not split groups or become targets. Jumps land
on the target group's first visible user block. `/ensemble-plan` and
`/ensemble-review` prompt rows are targets, but their workers are not.
Assistant/system content, tools/results, diagnostics, errors, plan
artifacts/handoffs, and compaction dividers are skipped. Jumps never wrap: without
a target, the cursor and viewport remain unchanged (Block scope still returns to
Message scope). Successful jumps request revelation with the existing oversized
selection rules and stay detached in Select mode. Full-page shortcuts are not
added to Select mode.

**Transcript folding.** Select mode uses Vim `za` / `zc` / `zo` to toggle / fold /
unfold the selected **message** in Message scope, or the selected **block/item**
in Block scope. Normal and both Select scopes support:

- `zR`: clear every Span, Message, Block, and Item key;
- `zm`: give each display turn a separately message-folded prompt / Plan handoff,
  **synthetic summary row(s)** for earlier messages, and an expanded final
  eligible entry. A display turn starts at a native `Prompt` header (conversation,
  ensemble, or Plan handoff) and ends before the next such header. Its eligible
  prompt gets a Message key, keeping its own header and body summary; entries
  between it and the chronologically last eligible entry get Span keys, split
  at compaction dividers and trimmed to eligible entries. Dividers never fold.
  The final eligible entry gets no new fold; a prompt-only turn stays fully
  expanded. Headerless entries join the current turn; a leading headerless run
  is its own turn and has no prompt row. Thus an all-headerless ACP inspect pane
  collapses all prompt cycles except the last. A span covering just one earlier
  entry still gets the grouped summary. `zm` is fold-only and idempotent,
  replacing overlapping spans with the new explicit range intent without
  removing existing Message or inner folds;
- `zM`: same as `zm`, plus a Message key on the final eligible entry of every
  display turn except the most recent one by position, even if that latest turn
  has no eligible entries. An older prompt-only turn is therefore message-folded
  too. The latest turn behaves exactly like `zm`. `zM` is fold-only and idempotent;
  `zm` after `zM` does not re-expand older final entries (`zR` or Message-scope
  `za` / `zo` clears those folds).

Span keys sit above Message keys, which sit above inner block/item folds: `zm`
and `zM` create Span and Message keys while preserving existing Message, Block,
and Item intent. Message-scope `zo` or `za` on a span summary
removes only that Span key, preserving Message and inner folds; `zc` is a no-op
there. Otherwise those chords act on the Message key. Any fold mutation that
leaves the selected entry message-folded or span-folded returns Block scope to
Message scope; a selection inside a span snaps to its representative (the first
entry). Message navigation treats each summary as one stop. Native user-prompt
jumps land on the standalone prompt rows; in headerless ACP spans, jumps still
skip prompts inside the current span. Eligible entries contain visible selectable
content; compaction dividers never fold. Any intervening key disarms the `z`
prefix; in Insert, `z` is ordinary text. New entries arrive expanded even after
`zm` or `zM`; existing spans are not re-extended until `zm` or `zM` is
pressed again. A previously most-recent turn's final entry is not automatically
folded when a new turn arrives; that requires another `zM`.
Explicit intent survives width and block-revision changes; body folding only
collapses bodies with more than one wrapped row.

A span summary is `▸ N earlier messages · M more rows` (`1 earlier message` for
a singleton). Spans never contain the turn's Prompt-header row; that row keeps
its own Message-fold presentation. N counts eligible entries, excluding entries
whose only blocks are hidden diagnostics. M sums the covered entries' wrapped
content rows at the current width, honoring inner Message, Block, and Item folds
but excluding inter-entry gaps. The summary always occupies one row, clipped in narrow panes; covered
entries render no lines or gaps. `ConversationCache::entries()` remains the
rendered view, while a separate covered-layout cache retains and refreshes the
real layouts (including the representative), reusing semantic blocks on
expansion. Every selectable representative item maps to the summary row, and
only that row is selected even when the selection originated inside the span.
In Message scope, `Enter` on a summary expands only that span and stays in
Message scope on its first entry. `y` copies the hidden entries' full message
texts, joined by blank lines, with the usual visibility and primary-copy rules;
`Ctrl-E` recalls only the representative entry, not the whole span.

A folded conversation message keeps its first visible block's role/native header
and **one summary row**: `▸ <first body line> · N blocks · M more rows`. The block
count is omitted when N is at most one. M counts the wrapped rows hidden at the
current width, honoring inner folds—exactly what unfolding the message would
reveal. All visible item indices map to the summary row, and only that row is
selected. ACP summaries retain the first block's card inset and decoration.
Plan artifacts, handoffs, and errors fold below their header; an ensemble folds
its prompt, confirmation summary, and workers together below its header. Inner
text block folds use `▸ <first line> · M more rows`. Tool/subtask block folds and
single-block message folds instead retain a typed logical header with resolved
status/outcomes in the layout cache: `▸ ◆ label… <status> · M more rows` when space
permits. No status is recovered by scanning strings or assuming the first body
line contains it: multiline commands keep their actual outcome even though expanded
rendering appends it to the last command line. Multi-block messages and earlier-
message span summaries remain aggregate views with no invented lifecycle status.

Summaries use terminal display width and grapheme-safe truncation and stay within
one physical row. Status space is reserved first; optional row counts and labels
yield space. At widths too small for the standard prefix and outcome, a status-only
fallback is used; zero-width areas render nothing. Grouped web outcomes retain the
complete ordered counts when they fit. Otherwise they prioritize Failed, Denied,
Interrupted, Running, Pending, Updated, Dismissed, then Done, with an omission
indicator when possible—mixed outcomes never degrade to a misleading lone `✓`.
Expanded rendering and copying keep every outcome/count. Partial-launch folds stay
statusless and retain their missing/requested-count meaning. Normally hidden
command and successful ACP output stay hidden. Copy payloads, identities, fold
intent, selection ranges and wrapped-height accounting remain independent of the
summary's visual truncation.

Folds belong to each root, worker, or inspect pane independently: they are never
persisted, sent to the provider, or applied to conversation/model state, so the
provider's cacheable prompt prefix is unchanged. Stable identity aliases retain
block folds across ACP rewrites; Message keys survive while their entry has any
blocks. Span keys survive while their full range still exists and contains at
least one selectable item (including currently hidden diagnostics). Accepted
edits prune removed entries/items and spans truncated at either boundary;
restore or projection replacement clears folds. Fold mutations capture the top
visible conversation block and clamp its within-block offset to the new height.
Any block in a message-folded entry resolves to that entry's offset, clamped to
its rendered height; a new anchor inside the folded entry uses its first visible
block identity. An anchor anywhere inside a span resolves to the representative's
summary row; a new anchor on that row uses the representative's first covered
block identity at offset zero (or falls back to row clamping for a non-conversation
representative). The resolved semantic row is then separately clamped to the new
scrollable range's maximum viewport start. In-range anchors remain exact; anchors
near the new bottom use the nearest legal viewport start, keeping the first frame's
visible range and paragraph offset consistent without re-enabling follow for a
manually scrolled pane. Live-tail follow stays pinned to the bottom. Missing anchors, inter-entry gaps,
and entries without semantic anchors (plans, ensembles, errors) fall back to
ordinary row clamping. Selection revelation still applies afterward with the
existing oversized-selection rules. Full semantic content remains
available for copy/edit and, after entering Block scope, child inspection. The
uncommitted streamed tail is always expanded and exempt from folding.

Ordinary viewport reconciliation still clamps after resizing or content shrinkage.
The first Esc enters Normal, the second must arrive within 500 ms, and selection
exit, recall cancellation, menu dismissal, and copy/open actions retain their
existing semantics. Root, worker, and inspect panes use this shared `App` path.

Conversation reducers return `ConversationChange` with the earliest cache
invalidation boundary, selection/viewport reconciliation, and discarded
ensemble run IDs. `App::reduce` converts cross-runtime consequences into
explicit effects such as `PruneEnsembleRuns`; `SessionViews` consumes them
immediately and rebases pane navigation. There are no hidden drain queues or
one-frame replacement markers. Rendering consumes a derived conversation tail,
a semantic pane-owned status view, and action-only composer chrome. It computes
`FrameLayout` first, then renders status, borderless transcript, lower surface,
ordinary-composer hints, and any command menu without early returns that skip
shared chrome. Direct `App` rendering does not request global header space;
`SessionViews` requests it, paints the visible pane, paints the workspace
header, and then reuses the returned protected modal body for picker and
question placement. App rendering may mutate only `ViewState`;
modal rendering may mutate only viewport state owned by `SessionPicker` or
`QuestionDialog` plus the question dialog's measurement/reveal cache.
No render path mutates session, workflow, composer text, persisted state, or
question answers. Monotonic clock observations belong to input handling,
lifecycle reduction (`reduce_at`), and the runtime, never to rendering; the
renderer derives timing text and animation from already observed session time.

### Immutable selected theme and Zevria Dark fallback

The TUI owns the terminal canvas and targets 24-bit-color terminals. Every
frame begins with the process's selected semantic canvas and foreground.
Without a `[theme] name = "..."` selector, the unchanged Zevria Dark canvas
(`#282C34`) and primary foreground (`#E5ECF5`) remain the default. That built-in
canvas is anchored to
[Ghostty's static default](https://github.com/ghostty-org/ghostty/blob/main/src/config/Config.zig),
not queried from a user's terminal configuration. No production renderer emits
`Color::Reset`, named ANSI colors, or an ANSI fallback. Composer and Plan
controls use the panel surface (built-in `#30343D`), while command,
session-picker, and question modals use the overlay surface (built-in `#333842`).
Decorative and strong boundaries use separate semantic references. Any renderer that
clears cells immediately repaints its semantic surface, so
startup, session switches, compact layouts, gutters, status padding, and modal
teardown never expose the terminal's configured background. The composition
root uses the public `zevria_tui_widgets::render_startup_frame` entry point for the
Connecting and Resuming states instead of duplicating palette knowledge.

`zevria-theme` retains authored OKLCH coordinates beside the built-in references.
Its focused modules define the strict saved schema, deterministic Rust
OKLCH generator, and pure post-quantization contrast/CVD validation. One
centralized reference-to-semantic graph maps 27 concrete colors to 38 fields in
`surfaces`, `text`, `workflow`, `roles`, `feedback`, `content`, and `syntax`.
Widgets consume the public read-only process-lifetime `zevria_theme::theme()` accessor rather than
fixed constants or raw RGB values. Build, Plan, Review, and Explore keep stable
workflow accents, speakers/tools use independent role accents, and feedback
retains recognizable meanings. Markdown, diffs, status, workspace metadata,
scrollbars, prompts, tasks, and the programmatic Syntect theme use that graph.
Syntect's grammar cache is independent; its one-time color cache is constructed
from the installed tokens.

The offline `zevria theme generate --name ocean --background '#1E1E2E'` command
saves a full versioned definition to `$HOME/.zevria/themes/ocean.toml` and selects
only its name in the configuration chosen by `ZEVRIA_CONFIG`. Storage is always
global, never workspace- or configuration-directory-relative. Theme publication
uses a stable store lock and no-clobber staging; selector updates use the shared
`.skills.lock` transaction after acquiring the store lock. Both files are
prepared first, then the theme is committed before the selector. A selector
failure leaves the file intact and reports “saved but not selected”; identical
file reuse makes retries safe. Reset removes only the selector and preserves
saved files. See [themes.md](themes.md) for commands, constraints, and recovery.

Normal configuration parsing validates only the name-only selector, never a
theme file. The interactive composition root loads and validates the selected
file and installs its tokens before terminal initialization, startup frames, or
highlighting. Explicitly selected broken themes fail instead of silently falling
back. ACP/headless startup does not load theme files. First use freezes the
built-in fallback for library callers; late or repeated installation is rejected.
There is no hot reload or cache invalidation: root/child/ACP panes, session
resume, and fresh Plan handoffs share the same palette until process restart.
Configuration changes in another process apply only on a subsequent launch.

Selection is a final compositing layer: selected rows use the selection
background with canvas-colored text at both span and buffer levels (built-in
`#739BC4` with `#282C34`).
This polarity keeps selected content readable while preserving a distinct
selection boundary against canvas, panel, and overlay surfaces. It deliberately
replaces nested Markdown, diff, syntax, and role foregrounds while preserving
glyphs and bold/italic/other modifiers. Palette contract tests require normal
text pairs and every semantic foreground to meet WCAG AA 4.5:1 on all three
surfaces, selected text to remain at least 4.5:1, and strong borders, focus
indicators, and selection boundaries to reach 3:1 against adjacent surfaces.
Full-severity protanopia, deuteranopia, and tritanopia simulations additionally
enforce minimum pairwise OKLab separation for all workflow and feedback
accents. CVD simulation is a quality check, not a guarantee of identical
perception for everyone. Labels, icons, markers, and modifiers remain non-color
cues; color never carries lifecycle meaning alone.

The composition root seeds `App` with the five configured `ModelProfileRef`
assignments before restoration. `SessionViews` retains that immutable map behind
one `Arc` and shares it with every later live or restored child and ACP pane.
A configured profile is only a pre-event display fallback. `UsageUpdated` and
`ContextUsageUpdated` metadata is authoritative for its `ModelRole`; changing a
role's observed profile clears response/context companions tagged with the old
profile before accepting the new snapshot. Build, Plan, Review, Explore, and Builder
therefore never reuse another role's accounting, and stale turn IDs remain
inert. ACP panes share the map for construction consistency but deliberately do
not display it because the ACP protocol does not expose a worker model identity.

Sessions and maintenance operations have no model-call count ceiling. Provider
transport retries and timeouts, cancellation, and context-capacity admission remain
independent safeguards. The engine resolves the submitted mode through the
composition-supplied `SessionPolicies` once, then clones that immutable policy
into preparation of every `ModelRequest` in the model/tool loop. Each request
carries complete rendered `instructions`, ordered `input`, its provider-neutral
`model_role`, and an optional `allowed_tool_names` list. Application, file
guidance, workflow policy, capability modules and the eligible catalog live in `instructions`; only
skill bodies/revocations and typed request directives remain ordered developer
input. Root Build selects Build; Plan selects Plan. Child kinds select Explore
or Builder regardless of nominal Build session mode. Request-local orchestration
adds no model role, route, or assignment. UI changes cannot alter an in-flight policy or catalog.

`InstructionSet::render` purely joins modules in fixed order: engine protocol,
optional application, optional file guidance sorted by component key, workflow,
command conventions, hosted search, inspection/scratch policy, and eligible skills.
Only applicable capability modules are included. Each workflow begins with a
one-line JSON declaration in fixed field order: `scope`, `tools` (allow-list or
`"registered"`), `skills`, optional `subtasks`, optional conditional `orchestration`,
optional `workspace`, and optional
`inspection`. The renderer adds headings, not policy prose. Validation rejects
inconsistent capabilities and embedded capability modules. Custom `session.preamble`
replaces only identity and engineering practice; shell conventions remain engine-owned.
The set is included in capacity estimates and request fingerprints, never persisted. The
eligible catalog contains installed names/descriptions without activation flags
and ignores pins, preserving byte-identical instructions across activation and
tool continuation. Providers send `request.instructions` as the sole top-level
instructions. Raw system messages are rejected, not lifted.

A deterministic reducer validates typed directive content, full skill pins,
correlated tool continuations and checkpoint boundaries. Directives stay in the
live `Conversation.items` sequence and persist at the same positions as dedicated
`zevria_skill_directive` records. Each stores only version 1 and its semantic
payload; loading validates the version and re-renders the exact model-facing text.
Append, atomic rewrite and degraded-writer repair write every item. Required
updates validate the full sequence before commit. Completed tool results, embedded
full activation pins and directives remain one truthful live batch even if
persistence fails. Further generation waits for repair. Disk equals all live
items; indices match physical lines excluding blanks. Directives stay hidden in
TUI/ACP.

Resume restores workflow/model selections, full historical pins and directives
at their recorded positions. Replay validates each directive against its pin and
restores effective state before dispatch. Current captured application/file
guidance, workflow policy and catalog are rebuilt; reconciliation emits only
necessary changes. Unchanged bodies or revocations are not appended again. There
is no separate instruction-state file; skill directives are part of the JSONL.
Obsolete application/file guidance is not reconstructed.

Tools retain per-mode lists and stable registration order. Catalog availability
does not change skill-capable tool lists. Appended skill bodies/revocations
preserve exact input-prefix continuity. Mode/synthesis switches and catalog
mutations change instructions, requiring full replay even when tools are unchanged.
Standard → orchestrated → Standard Build requests instead append typed request
boundaries/corrections without changing fixed instructions, catalog, or tool order.
The existing session/profile
cache key is unchanged and never substitutes for exact continuation checks.
The prefix requirement applies to live and resumed sessions, including intentional
edit and compaction resets. With unchanged instructions, tools and policy, resume
retains the pre-shutdown input prefix. Changed guidance or catalog metadata can
still break cache reuse; no server-side retention or recomputation schedule is
promised. Only current-format histories are supported. If a crash interrupts a
current trailing directive append, the next prompt reconciles missing directives
from retained pins and current policy after valid lifecycle recovery.

The Responses router combines required provider/model/reasoning assignments in
`config.toml` with provider capabilities in its sibling `models.jsonc`. For example,
the catalog contains only providers and their models:

```jsonc
{
  "providers": {
    "openai": {
      "base_url": "https://api.openai.com/v1/responses",
      "api_key": "replace-with-api-key",
      "supports_websockets": true,
      "models": {
        "gpt-5.6-sol": {
          "context_window_tokens": 272000, "input_token_limit": 272000,
          "retained_user_tokens": 20000,
          "reasoning_levels": ["low", "medium", "high", "xhigh"],
          "reasoning_summary_level": "detailed"
        }
      }
    }
  }
}
```

Store the matching complete assignments in `config.toml`:

```toml
[modes]
plan = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
build = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "medium" }
review = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
explore = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "low" }
builder = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
```

`config.toml` also retains session/skills/theme/ACP/ensemble/command/log settings.
`ZEVRIA_CONFIG=/x/work.toml` also selects `/x/models.jsonc`; no extra environment
variable exists. First run creates missing files with `0600` permissions and stops.
`jsonc-parser` accepts comments and trailing commas, but no other loose syntax;
the catalog is never rewritten by model management. `toml_edit` preserves assignment
comments and value decoration in both inline and ordinary TOML tables.

Provider keys and model IDs are exact, case-sensitive durable identities. A
provider owns endpoint, literal credential, transport capability,
compatibility flags, exact-count policy, additional request parameters, remote
compaction, and a model catalog. A model owns its physical context window,
optional hard input-cost ceiling, retained-user budget, supported `reasoning_levels`,
and `reasoning_summary_level`. Every mode requires an explicit supported
`reasoning_level`; there is no model-wide selected/default level. JSONC `modes`
and model-level `reasoning_level` are obsolete and rejected, without migration.
There is no built-in provider catalog, field-wise provider merge, model alias,
environment credential override, or legacy `[openai]` migration.

`ModelRouting` resolves all five required assignments (including `builder`, with
no Build/Explore fallback; existing configurations must migrate) and the complete selectable catalog
at each new/resumed session's construction, using the configuration path captured
at startup (including `ZEVRIA_CONFIG`). The root `ResponsesRouter`
contains only Build, Plan, and Review routes, deduplicates exact
`(provider key, model ID)` matches, and lazily constructs each profile's
single-model runtime on first use. Every slot has independent WebSocket/HTTP
transport, continuation, retry, sticky fallback, and failure state. Build and
Plan use their matching roles; Ensemble Plan uses Plan; Ensemble Review keeps a
Build UI workflow mode but explicitly uses Review; Explore children use Explore
and Build children use the distinct Builder role. Neither child role is installed
in the root router merely because its profile is selectable.
A failure or HTTP fallback in one slot cannot poison another slot.

Build, Plan, and Review project one shared root transcript through the active
profile. Each role's resolved context policy controls pre-commit capacity,
automatic/manual/edit/mid-turn compaction, retained-user selection, skill
capacity, and usage denominator. `context_window_tokens` describes physical
provider capacity; `input_token_limit` is the hard admission and paid-input
ceiling and defaults to the physical window when omitted. Prospective requests
above a target limit can trigger target-role pre-turn compaction when the retained
prefix contains a compaction prompt. Local summarization tries the full effective
source first, then successively shorter replay-safe prefixes if size admission
fails. An oversized full source does not by itself require a different model or
fresh session. The rebuilt request is measured again before the prompt is
committed; the preserved tail can still exceed capacity even after a successful
summary. If no nonempty replay-safe source prefix fits, the source stays unchanged
and recovery requires a larger compatible configured model or a fresh session.
If the prefix has no compaction prompt, compaction is reported as unavailable.

## Session model persistence and recovery

A root transcript starts with metadata only:

```json
{"zevria_session_models":{"version":1,"build":{"profile":{"provider":"p","model":"a"},"reasoning_level":"medium"},"plan":{"profile":{"provider":"p","model":"b"},"reasoning_level":"high"}}}
```

This canonical snapshot records current selections, not creation provenance.
It contains no credentials, endpoints, supported-level catalogs, or frozen limits:
exact case-sensitive identities resolve through the **current** catalog. Both saved
selections include their required reasoning level. Resume overlays profile and
level together; an unavailable saved identity or unsupported level blocks resume
with guidance to restore the catalog entry/level or start a new session. It does
not substitute current defaults, rewrite history, or make provider calls. Only version-1
headers with complete selections are supported; no compatibility layer is provided.
Tail edits, Plan replacements, compaction, and persistence repair
preserve the current header; editing an old prompt never reverts a later selection.
It contributes no model input, retained-user content, prompt ordinal, TUI row, or
ACP replay update. Request-local orchestration uses the saved Build identity;
the header does not acquire another model role. The session-local selected workflow mode
is durable state separate from these model identities and from Plan artifacts
(see the frontend workflow below). Empty files and valid models-only roots
(with optional trailing whitespace) without a saved mode are hidden from listings/`--continue` and removed
best-effort on clean shutdown. Any mode-bearing root, including a new canonical
Build header or an explicit Plan → Build selection, is retained even with
no messages. Damaged, retired instruction-bearing, or substantive history is
never classified as safely empty. Fresh roots write models plus mode, native
ensemble workers write models, and ordinary child logs may start empty.

Root and native-worker resume require saved Build and Plan identities. Missing,
duplicate, misplaced, malformed, or unsupported metadata rejects startup before
writable repair; there is no global-default or history-inference shortcut. The
`sessions recover-models` command and automatic metadata initialization are
removed. No migration backup is created and no original history bytes are changed.
Start a fresh session or use a matching older binary for older history. Missing
profiles can still be restored under their exact provider/model catalog keys.
Root metadata requirements do not apply to generic transcript readers or ordinary
Explore/Build children, but those readers also reject unsupported persisted formats.

Runtime and engine share an ownership lease on a separate `.jsonl.lock` file,
so atomic transcript replacement cannot bypass the nonblocking exclusive OS lock.
Per-session sidecars are **transient**: final lease release, after background
shutdown and abandoned-transcript cleanup, removes eligible regular, zero-byte
sidecars best-effort. Dropping a frontend or timing out an observer does not
release an engine's ownership. File presence alone does not mean a lock is held.

Native Windows uses `workspace_state_root` (`.zevria/windows`) for every run-state
namespace; Unix/WSL use `.zevria`. Project guidance and `.zevria/skills` remain
shared inputs. All listing, continue/resume, worker recovery and cleanup paths use
that same helper. No cross-runtime migration or fallback scan is performed.

Each namespace (`.zevria/sessions` and `.zevria/ensemble-sessions` on Unix/WSL,
with the additional `windows/` component natively) retains exactly
one permanent `.leases.lock` coordinator. Updated writers hold it for short
sidecar open/lock and close-before-delete operations, never for the session's
lifetime, transcript IO, or directory enumeration. This prevents contenders from
retaining an old locking identity across deletion. Startup scans only its own
namespace on a blocking worker and reclaims inactive sidecars, including existing
retained files and crash leftovers, with or without matching transcripts. Every
candidate is revalidated and must pass an actual exclusive OS lock probe; even
an active lease without a transcript is preserved. Symlinks, nonregular paths,
nonempty/customized files, unrelated names, and transcript bytes are untouched.

Coordinator waits are bounded at startup; final release makes one nonblocking
attempt, without sleeping in a destructor. Coordination, permission, or deletion
failures can leave sidecars behind for a later startup to retry. Their presence
does not prevent a new owner if the OS lock is free. Routine cleanup never removes
or replaces the permanent coordinator, and it is not session-history retention.

**Rollout requires stopping every pre-fix Zevria process and worker using the
workspace before starting the updated binary.** Older writers ignore the
coordinator, so mixed-version deletion cannot be made safe. Do not manually delete
lock files (especially `.leases.lock`) during use. Locks require working
cross-process filesystem advisory-lock semantics; arbitrary external replacement
and filesystems with broken locking are unsupported. Explicit offline
`zevria clean` remains a destructive whole-directory purge, not this automatic
cleanup mechanism, and still requires all workspace sessions and workers to stop.
Neither kind of lease makes a concurrent purge safe. Configuration `.skills.lock`
files are unrelated and unchanged.

Downgrading is not a metadata-stripping path. Root, child, and existing referenced
worker histories are preflighted before any frontend restoration, writable repair,
provider use, or supervisor startup. An unsupported history never becomes a partial inspect-only
session, even when its model metadata is valid. A missing referenced worker log
remains a legitimate interruption rather than an unsupported format.

## Offline workspace cleanup

Run `zevria clean` from the workspace to irreversibly remove these five directories
and everything inside them, in this order (native Windows inserts `windows/`
after `.zevria/`; the other runtime's history is untouched):

- `.zevria/agent-runs`
- `.zevria/plans`
- `.zevria/sessions`
- `.zevria/subsessions`
- `.zevria/ensemble-sessions`

**Stop every session and agent worker using the workspace before invoking this
command.** Deletion starts immediately without a confirmation prompt. The command
does not acquire workspace locks, detect active sessions, or terminate processes.
Concurrent cleanup is unsupported: running sessions can lose persistence or
recreate files, and per-session leases do not protect a whole-directory purge.

The workspace is the process's startup current working directory, resolved to its
existing canonical directory—not a discovered Git root, ancestor workspace, or
configuration-file location. The command accepts no additional arguments or
options and cannot be combined with other startup modes. It runs before provider
configuration, logging, and TUI/ACP initialization, needs neither providers nor
network access, and works with missing or malformed configuration and closed stdin.

This is a whole-directory purge, not transcript-aware recovery or archival. It
includes hidden files, stale or malformed histories, plan projections, nested
artifacts, `.jsonl.lock` leases, `.leases.lock` coordinators, customized files, and
`.gitignore` guards. It keeps `.zevria` itself and all other siblings, including
`skills`, `themes`, configuration files, and unknown user files. It does not
separately clean home directory state, ancestor workspaces, or legacy `.cazean`
storage.

Missing directories are successful no-ops; repeated invocations succeed without
creating configuration, logs, storage directories, or replacement ignore guards.
Normal session startup can recreate the storage it needs later. Before deleting
anything, cleanup uses `symlink_metadata` to validate `.zevria` and all five roots.
Symlinks (including dangling links), non-directory roots, and inspection failures
other than `NotFound` reject the operation without deleting any target. Recursive
removal of valid directories removes nested symlinks without following them or
modifying their referents. These static checks do not claim protection against
hostile concurrent filesystem replacement or arbitrary external writers.

Output identifies the canonical workspace and each removed or already-absent
directory. The five removals are not atomic: on the first genuine removal error,
the command stops with a nonzero exit, the failing path, and progress counts.
Earlier removals are not rolled back, the failing directory may be partly removed,
and later targets are not attempted. Interruption can likewise leave partial
cleanup; resolve the error before retrying offline.

## Runtime model selection

Bare `/model` and `/model-session` open the same configured-profile picker in the
idle, writable interactive root composer. It captures the current Build or Plan
mode and persistence scope; request-local orchestration uses the existing Build role. The picker supports filtering and bounded scrolling, and accepts
two stages: Enter chooses a profile locally, then a supported reasoning level must
be explicitly confirmed. The current level is highlighted when supported, otherwise
the first listed level is highlighted. Left/Backspace returns to the profile stage;
Esc cancels. No selection is saved or applied by the first stage. Both commands are argument-free; a leading
space sends literal text instead. Recall and inspect-only panes cannot change
models. Turns, ensembles, maintenance, and pending Plan approval reject model
mutations rather than queueing them. Turn commands received during counting or
conversion queue FIFO and execute after maintenance, rather than producing a
synthetic turn-0 failure. Skill queries remain available from the projection
captured at maintenance start; skill mutations remain idle-only.

| Command | Active session and saved Build/Plan header | Global config |
| --- | --- | --- |
| `/model-session` | Save the captured role's complete model/reasoning selection | Neither file is written |
| `/model` | Save the captured role's complete model/reasoning selection | Save that mode's provider, model, and reasoning level in `config.toml` |

The local picker is labeled **session only — saved on resume; config unchanged**;
`/model` is explicitly labeled as changing the global default too. In Build,
`/model-session` updates the session's saved Build selection and `/model` also
updates `modes.build.provider/model/reasoning_level`. Orchestrated requests use
that same selection; there is no `modes.orchestrate` assignment or extra picker.
The other mutable role's selection, Review, Explore, Builder, external workers,
tool permissions, and other running sessions are unchanged. Fresh roots (including empty `/new` sessions and
`/implement-fresh` handoffs) use current globals from the startup-captured
configuration path.
CLI `--continue`, TUI `/resume`, and ACP load/resume restore the **last successfully
selected Build and Plan identities and reasoning levels** from the version-1 header, including
changes acknowledged immediately before closing, with no later assistant response.
Review, Explore, and Builder still use session-opening globals. Ordinary resume never rewrites
globals. The header stores complete selections, not credentials or a historical config
snapshot; removing a selected catalog entry still blocks resume under the existing
strict restoration rules. Roots with a saved workflow selection remain resumable
even without conversation messages; only empty files and legacy metadata-only
roots without a mode record qualify as abandoned.

Global configuration revisions remain separate from effective session routing:
`/model-session` does not advance the config revision. A compatible already-active
local selection is a no-op, with no transcript rewrite or route/continuation reset.
It still undergoes preflight and can require conversion. `/model` can promote an
already-active session-local choice to the global default. The frontend waits for
a request-, mode-, and scope-correlated authoritative success before changing its
profile/limits or clearing old response/context telemetry. Management is not a
prompt, assistant response, or synthetic turn.

`provider::replay` is the shared side-effect-free destination projection for
preflight, HTTP, full WebSocket input, exact counting, and compaction. Original
replay v1 records and source identities remain intact. Untagged assistants
are portable, not evidence of native provenance. Tool ownership follows preceding
call occurrences and both logical/provider handles; ambiguous or duplicate
correlation fails explicitly. Results of deliberately omitted unsupported native
tools are distinguished from supported function results. Historical tools are
never re-executed by projection.

Ordinary compatible selections need no inference. Near the target capacity
boundary the existing exact-count/fallback policy applies. A foreign opaque
checkpoint or over-capacity destination requires explicit confirmation before a
bounded, cancellable source-profile completion produces a portable text summary.
The dialog explains the token cost, possible detail loss, shared root-context
change affecting both root modes, and selected persistence scope. The source must be configured, must read the entire
effective checkpoint-aware conversation, and must fit its own admission policy.
Typed live instructions are filtered structurally; application/trusted system
components and explicit no-tool maintenance guidance are supplied separately in
input. Active skill/synthesis guidance is excluded; remote opaque compaction
is bypassed. Empty/oversized summaries and unavailable/multiple opaque sources
fail without switching. Stay with the source or start a fresh session to recover.
Startup never spends tokens to convert a foreign model checkpoint.

Preview/confirmation binds the request ID, runtime/history generation, captured
mode, persistence scope, complete source and target selections (including reasoning),
and relevant configuration revision. Altering a
session-only preview cannot authorize a global save. Both scopes validate the
configured catalog; relevant external edits may require reopening the session.
There is no automatic model-catalog reload, CLI flag, or ACP model-selection API.

Persistence is checkpoint-first, **not** a two-file atomic transaction:

1. Prepare and validate conversion plus route/context changes.
2. Revalidate configuration after asynchronous counting/conversion and immediately
   before persistence; session-only selection cannot rely on a config writer here.
3. Durably append/install a validated portable checkpoint if required.
4. **Only for `/model`:** atomically save the selected global assignment.
5. Atomically replace only the selected role in the root's Build/Plan header.
6. Infallibly install the prepared route/limits and publish success.

`/model-session` never calls the settings save service or enters a config-writing
transaction, including on retry, cancellation, and failure. Read-only validation
is allowed: a readable config need not be writable. A header failure leaves active
and durable selections unchanged and explicitly reports **config was not modified**,
with no newly committed global revision. If conversion already saved a portable
checkpoint, the error says so; retry can reuse it without another summary call.

For `/model`, settings writers retain the stable `.skills.lock` inode and
comment-preserving `toml_edit` transaction targeting `config.toml`; only the captured
mode's `provider`, `model`, and `reasoning_level` values change. Inline assignments
and ordinary `[modes.build]`/`[modes.plan]` tables are supported. The sibling
`models.jsonc` is validated and checked for external edits, but never written or
required to be writable. Complete-assignment no-ops preserve modification times. The lock remains anchored to `config.toml` (or `$ZEVRIA_CONFIG`), shared
with ordinary TOML skill/theme writers. Existing permissions, symlink protections,
and external-edit checks are retained. No configuration lock crosses inference.
If the global save fails, active and saved session selections and the default
remain unchanged, but a successful checkpoint remains. If the later header save
fails, the **global default was saved**, but active and durable session selections
remain unchanged; the rejection carries the committed global revision for retry.
The UI explicitly reports either partial outcome and any saved checkpoint.
A crash after header commit restores the selection even if the acknowledgement
never reached the frontend. Cancellation before persistence installs neither. Every synthetic source request resets continuation state,
on success and failure. Accepted genuine profile switches also reset every initialized
continuation, exact-count cache, usage baseline, and context snapshot, and re-arm
the selected role's capacity assessment. Ordinary Build/Plan mode changes retain
existing safe per-profile continuation behavior, subject to replay requirements
for changed policy or tools. Request-local orchestration keeps Build's fixed prefix.

### Per-role reasoning and cache stability

There is no standalone reasoning management request, result, or picker. `/model`
and `/model-session` are the only interactive model/reasoning configuration
commands; `/reasoning` and `/reasoning-session` are removed, with no aliases.
Review/Explore/Builder assignments remain config-only opening/resume snapshots.
All Build requests use Build; Plan remains independent even on the same slot.

Each router route stores `{slot, reasoning_level}`; the catalog has capabilities,
not selected/default levels. Before completion, compaction, or counting, the
router applies the role's explicit level to the shared runtime. Destination
counting uses the requested level; source conversion uses the captured pre-switch
role's level, validated against the source profile. If an opaque source cannot
support that level, select the source with a supported level before conversion.
No model-wide fallback is invented.

A compatible same-profile reasoning change follows the unified persistence
transaction, then updates the route in place. It does not reset/reconnect a socket,
rewrite instructions, tools, or input, or change the prompt-cache key. Reasoning
never belongs in instructions or cache identity. Request-property comparison
invalidates only continuation: one full-input resend keeps the identical prefix,
and normal incremental continuation resumes afterward. The status displays the
selected level beside the model. Genuine profile changes keep the existing reset
behavior, and root selections never retarget child routes.

## Context measurement and capacity

Context decisions use semantic token estimates rather than serialized
transcript byte size. The payload estimate walks typed messages and counts
unescaped visible text, reasoning summaries, tool names and arguments, tool
results, and user JSON. Encrypted or redacted reasoning contributes only a
constant marker; provider IDs, signatures, status fields, and JSON envelope
punctuation do not become semantic payload. A second conservative estimate
retains non-opaque envelope overhead after replacing opaque blobs with constant
markers.

Replay-backed history is estimated from its canonical message for both its
source profile and a portable foreign-profile projection. Replay-only
checkpoints decode known Responses output shapes and use an opaque-safe
conservative fallback for future shapes. Native replay remains lossless at
dispatch; this normalization applies only to capacity measurement.

One `ContextTokenSnapshot` is authoritative at every admission and dispatch
boundary. An exact provider count wins. Otherwise a valid same-profile
completed-response usage baseline plus locally appended semantic deltas wins;
without that baseline, the conservative opaque-safe estimate is used. The
payload estimate remains a diagnostic candidate. Zevria requests an exact
count when candidates disagree across the automatic trigger or hard ceiling,
or before an estimated compaction or rejection. Definitive unsupported
responses fall back locally. A transient failure suppresses further exact-count
attempts for that turn, while a later turn may retry.

The same snapshot drives prompt and edit admission, skill activation, ensemble
synthesis, automatic-compaction eligibility, and the final gate before every
provider call. A prepared request can trigger at most one automatic compaction:
Zevria installs the checkpoint, rebuilds the complete request, measures again,
and rejects only if the rebuilt request still exceeds `input_token_limit`. A
post-compaction request above the trigger but within the ceiling proceeds, so
the loop cannot repeatedly compact one dispatch. Diagnostics identify whether
the value was exact, usage-backed, or estimated and whether compaction was
attempted or unavailable.

Compaction anchors are independent of editable prompt rows. Ordinary and
direct-skill prompts, typed Plan `Handoff` records, and ensemble `ReportsReady`
evidence can anchor automatic or manual compaction. In particular, a fresh
`StartFromPlan` session needs no ordinary `Submit` to become compactable.
Handoffs do not consume frontend prompt ordinals, and the other Plan lifecycle
records are not compaction anchors.

The handoff's model-visible prompt joins ordinary/direct-skill instruction text
in bounded retained-user selection. Selection carries forward the latest
checkpoint's retained text, excludes generated summaries, and selects recent
instructions in chronological order under the active profile's
`retained_user_tokens` budget, using the existing middle-truncation policy.
This bounded metadata remains available to explicit `/model` or `/model-session` conversion, which
retains its existing retained-prompts-then-summary layout. Ordinary automatic/manual local compaction does **not** reinject
those copies. Ensemble evidence and user-role tool-result batches are not retained
user instructions. Truncating metadata never changes the complete approved
artifact's durable authority in its original typed handoff record.

### Adaptive local summaries and verbatim tails

The immutable source is the effective `CompactionSource.input` model projection,
not physical JSONL lifecycle records. It includes earlier checkpoint replacements
and their complete tails. Starting with all `n` source items, local summarization
considers exclusive prefix boundaries `n`, `n-1`, …, `1`, without halving, text
fragmentation, or a separate retry cap. A call/result exchange may not straddle a
cut: occurrence-based canonical/provider/native handles determine safe boundaries,
including batched calls/results and indivisible native replay envelopes.
Self-contained native outputs do not require external results merely because their
type ends in `_call`. Malformed original input is an error, not a reason to drop
newer items; provider preflight validates source and replacement projections.

Each candidate request carries the filtered conversation prefix and configured
summary prompt, with no ordered directives. Its top-level maintenance instruction
set contains captured application/file guidance and the no-tool summary policy. Typed live skill/workflow history is not summarized; ordinary
user/tool text is never removed based on marker-like strings. Catalog-backed adapters
use the same profile's exact-count/conservative admission; adapters without a
catalog retain provider-owned admission. Negative local admission or the typed
`ModelInputTooLarge` provider error advances one item backward. Other errors and
cancellation stop the operation. Attempts are bounded structurally by descending
nonempty replay-safe source prefixes, not by a call counter. Local rejects and
boundary checks make no model calls; a long descent can incur substantial counting
latency and paid attempts. Continuation state is reset before and after synthetic
completions, and synthetic streams and usage are not published as ordinary turn output.

On success at boundary `k`, the replacement is **summary first, then exact
`source[k..n]`**. Full-history success has an empty source tail. Tail items retain
value, variant, content, identifiers, replay envelopes, and order; they are not
selected, truncated, or bounded by `retained_user_tokens` (including zero). The
engine's task snapshot remains a separate suffix. Version-1 checkpoints contain
no instruction snapshot. Live and resumed projection borrow and fold preceding
directives with the reducer's skill keys/order: revocations remove effective bodies.
Projection places the effective boundary directives after replacement history
and before subsequent input; an uncommitted prospective activation cannot enter
it. Destination-fit checks include this live state, not just summary text.
Loaded histories retain ordered directives and their effective state; current
guidance is rebuilt before dispatch. Maintenance preparation uses captured current application/file guidance
for both preflight and actual requests, including immediate post-resume and
edited-prefix compaction; it never derives authority from source conversation.
A local-summary checkpoint with a replay-only tail remains profile-bound:
Both commands' preflight and explicit conversion still enforce opaque provenance.

Successful partial checkpoints are persisted append-first for live automatic
pre-turn, mid-turn, and manual compaction, even if the rebuilt normal request is
still too large. That request is blocked with a capacity diagnostic, not dispatched
or recursively compacted; a new prompt remains uncommitted. Manual `/compact`
saves and refreshes telemetry without normal dispatch. Edited-prefix preparation
is the exception: its checkpoint is committed only together with an accepted edit,
so a rejected/cancelled edit cannot hide the still-authoritative old tail. If no
nonempty replay-safe prefix fits, there is no checkpoint or empty-source summary.
Native remote compaction and explicit model conversion remain separate paths
with their existing contracts; neither migrates unsupported persisted formats.

The adapter always sends Responses request bodies and accepts only Responses
HTTP/SSE or WebSocket events. Direct Chat Completions endpoints are not a
second adapter and are not translated. HTTP-only providers set
`"supports_websockets": false`; optional Responses request properties can be
disabled independently under `providers.<key>.compatibility` in `models.jsonc`. See
[Responses-compatible gateways](responses-compatible.md).

Production allow-lists for ordinary turns are explicit. Build receives `command`, `task`, `edit`,
`write`, `delete`, `launch_subtasks`, and eligible `skill` and `skill_read`
tools. The two skill registrations are statically registered
in root runtimes even when not advertised. Plan receives `command`, optional
`launch_subtasks`, mode-enabled skill tools, `question`, and `submit_plan`. Explore children
receive only `command` plus an empty skill catalog. Build children receive exactly
`command`, `task`, `edit`, `write`, and `delete`, rooted at their reserved directory.
Both child registries omit interaction, skill, planning, reconciliation, and nested
launch tools physically, with skills disabled in both nominal mode policies.

Ensemble root synthesis applies a narrower policy override without changing
the underlying Plan and Review model roles: Ensemble Plan receives exactly
`command`, `reconcile_reports`, `question`, and `submit_plan`, while Ensemble
Review receives exactly `command`. Ordinary Plan does not allow
`reconcile_reports`, even though the production registry contains it. Explore
children use a physically command-only registry, and ACP projection classifies
reconciliation as a thinking tool.

The external ACP workers are separate processes and never use this root tool
policy. `SessionEngine` first collects every terminal worker outcome and
publishes each `AgentRunFinished`, then persists and publishes `ReportsReady`.
That record contains the exact bounded synthesis message plus typed worker
summaries carrying the decision-ID catalog and accepted-but-unavailable
markers. Only beyond that durable boundary does the root model/tool loop begin.

Ensemble Plan replaces the inert ordinary Plan candidate lock with a
`PlanSubmissionGate`. Its durable stages are: at least one terminal `command`
inspection attempt; the latest accepted `reconcile_reports` declaration; a
terminal standalone root question when that declaration requires one; and an
accepted Plan candidate. A later accepted reconciliation supersedes the prior
declaration and resets its question state. Repository evidence may resolve
facts, objective equivalence, infeasibility, and incorrectness, but cannot pick
a user preference from the status quo, tests, diff size, worker consensus, or
model preference. Captured worker decisions are applied without duplicate
questions; unresolved viable preferences, conflicting captured decisions, and
accepted-but-unavailable choices require one root question.

Root calls and results use the normal durable transcript. A post-`ReportsReady`
tail reducer correlates assistant calls with tool-result metadata and
reconstructs all four gate stages after restart without rereading child logs.
This preserves the exact durable synthesis input and retains completed
inspection, reconciliation, question, and submission work. Existing historical
`ReportsReady` records are not rewritten.

Ordinary Plan (including ACP), Explore, native worker Plan/Review, and both
ensemble synthesis workflows use the static [inspection policy](instructions/inspection-policy.md):
**source-read-only with temporary investigative execution**. Canonical Plan,
Explore and worker constants compose it at compile time. Plan synthesis inherits
it from ordinary Plan; Review synthesis explicitly includes it once. The
startup workspace remains the command's default cwd on every invocation, not a
read-access boundary. Task-relevant absolute/home paths (including
`~/.zevria/logs/zevria.log`), parent-relative paths and external symlinks are
readable under existing OS permissions. Such evidence may enter model requests
and persisted logs; keep reads task-scoped.

Analysis agents may create unique, private, agent-owned directories in the actual
OS temporary location through `command`, recording the absolute path in ordinary
command/result history. Shared temp roots, other workers' scratch, project `tmp`
directories and source worktrees beneath OS temp are not authorized write targets.
Task-related downloads, source copies, transformations and archive extraction
need explicit destinations and protection against traversal and symlink/hard-link
escapes. Scripts, builds, tests and package installation require prior inspection
of scripts/configuration and scratch-local outputs, dependencies, caches, logs
and subprocess temporary files. Writable source trees must be independent copies,
not the original repository or hard-linked copies. Global state, ordinary home
configuration, non-scratch files, remote state and the original project remain
protected. Skip execution when relevant write locations or side effects cannot
be established sufficiently. Required repository inspection before synthesis
remains independent of scratch creation or validation work.

This is behavioral containment, **not an OS sandbox**: the shell has ambient
application permissions, and neither cwd nor environment variables technically
confine arbitrary programs. The analysis allow-lists still remove structured
mutation tools; scratch uses the existing command tool. Builder workspace rules,
skill resource containment, ACP permissions and native handoff exceptions remain
separate. Scratch prototypes are not implementation or Plan publication/approval.
Clean up only owned scratch before completion gates when practical, with no
hard-termination cleanup guarantee or assumption of survival across resume.

No scratch registry, permission migration or dynamic path-bearing instruction is
introduced. The provider's immutable bootstrap, instruction version/identity and
cache-key derivation are unchanged. Workflow changes replace the rendered
instruction set; any resulting skill revocations or restorations append durable
ordered directive records. The workflow instruction set itself is not persisted.
Resume reconstructs native policies from the running binary, and compaction keeps
instruction authority separate from summaries and historical refusal prose.
Restart/reopen workers to pick up compiled policy changes. Native current policy
clarifies that generic older no-mutation boilerplate does not negate its scratch
exception; specific user restrictions still apply. A fresh ensemble receives the
full revised launch envelope. External ACP agents retain their own policies and
sandboxes, so support for these operations is not guaranteed.

The provider filters advertised definitions to the request allow-list. The
engine independently checks the same snapshot before Rig dispatch; a forbidden
call is not invoked and instead becomes a correlated, model-visible tool result
with `ToolCallOutcome::Denied` and no file changes. Within one assistant batch,
every `launch_subtasks` call is started first and awaited concurrently, so
children run while the remaining calls execute; ordinary calls remain
sequential. Result slots keep the original assistant-call order, and all
completed work is recorded before continuing.

Ordinary Plan still serializes a batch containing `submit_plan` in assistant
order. The first valid candidate is retained by the inert submission gate; all
later calls in that turn are denied, and the model receives one continuation
for a short confirmation. The candidate is committed only after the entire
provider turn succeeds.

For Ensemble Plan, stage-specific denial slots keep the same model turn alive.
They deny reconciliation before inspection, a question before reconciliation,
a duplicate or unnecessary question, submission before reconciliation, and
submission before a required question settles. A `question` must be the only
call in its assistant response, and a batch containing both `question` and
`submit_plan` is rejected so the Plan cannot claim to incorporate an answer it
had not received. Reconciliation and submission likewise require separate
rounds. Denials are correlated ordinary tool results and therefore remain
model-visible and durable. If synthesis still ends without a required stage,
the engine records a hard workflow failure with a stage-specific diagnostic;
missing `submit_plan` remains the final hard failure.

The session-local question broker is shared by the Plan-only native `question`
tool—including post-`ReportsReady` Ensemble Plan root synthesis—and ACP form
elicitations from ensemble Plan or Review workers. It registers one pending
typed request, publishes `QuestionAsked`, and waits on a correlated oneshot.
ACP workers serialize access through a fair async gate, so only one global
modal is visible while unrelated worker traffic remains parallel. Root
synthesis starts only after all workers are terminal, so its unresolved
disagreement question cannot contend with a live worker form. Each terminal
path publishes an ID-matched `QuestionClosed`; the TUI ignores stale closes
that target an older modal.

The question popup measures prompts, answer labels/descriptions, editors, and
validation with the exact width and wrapping configuration used to render.
When space permits, the wrapped prompt and validation remain pinned around an
independently scrollable answer viewport; terminals too small to leave one
answer row use one whole-body viewport inside the same border. Selection is
bounded. Text and Other share editor-first key routing: Left/Right move by
extended grapheme, Ctrl+Left/Right use the composer's alphanumeric/underscore
word boundaries (punctuation and whitespace separate words), Backspace/Delete
remove the preceding/following grapheme, and typing inserts at the caret.
Home/End move to the start/end of a focused draft; outside an editor, including
Text's optional Skip row, they choose the first/last selectable row. Other's
Esc returns to choices without losing its draft or caret. Unmodified Left in
Text returns to the previous question only when already at text start;
Ctrl+Left never leaves a focused editor. Text's Up/Down still navigate the
editor/Skip rows, and typing or Backspace from Skip refocuses the editor.
PageUp/PageDown pan one visible page minus a context row without changing the
selection, draft, caret, or answer. The next selection change, caret movement,
or text mutation reveals the active item or inline `▌` again; unchanged editor
boundary keys preserve manual panning. Renderer-backed caret measurements use
the complete wrapped editor, bounded scratch buffers, and a revision/caret/width
cache. The caret's one-row reveal anchor is independent of the full selected
segment, so selection painting also covers rows after the caret. Draft carets
and measurements remain local UI state, not typed or persisted answers.

The frontend answers or explicitly dismisses through
`SessionCommand::Control(ControlCommand::AnswerQuestion { .. })`; the engine resolves that command immediately
inside the active-turn select loop, so ordinary and Ensemble Plan root answers
become the existing tool call's result, while ACP answers resume the suspended
worker request rather than creating a root user prompt. A drop-safe
registration guard releases the single-pending slot on cancellation, frontend
loss, or waiter teardown. Native question JSON retains its historical
string-answer shape. Tool-result metadata adds a typed terminal disposition:
`answered`, `dismissed`, `unavailable`, or `invalid_frontend_response`. All four
settle an Ensemble Plan question attempt according to conservative disclosure
policy. Invalid model arguments carry no disposition and do not release the
gate; parent-turn cancellation still terminates the turn.

ACP prompts add text/single/multi kinds, bounds, defaults, optional Skip, and
private display-to-wire mappings for enum constants and booleans. Before an ACP
`accept` response is queued, the exact non-secret display values are normalized
and synchronously appended to the worker log. These decisions flow through the
worker projection and outcome into bounded synthesis; raw provider constants,
unselected options, descriptions, and metadata are excluded. Oversized exact
batches become accepted-but-unavailable markers rather than truncated values.
The pane renders only the compact field-count/outcome diagnostic, while the
normalized worker JSONL and root synthesis expose accepted non-secret values.
Secret-marked forms remain declined.

`task` is a Build-only progress tool. Every call supplies a complete 1–20 item
replacement snapshot with concise `pending`, `in_progress`, or `completed`
steps and at most one active item. Validation normalizes Unicode and visible
whitespace, rejects duplicate or multiline steps plus unsafe control/invisible
formatting characters, and returns model-fixable argument errors. The
assistant call and correlated acknowledgement use the ordinary model/tool
loop and transcript format. Before lossy context compaction, the engine
replays those records to find the latest call with a correlated successful
result and appends its exact compact JSON snapshot to the checkpoint's model
history. Failed, denied, or interrupted calls cannot replace that state. The
latest successful call is the current checklist, while earlier snapshots
remain chronological history. The TUI renders both live and restored calls as
compact checklists instead of raw arguments, hides successful acknowledgement
text, and preserves explicit interrupted (`◼`) or unknown (`?`) state rather than
inferring success from a terminal turn event. Confirmed updates use `•`; item rows use
`○`/`◐`/`✓`. Raw `y`/`yy` payloads remain distinct from Block-scope `Y` readable lists.

Structured file-tool metadata remains separate from the ordinary model-facing
result. `edit` keeps default-context unified patches, a 512 KiB metadata bound,
and a 1,000 wrapped-row TUI bound with one truncation notice. Readable UTF-8
`write` and `delete` changes instead preserve complete content: additions and
deletions store the exact file text, while an overwrite stores one full-context
unified patch. A byte-identical overwrite stores an empty patch. Successful
`write`/`delete` calls with one correlated change render only one compact
`◆ <tool> <argument path> ✓ (+A -R)` summary row, without a readable diff
body or truncation notice. Unavailable-content diagnostics retain their reason
and byte count. The ordinary result remains available to secondary copy but is
not rendered redundantly. Defensive multi-change records render every file's
summary and any omission notices, never readable diff bodies. Live and restored
calls use the same presentation. Partial outcomes retain surviving file changes;
denial does not fabricate mutation details. Typed outcome/diagnostic sidecars decide
status and visible failure detail, not result-text prefixes. Legacy/missing outcome
evidence stays unknown, with raw output still copyable. Model-facing result bytes
and ACP diff content are unchanged; optional display metadata is persisted and
forwarded separately.

`reconcile_reports` uses the same transcript-backed native-tool lifecycle but
renders a compact header with disagreement and accounted-decision counts,
followed by one metadata-only row per disagreement, recorded decision, and
unavailable decision. Large summaries, positions, evidence, requirements,
explanations, and reasons stay out of expanded transcript rows. The successful
acknowledgement is hidden; `y` copies the complete original declaration, `yy`
copies the result, and `Y` copies a readable header/list with full identifiers and
supporting text. Typed unsuccessful outcomes and display diagnostics remain visible
without deriving status from prose. Live and restored calls use the same renderer,
with `✓` reserved for correlated successful completion.

## Cancellation, bounds, and durability

One `TurnContext` carries the turn ID, immutable mode, and a cancellation token
through the provider, every tool call, and every child launch. When the focused
root surface permits operation cancellation, `Ctrl+C` sends a turn-ID-aware
request; only an idle root may quit. A focused composer clears its nonempty draft
first, workers cancel only their bound input, and global dialogs act locally as
described in [TUI interaction](tui-interaction.md). Root operation cancellation
drops the active provider future and therefore its HTTP stream, terminates an
active WebSocket generation, stops command process groups, cancels
queued/running children, records completed work already observed locally, and
emits `TurnCancelled`. No mid-stream model
idle timeout is configured: after the 30-second first-event deadline, a
healthy in-progress stream may remain quiet indefinitely.

Shell commands default to a five-minute wall-clock timeout and 1 MiB combined
stdout/stderr capture. The configured limits apply identically to root
sessions and both child kinds. Both streams are drained concurrently into
bounded head/tail buffers, preventing pipe deadlocks without retaining
unbounded output. On Unix each command owns a process group; timeout,
cancellation, or worker abort terminates descendants as well as the shell.
Native Windows freezes a Git Bash shell/child environment for root and both child
kinds and checks RTK before model-driven tools. It starts workloads suspended,
assigns an owned kill-on-close Job Object, then resumes. Leader exit status is
waited separately from job lifetime; remaining descendants are killed before
capture drains, including on ordinary shell exit. Configured ACP agents use a
private native job helper around the pinned SDK's direct-child cleanup. WSL
handoff is whole-application and never treated as Windows-owned Linux descendants;
no cleanup terminates a distribution. See [Windows runtime setup](windows.md).

Transcript appends use complete writes, flush, and `sync_data`. Rewrites and
edited-turn replacement are staged, synced, and atomically renamed. If a
completed external action cannot be appended, it remains in the authoritative
in-memory conversation and the session becomes persistence-degraded; new work
first attempts a full atomic repair and is rejected if repair still fails.
Resuming a transcript removes malformed records through the same atomic
rewrite and displays a recovery notice. Content-only `write`/`edit` operations
also use atomic sibling replacement. A combined edit-and-move attempts rollback
if final installation fails and reports any surviving mutation as a typed
`Partial` tool outcome.

## Subtasks: independent Explore and Build subsessions

`launch_subtasks { tasks: [{ title, prompt, type, workspace? }, ...] }` is one blocking
call launching a nonempty batch of independent inspection-intent `explore` or mutating
`build` child agents. A single entry delegates one child. Concurrency does not depend
on the provider emitting multiple native calls or enabling `parallel_tool_calls`.
Foundation owns the provider-neutral identity/descriptor/result values;
`zevria-session-api` owns reservations, launch channels and lifecycle machinery.
The tool lives in `zevria-tools`; the Responses-backed runner lives in app:

```text
launch_subtasks tool (zevria-tools)
│ whole-batch preflight, then concurrent bounded sends over a SubtaskLauncher channel
▼
session-api registry + channels (SubtaskLauncher / SubtaskLaunchRequest)
│
▼
supervisor (zevria-app, subtasks.rs)
├── fresh single-profile Explore or Builder router/transport per child (ResponsesRouterFactory)
├── independent SessionEngine/history and kind-specific instruction set
├── Explore: shared command-only registry
├── Build: private command/task/edit/write/delete registry rooted at its reserved directory
├── child transcript under .zevria/subsessions/<root-id>/<child-id>.jsonl
├── child SessionUpdates consumed through the ordered receiver
│   ├── lifecycle forwarded to the frontend tagged as SubtaskSession
│   └── stream batches republished under the child id on the parent channel
└── exactly one SubtaskOutcome per launch, even on cancellation/connect failure
```

The tool requires a nonblank title and prompt; violations return model-fixable
invalid-argument failures before any request is queued or child metadata is
attached. A concise 3–5 word title is advisory, not a launch requirement: the
title's wording is preserved after outer-whitespace trimming, without automatic
shortening. Outer and entry arguments reject unknown fields; `tasks` is required
and nonempty, and each entry requires title/prompt/type.
Explore accepts omitted/null workspace, but rejects a supplied path. Native Build
children require the effective turn's `build_subtasks` capability and a non-null
workspace. Root Build's immutable policy declares conditional `orchestration`
eligibility, but ordinary `subtask_kinds()` advertises only Explore. The model
loop derives the effective copied turn capability solely from the accepted typed
request; both launch gates read it, not the `SessionMode` enum or tool arguments.
Standard Build and Plan reject `type: "build"` before filesystem preparation.
Build still permits Explore, and Plan permits Explore when configured. An explicit
`/orchestrate <prompt>` retains all ordinary Build tools and authorizes Builders
for that request only. Root prompts are embedded from
`docs/instructions/build-mode.md` and `docs/instructions/plan-mode.md`; activation,
Standard reset, and correction are separate ordered request directives.
Native ensemble workers retain their separate Plan/Review restrictions and never
gain builder access merely because a turn is nominally Build.
Every entry is authorized and every Build path resolved before any mutation or
launch. All workspace keys are reserved atomically, rejecting pairwise overlaps,
symlink aliases, and conflicts with other active batches before directory creation.
Keys are resolved before taking the reservation mutex; no I/O or await holds it.
Preparation failure drops all untransferred guards. Filesystem creation is not
transactional: newly created empty directories may remain after later failure.

Enqueue futures are polled concurrently before awaiting reports, reusing the shared
supervisor rather than creating a second scheduler. Each accepted child announces
`SubtaskLaunched` with the real outer call ID, input entry index, and distinct child
UUID. Reports are collected without failing fast and returned in deterministic input
order with requested/launched counts and per-entry status, report or error. Success
requires every entry to complete; parent-turn cancellation yields Cancelled, and
other unsuccessful batches yield Error. Completed sibling reports survive both.
`ToolResultDetail::Subtasks(Vec<SubtaskEntryMetadata>)` records each input index and
terminal child status, plus optional accepted launch identity. Unaccepted slots never
invent IDs. Reports remain only in the model-visible result, not sidecar metadata.

The model-facing contract says to include **all ready independent tasks in one array**.
Separate blocking calls cannot overlap across assistant responses; later calls are
appropriate for dependencies or explicitly serial work. Both single-call internal
fan-out and multiple native batch calls obey the same supervisor limit, including a
limit of one. The internal fan-out also works when submission gates serialize the
outer calls. Keep launch-only assistant responses as behavioral guidance; the
existing mixed-call, permission, submission, and skill boundaries remain enforced.
The parent resumes only after every call resolves, with exactly one provider-correlated
result per outer call. Task lists, counts, queue state and IDs live exclusively in
arguments/results/events, never in instructions or generated tool definitions.
The static schema revision does not cause per-batch cache-prefix churn.

### Request-local orchestration contract

Admission binds `RequestBehavior::{Standard, Orchestrate}` to a fresh versioned
`RequestMetadata` identity on the accepted turn. Root Build must explicitly be
eligible, `launch_subtasks` must be registered/permitted, no Planning/Ready Plan
may be pending, and the captured supervisor capacity must be at least two. These
checks precede persistence repair or history mutation. The app supplies the same
`session.max_concurrent_subtasks` value to admission and the existing scheduler.
Standard Build, Plan, workers, children, direct skills, Plan handoffs, and ensemble
synthesis do not acquire Builder permission through prose or nominal Build mode.

An explicit prompt persists as the ordinary user message plus a `zevria_request`
sidecar. Its matching `zevria_request_directive` boundary immediately follows it,
without adding another editable prompt ordinal or prefixing model-visible user
content. Boundaries after skills, handoffs, and ensemble ReportsReady are Standard.
Request records validate versions, ownership, unique identity, and placement
independently of the skill ledger; they cannot split a tool call/result batch.
A correction must match the current accepted request and follow a no-tool assistant
response, at most once. Text lookalikes and tool arguments do not supply authority.

The in-flight gate starts empty for every accepted request. Only committed,
replay-valid `ToolResultDetail::Subtasks` metadata can qualify: one result must
match the real `launch_subtasks` call by logical ID, provider call ID, and tool
name, and contain at least two distinct accepted child IDs. Two single-child
calls, rejected entries, duplicate IDs, old history, counters, summaries, or prose
do not qualify. Child failure/cancellation does not erase its accepted identity;
parent cancellation, persistence failure, or replay failure still prevents success.
Qualifying submission is not a guarantee of simultaneous wall-clock execution.

A premature completed assistant response is preserved natively with usage and
its display-attempt identity, but emitted as Intermediate rather than successful
completion. The engine may durably append one corrective directive. A second
premature final fails the request; the one-correction rule is independent of call
count. The parent must perform meaningful independent work, integrate
reports/artifacts, handle failures, and
validate, not invent padding tasks to satisfy the gate.

Compaction separately reprojects the active typed boundary and correction, with
normal capacity accounting; maintenance summaries cannot invent or retire them.
The live gate retains committed evidence across that request's compaction, while
resume/new prompts and transcript replacements start fresh authorization and
obligations. Fixed Build instructions, eligible catalog, tool schemas/order, model
role, reasoning, and provider cache identity are unchanged across Standard →
orchestrated → Standard requests. Ordered request directives extend the input
suffix without rewriting earlier native output or skill directives.

Both child factories capture their configured Explore/Builder assignment at session
opening, independent of root Build/Plan switching and durable selections. They use
`ResponsesRouterFactory::new(role, profile, preamble)` and `create(session_id, tools)`;
provider and child engine receive the same tool handle. Children inherit the root's
opening/resume guidance, not active skills or child-local guidance. Like root
sessions, children have no model-call count ceiling. Explore retains its
inspection-only directive and shared command tool.
Build uses `docs/instructions/build-subtask.md`, a JSON `workspace` object containing both absolute roots, and a private
five-tool registry. Both kinds run nominal Build turns but use their dedicated
Explore or Builder role and immutable child policy, never root orchestration
eligibility or an inherited request obligation. A parent's mode or model selection does not expand them.

Build workspace syntax must be a nonblank relative strict subdirectory, with no
explicit dot/parent segments, root/prefix, or first `.zevria` component. Read-only
resolution canonicalizes existing ancestors (including symlinks), rejects dangling
links, non-directories, escaping aliases and aliases to startup/protected storage,
then appends any missing normal suffix. Reservations are per parent: launcher clones
share a short mutex-protected component-aware table, but different root sessions do
not. Equal, ancestor and descendant targets conflict; siblings and `book` versus
`book-extra` do not. Canonical safe aliases contend for the same target.

Reservation precedes `create_dir_all`; existing content is preserved. Final canonical
identity must match the reserved key and pass containment/protected-storage checks
again, or preparation fails with a retry diagnostic. Ownership lasts through
preparation, queue backpressure, semaphore waits and execution. The non-cloneable
RAII guard releases only ownership, never directories or artifacts. Failed preparation,
failure and cancellation may leave partial content: no rollback, scratch `.zevria`
artifact tree, Git worktree, automatic cleanup, or cross-process lock is provided.

Both kinds share `session.max_concurrent_subtasks`, default **10**, with positive
configured lower limits preserved. Cancellation-aware bounded sends wait for queue
capacity instead of rejecting bursts. Already-cancelled/pre-enqueue rejected calls
announce no child and retain unlaunched terminal slots (validation rejection has no
sidecar). Accepted children can wait as Starting:
the execution semaphore is not a cap on all pending JoinSet workers/reservations.
The executor keeps original result-slot order and homogeneous batches remain guidance.

Parent cancellation reaches queued permit waits, provider handshakes, model turns
and commands. Cleanup owns execution, its permit and workspace even after supervisor
abort; abort drops the unresolved outcome sender but cancels rather than aborts the
cleanup task, which awaits command termination before releasing ownership. Normal
child event forwarding retains its existing backpressure; abandoned workers bypass
UI delivery to finish cleanup. Ownership is explicitly dropped before terminal
oneshot/status publication, never retained just because final status delivery blocks.

Task-relative input paths default to startup; child tool-relative paths and shell cwd
refer to its directory. Build may read startup inputs, but writes (including shell
redirection, caches, temporary and generated files) must stay inside the child root.
Git, installation, parent-project builds, and network use require explicit task
authorization, never permission to write elsewhere. Structured mutations reuse
`resolve_path_for_write` unchanged. The shell is unsandboxed, so confinement and
parent non-interference are behavioral: parent tools, other sessions, external
processes or disobedient commands can violate ownership. Filesystem races and
unsupported case/filesystem equivalences are not solved. Keep directory identities
stable; this is not a security sandbox or filesystem transaction.

Restore uses full launch metadata (kind, title, optional workspace) from root tool
results and is presentation only: no directories are created, no reservations are
acquired, and historical children never restart. Every accepted child is restored,
including failures and cancellations, using its own status rather than the aggregate
tool outcome. The plural sidecar is an intentional breaking change; uncorrelated
transcripts remain placeholders.

The Ratatui frontend has no batch summary row: an ordered collection of
separately selectable `◆ <kind> · <title>[ · <workspace>] <icon>` child rows
alone represents the batch (no prompt, raw arguments/tool name, result text, or
child output), correlated through the immediate `SubtaskLaunched` event or
restored result metadata. Nothing renders while executing until children are
accepted, and the batch block stays hidden while executing and whenever all
requested children have launched. Terminal child lifecycle states remain
authoritative over generic tool outcomes. After execution stops, the batch block
shows only what child rows cannot convey: `◆ N of M subtasks not launched`
above accepted children for a partial launch, with an error-colored count label
but deliberately no aggregate failure icon. Descriptor-less failure/denial rows
are `◆ subtask launch ✗ · <reason>` / `◆ subtask launch ⊘ · <reason>`, with
error-colored icons and reasons. Descriptor-less interruption/cancellation uses
`◆ subtask launch ◼` (warning icon); completion uses `◆ subtask launch ✓`
(success icon). Prefixes and ordinary labels use `roles.tools`. Child Starting
uses muted `○`, Running info `◐`, Completed success `✓`, Failed error `✗`, and
Cancelled warning `◼`; selected-row foregrounds override these semantic colors.
Hidden batch blocks take no
space or role header and cannot be selected; visible ones still copy their raw
arguments. A descriptor-less row cannot open a child pane.

The narrow exception to result hiding is the failed or denied launch reason,
extracted only from the correlated result's error/reason envelope, never from
raw arguments. Reasons are single-line, stripped of unsafe terminal/bidi
controls, and capped at 240 graphemes plus an ellipsis, with the bare label for
missing or unusable reasons. This works live and on restore, including
metadata-free historical `status: error` results, without inventing a child
identity or rerunning a rejected call. Successful reports and launched-child
result bodies stay hidden.
Each tagged `SubtaskStatus` transition updates its matching root row and child-pane footer
identity/lifecycle independently and immediately; model-visible tool results
still arrive as one correlated batch only after every call in the launch batch
resolves. Child pane labels are `<kind> · <title>[ · <workspace>][ · <status>][ · historical]`.
Panes select Explore or Builder before replay. Late authoritative descriptors correct
placeholder activity/compaction roles without merging role-keyed usage or regressing
Running/terminal status. Duplicate titles are harmless: rows use stable presentation
identities and children use UUIDs. Result metadata creates missing rows and panes
when launch events were missed. Builder uses the Build accent where applicable;
Inspect panes retain the Inspect accent. ACP projects distinct display IDs per child,
associated with the outer call ID and input index, both live and on replay. Child
completion never completes the outer call or overwrites a sibling; only the outer
batch result completes the actual provider-correlated tool lifecycle. A session-view manager owns the root pane plus
one inspect-only pane per child:
`Enter` on a selected launch row opens the live child, `Ctrl-O` returns to the
parent without cancelling anything, and `Ctrl-I` (or `Tab`, its legacy
encoding) reopens the most recently entered child — intercepted ahead of
approval/select/insert handling so navigation works while busy. Tagged
`SubtaskSession` events reduce into hidden child panes continuously. A current
child's final report returns within the single correlated `launch_subtasks` batch
result; restoration never relaunches children.

## Skills: definitions, catalog, historical pins, and context

Skill lifecycle state is explicit and engine-owned (see [skills.md](skills.md)).
Four concepts share one five-stage workflow: **discover → resolve a name →
prepare an application → commit it → project instructions**.

- `SkillSnapshot` contains a validated name, normalized complete body, metadata
  with one authoritative description, optional provenance and a full `SkillDigest`.
  Its deterministic domain-separated digest covers every snapshot field except
  itself and is verified during deserialization. Unknown fields are rejected.
  `SkillDefinition` adds optional runtime filesystem locations, not persisted
  arbitrary-path authority.
- `Arc<SkillCatalog>` owns selected definitions, all candidates, invalid candidates,
  bounded diagnostics, fixed roots and validated configuration. Fallible
  constructors reject duplicate programmatic names and invalid configuration.
  Its management revision includes candidates, configuration, fixed paths,
  captured canonical roots and diagnostics.
- `ActiveSkills` is a name-sorted ledger of historical per-session snapshots.
  `SkillApplication::Activate(snapshot)` and `Reapply(name)` use one fallible
  reducer for preparation and replay. Duplicate activation and unknown reapply
  are errors. Disablement changes projection, not this ledger.
- `SkillContext` captures catalog, pins and effective permission. One resolver
  checks workflow/name enablement, prefers a pin, otherwise resolves an installed
  definition, then enforces fresh explicit-only policy for model-origin calls.
  Model disclosure and explicit completions share this resolver.

Discovery uses only `~/.zevria/skills` and startup-workspace `.zevria/skills`.
The bounded parser, discovery and handle-relative resource reader remain separate
modules. Project overrides global; malformed, ambiguous or incomplete higher
scopes may fall back only to a fully resolved lower scope. Canonical aliases are
deduplicated and outside-root candidates rejected. Actual workflow and registered
activation capability gate bodies, disclosure and admission. Skill-capable empty
catalogs explicitly report no eligible entries; disablement clears prior visibility.
Static root tool registration remains catalog-free.

App's `LocalSkillService` owns the captured fixed roots and configuration
path, not provider credentials. Core accepts correlated data-only management
commands. During a turn it serves installed-snapshot queries but immediately
rejects reload/config mutations instead of queuing them. Idle mutations require
the expected revision, prepare an independent replacement, and commit any
configuration write before infallible engine installation. Unchanged revisions
are no-ops. Bounded `SkillsChanged` events invalidate client projections without
publishing catalog bodies. Mode permission is captured independently of startup
availability, so empty-to-nonempty reload works without enabling forbidden Plan
skills. Request-local orchestration does not alter Build's skill capability or
pinned-body lifecycle; Plan still obeys its captured `allow_skills` setting.
Skills cannot activate orchestration, and direct skill invocations are Standard. Neither skills nor
mode selection grant child skills or expand native worker Plan/Review policies.
Historical activation snapshots remain separate from the new catalog.

The same service backs `/skills`, offline `zevria skills` commands, and the
versioned `_zevria/skills/*` ACP extensions. TOML edits preserve comments and
unrelated settings through an adjacent cross-process lock, source-content
check, and `atomic_file::replace`. Live sessions adopt external skill edits only
on their own explicit reload. Management returns complete filtered views, with
per-field metadata shortening and bounded discovery/diagnostics but no pagination
or public source selectors. Inspect returns all candidates for a validated name.
Nameless malformed candidates remain searchable diagnostics. Responses carry
unique authoritative explicit-invocation completions, including eligible explicit-only
names and historical pins rather than frontend-filtered `enabled` rows.

TUI drafts, CLI inspection and ACP invocation are name-only; the engine resolves
at admission. ACP skill capability, requests, responses and notifications are v1;
all other extension versions are rejected. Invocation retains ordered text/image
arguments, `PendingPrompt` cancellation and Plan readiness tracking. A Ready Plan
uses `RevisePlanWithSkill { expected, name, args }`: version, permission, skill
and capacity checks precede the single commit of revision transition, invocation
and directives. Rejection or pre-commit cancellation leaves Ready unchanged with
no pin or invocation. This cannot approve/implement a Plan or bypass ensemble
restrictions. Generic ACP prompt text remains literal with respect to skills.

Reload replaces the installed catalog, never the historical ledger. Pins are
replayed from records rather than copied into installed definitions; changed/missing
source diagnostics are projections of the same context. Resources remain live.

- A direct invocation is a dedicated `zevria_skill_invocation` record containing
  name, ordered arguments and application. It has no UUID, version or message
  mirror. Model/display messages are derived from name and arguments only.
- `ToolResults` carries `zevria_skill_applications`, with local `call_id` and
  application. Ordinary provider correlation stays in messages/metadata, not a
  duplicate skill field. Snapshots never enter public result metadata or events.
- `skill` is a schema registration whose raw dispatch fails explicitly. The
  engine parses arguments, prepares applications, checks full prospective capacity
  and constructs bodyless acknowledgements without a tool-server round trip.
  `skill_read` remains an ordinary captured-context resource tool.

Direct admission groups body/revocation reconciliation, invocation and its new
body directive. `TranscriptItem::Directive` directly contains
`DirectiveContent`, without ownership wrappers. Edit anchors are actual prompt
positions: preceding skill bodies and revocations survive both edits and resume.
A same-name first-use edit preserves its full recorded snapshot—including metadata
after source change/removal. A different name resolves normally against the retained
prefix and current catalog. Rejected replacements keep the old tail unchanged.

Model responses containing `skill` must be skill-only; mixed batches execute
nothing, including concurrent subtasks. Skill calls stage a local prospective ledger
in assistant order: repeated names activate once then reapply, and capacity
accumulates. A later failed call does not erase earlier accepted applications.
Only accepted owning records install pins.

Replay correlates actual tool-result blocks, metadata and applications within each
batch, validating unique relevant IDs and matching tool names. Each successful
skill result requires exactly one application; failed/denied/cancelled results
require none. Applications reduce in actual result order, not sidecar order.
Acknowledgement prose supplies no authority. Results and resulting directives
persist together at one atomic commit boundary, including exact directive
positions. Prospective capacity shares `InstructionPreparation`
with direct admission, post-compaction checks and dispatch, including the complete
catalog as fixed overhead. Synthesis preflight likewise includes its rendered
instruction set and any body revocations.

Bodies are rendered deterministically once while effective as typed developer
input. Their semantic payloads persist in ordered directive records, while full
historical bodies also remain in authoritative activation pins. They never become
retained user candidates, tool-result prose, or prompt previews. The complete instruction set defines authority,
through its protocol module and skill eligibility/lifecycle through its selection
module; conversation cannot supersede it. Runtime permissions
remain independently enforced. Name disables precede duplicate shortcuts;
explicit-only policy restricts fresh model activation, not enabled bodyless
reapplication after typed user selection. Skill-disabled scopes append
revocations; returning to an eligible scope restores pinned bodies as needed.

The Eligible skills section of `InstructionSet` contains the complete sorted
installed eligible set of `{ name, description }`, independent of historical pins.
Configuration disablement renders a selection-unavailable sentence; an empty
array means no eligible skills. Skill-free roles render the unavailable sentence. Descriptions are safely shortened to
1,024 UTF-8 bytes and JSON-escaped; there is no entry-count/rendered-byte cap,
lookup fallback, revision or digest in this prompt projection. The model must
apply clearly matching skills before task execution, as specified once in
`docs/instructions/skill-selection.md`. Tool descriptions separately own invocation mechanics. Descriptions are file-controlled
matching metadata, never instruction authority. Full disclosure counts toward
capacity: an irreducible catalog that does not fit is rejected before commitment,
while management stays usable to disable names or reload. No mandatory selector call or frontend
intent rewriting is involved. Catalog changes replace the rendered instructions
and invalidate cached measurements; activation alone never changes them.
Summaries, remote compaction and model conversion have no catalog or ordered
directives. Resume rebuilds the catalog from current installed/configured metadata,
not saved pins.

Instruction directive format v1 uses `SkillBody { name, digest, body }` and
`SkillRevocation { name, reason }`, rendered with compact `Skill directive:` markers
and the body/reason only. Unsupported skill directive versions require a fresh session
through the existing unsupported-version error path.
The dedicated `zevria_skill_directive` envelope stores only `{version, payload}`;
`text` is derived on load, and extra fields or unsupported versions are rejected.
Standalone validation checks canonical structure/text, while replay checks all
three body fields against the full pin; a body-only hash cannot validate a snapshot
digest. Only v1
checkpoints are accepted, with nonempty replacement history and no instruction
snapshot, developer instructions, raw system messages or redundant pin identities.
Full retained-transcript replay reconstructs every pin, including disabled ones.
Live and persisted replay retain header checks, body/revocation-to-pin linkage and the ban
on directives/checkpoints splitting unresolved tool batches. Retired
`zevria_instruction_prefix`/`zevria_directive` envelopes, including mixed or
interrupted tails, and old checkpoints fail before writable restoration. There
is no compatibility reader, migration, automatic rewrite or upgrade backup.
Incomplete trailing skill directive records are recoverable because they carry no
pin authority; the next prompt derives missing directives from pins and policy.
Completed invalid inner envelopes and pin-bearing lifecycle tails are never
removed as crash debris. Literal marker strings inside ordinary messages, tools, pins or provider data
are not engine envelopes.

The protocol/application/file/workflow/capability/catalog instruction set is sent to providers but
not saved as engine transcript records. Ordered skill directives are the exception:
their payloads, including bodies and revocation reasons, persist in JSONL. Full
skill pins also deliberately persist with bodies, digests, metadata and provenance. This is not blanket redaction: old files stay
unchanged, ordinary conversation or generated summaries can quote instructions,
and provider-side retention is outside this boundary. Removing an activation removes its pin from the new branch; same-name
first-use replacement explicitly preserves that invocation's snapshot. Directive input is counted exactly
once, including superseded records until compaction. Prepared exact counts bind
the complete logical request, not just turn/role. Directive-only growth invalidates
stale counts but neither resets continuation nor re-arms automatic compaction.

Resume keeps discovery distinct from the authoritative active ledger. The
frontend's authoritative completion metadata carries validated names and pinned
descriptions, while the catalog retains source and disable state. Resource reads
verify private provenance bindings against the current canonical fixed root,
recorded scope and relative manifest; a retargeted root cannot redirect an old
pin. The current normalized main name and body are compared directly, allowing
metadata-only edits despite the changed full digest. There is no same-name or
cross-scope fallback. Main bodies are pinned; auxiliary text is live, bounded and
compactable, with cursors bound to snapshot/resource/live content identity. Unix
opening uses handle-relative no-follow checks; Windows uses owned handle-relative
NtCreateFile opens and rejects reparse traversal. Other platforms fail closed.

Transcript loading validates reserved record boundaries and the complete
lifecycle before returning any projection. Unsupported versions, removed skill
or subtask sidecars, body-bearing skill acknowledgements, and invalid identities
reject the entire open, including read-only opens, with a path and original line
when available. Ordinary marker-like strings and tool arguments remain data.
There is no migration, body-search normalization, format-upgrade write, or new
migration backup; original bytes and existing backups remain untouched.

Incomplete final appends without a newline remain recoverable only when their
known fields are current-format and the surviving lifecycle is valid. Invalid
interior or complete records are never crash debris; recognized instruction and
skill-lifecycle tails fail without truncation. Provider replay remains v1 and
normalized worker-log headers remain v1, independently of the new skill envelopes
and instruction/checkpoint versions. Flat and programmatic body-only definitions
have no package resource authority, and restoration never infers it.
Current provider interoperability, explicit model conversion, and current-format
semantic/crash repair are independent of these persisted-format restrictions.

## Frontend presentation boundary

The TUI does not render durable Rig messages or ACP journal events directly.
Both are adapted into pane-local semantic presentation blocks for user and
assistant text, reasoning, tool calls, checklists, placeholders, errors, and
diagnostics. This leaves session API contracts and versioned transcripts
provider-neutral while giving native and ACP panes the same Markdown, role
headers, selection, copying, and tool-row behavior.

TUI reasoning adaptation preserves summary and plaintext entries verbatim and
in source order while omitting encrypted and redacted entries. If a reasoning
item has no readable entries, it still becomes a heading-only reasoning block,
so the Assistant role header and muted `reasoning` heading remain visible and
selectable. Copying reasoning includes only readable text; a heading-only block
copies an empty string. This filtering is presentation-only: canonical provider
replay and persisted transcript data retain the complete reasoning payload.

Mutable tool calls, streamed ACP segments, and plan snapshots retain stable
block IDs and increment revisions in place. Layout caches each block
independently, including its visible role transition, so a streamed delta or
sparse tool update reparses only the changed block. Diagnostic-only ACP blocks
stay in chronological order but are filtered before layout by default; hiding
them creates neither role transitions nor blank separators.

At terminal heights of at least six rows, `SessionViews` also owns a persistent
single-line workspace header on the first physical row. `FrameLayout` gives the
row the same adaptive horizontal gutters as the composer and status line: one
column per side in compact frames and two per side in spacious frames. The
inset header and its outer gutters use the canvas, with no panel band. The bold
home prefix ` ` appears only when its measured display width leaves at least
one path cell. The home icon (`U+F015`) and Powerline branch icon (`U+E0A0`)
require a compatible terminal font, such as a Nerd Font; fonts without these
glyphs may show missing-glyph boxes.
The displayed startup path abbreviates the resolved home-directory prefix to
`~` and uses `/` separators on every supported platform before fitting or
parent/leaf styling. It uses muted parent directories and a bold accented leaf,
and is leading-ellipsized so trailing components survive. Optional ` <branch>`
or ` detached@01234567` metadata is bold, right-aligned, and trailing-ellipsized;
non-Git workspaces omit it. Home
icon, leaf, and Git label share the visible pane's status accent (Build mint,
Plan blue, or the displayed Review/Explore/inspect accent). Fitting measures
prefixes and labels in terminal display cells and clips at grapheme boundaries
without wrapping. It reserves the home prefix first, keeps a one-cell path/Git
gap, and balances the path/Git split when both overflow. Git is hidden unless
there is room for at least one path cell, the gap, and the Git prefix plus two
status cells. Spacious layouts paint a border-colored `─` rule across the same
inset width in the existing gap row; compact layouts omit that rule and gap.
Repository discovery retains the absolute startup path, and filesystem I/O
occurs only during explicit refreshes, never while rendering. Shorter frames
suppress this metadata row entirely.

Every normally sized pane owns one compact status line on its final row.
`FrameLayout` horizontally insets that rectangle with the same metrics as the
transcript and composer: one column per side in compact frames and two per side
in spacious frames. The status rectangle explicitly paints the Zevria canvas,
including unused padding; the composition root keeps its outer gutters on that
same explicit canvas. Interactive panes place it below the composer or Plan
choices; inspect panes render only
conversation body plus the status line, so they do not allocate an empty
bordered composer. The status line is passive and never becomes a focus target
or command-menu anchor.
Its primary text follows one precedence: persistence/fresh-handoff warnings,
Plan approval, inspect identity/lifecycle, selection/command/recall focus,
active compaction/reconnect/stream/wait state, then idle Build/Plan focus.

Widget-level palette tokens are centralized in `zevria-theme`. Normal primary
status text is bold and uses the displayed role's selected semantic accent
(built-in: Build mint, Plan blue, Review amber, Explore lavender); inspect uses
the tools accent.
Warnings override that accent with the warning feedback color. Detail text,
provider/model identity, inspect controls, separators, projected-context
accounting, and latest-response accounting use the shared muted color. The
ordinary composer's rounded
Build/Plan prompt uses its mode label and matching model-role accent; request-local
orchestration does not change the caption or mode. Its local state
flags remain in the bottom border, and action help remains in the shared hints
row directly above the status line when height permits. Plan, command/skill,
picker, and question controls remain in each overlay's own rounded title.

The status line resolves `provider/model` from the displayed role, projected
next-request context separately from the latest completed response, and ACP
`used/size` context without cost or a fabricated model. Width admission uses
terminal display cells and grapheme-safe truncation. Compression keeps its
fixed order: full latest-response detail, response total only, no response,
context without its source, no optional detail or inspect controls, shortened
model identity, no model identity, no projected context, and only then an
ellipsized mandatory primary state. The right telemetry group remains aligned
to the inset rectangle's right edge and the line never wraps. Interactive
frames shorter than five rows and inspect frames shorter than four omit the
status line and give the complete frame back to the existing body layout.

`SessionViews` renders the current pane first, then the workspace header and
its spacious rule, then the session picker or question popup. Picker and question
geometry is centered and clamped inside `FrameLayout::modal_body`, so overlays
may cover the transcript and composer but cannot cover the global header, its
spacious rule row, the shared hints row, or the compact status line.

ACP pane identity retains agent label, safe mode, lifecycle, and historical
state. Live `AgentRunEvent::Usage` and terminal/restored `AgentRunOutcome`
usage update an independent external context field; diagnostics toggling and
inspect navigation remain optional footer controls. ACP exposes no model
identity to Zevria, so root Review routing is never implied for a worker pane.

## Build and Plan frontend workflow

New root sessions start in **Build**. There are exactly two root modes:

| Root mode | Parent capabilities | Native Explore children | Native Build children | Root model role |
| --- | --- | --- | --- | --- |
| Build | Full ordinary parent toolset; direct implementation | Available | Only on an explicitly orchestrated request | Build |
| Plan | Restricted inspection, questions, and `submit_plan`; configurable skills | Only when `session.plan.allow_subtasks` permits | Forbidden | Plan |

`/orchestrate <prompt>` is request-local behavior in Build, not a third mode. The
parent may edit, run commands, integrate, and validate directly, but successful
completion requires a qualifying concurrent delegation batch. Worker Plan/Review
and child policies are unchanged; nominal Build child turns inherit neither root
permissions nor orchestration obligations. All five model roles remain distinct.

The idle interactive root exposes fixed zero-argument `/build` and `/plan`
commands. Each selects that exact mode, without submitting work, approving a Plan,
or starting inference. Arguments are rejected. Active work, management, recall,
worker/inspect panes, and blocking Plan decisions retain their existing guards.
`Shift+Tab` toggles only **Build → Plan** and **Plan → Build**.

`/orchestrate <prompt>` instead strips its own leading prefix and submits the
ordered remainder with typed `RequestBehavior::Orchestrate`. It never parses a
nested command/skill in the remainder. Text/image and image-only arguments work;
a bare command is a local error retaining the draft. Completion Enter only inserts
the command; Ctrl-Enter submits it. History/recall derive the prefix from metadata;
keeping it on edit creates a fresh obligation, removing it produces Standard.
Literal Standard prompts beginning with `/` or `$` receive a leading-space display
escape so recall cannot accidentally turn data into a command. ACP requires the
version-1 `zevria.orchestration` prompt metadata extension; ordinary text is literal.
Neither surface implicitly switches modes or approves/revises a pending Plan.

The acknowledged selection is session-local durable state, not a global default
and not a side effect of the next prompt. `--continue`, `/resume`, and ordinary
ACP load/resume restore it, including a switch immediately followed by close with
no intervening assistant response. Selecting a mode alone does not approve,
abandon, revise, or erase a Plan artifact. A pending **Ready** artifact forces
Plan mode and retains the existing versioned approval decision; this restoration
is not approval and cannot start implementation. Planning with a retained artifact,
Resolved history, or an ensemble **Published** artifact must not erase a later
explicit selection. Publication alone does not opt into Build or orchestration.
Fresh roots still start Build and do not inherit the source's mode selection.

One model-inert canonical record immediately follows optional `SessionModels`
at index zero; without models it must be the first item:

```json
{"zevria_session_mode":{"version":1,"selected":"build"}}
```

The version, exact ID, uniqueness, and position are validated before restoration
or writable recovery; malformed or truncated reserved mode records cannot be
silently discarded. A persisted legacy `selected: "orchestrate"` is explicitly
unsupported: loading fails before restoration/repair with guidance to start fresh
Build and use `/orchestrate <prompt>`; original log bytes remain unchanged.
Literal marker text inside a message stays ordinary text.
Supported histories without this record use their authoritative Plan state:
Planning/Ready/Published restore Plan; Idle/Resolved restore Build. Model header
changes, compaction, and transcript edits preserve the selection. `SetMode` uses
a staged atomic rewrite, returns correlated `ModeResult`, and allocates no turn ID
or provider request. Equal selections do not rewrite. Busy, read-only, restricted
profile, degraded-persistence, and Ready requests are rejected without queueing.
Accepted workflow actions use `ModeChanged` and commit their selected mode with
their workflow records: revision selects Plan; current/fresh implementation and
fresh handoffs select ordinary Build. Only the next actual request reconciles
newly selected workflow instructions; saving a selection does not send them.

The selected next mode and immutable in-flight mode are separate. Submission
captures the selected mode and model role locally. `TurnStarted` authoritatively
supplies the ordinary Build/Plan workflow mode, while ensemble workflow and pane
override retain the distinct Review, Explore, and Builder roles. All Build requests
use the saved Build model selection for `/model` and `/model-session`; Plan uses
its own saved selection.

The TUI-only `/new` command immediately starts an empty root in the same
workspace, without confirmation or a session picker. It requires eligible idle
root input with no blocking Plan dialog; it cannot run from worker/inspect panes
or replace a recalled transcript item. It takes no arguments (trailing whitespace
is accepted); submitting `/new anything` reports `Unknown command: /new anything`
and preserves the draft. The frontend returns `UiOutcome::New`, waits for normal
session shutdown, then starts `SessionStart::New` with the existing `Connecting…`
frame. The new session starts in Build mode with no conversation, Plan artifact or
handoff, session-only model selections, or active-skill history. It resolves
current global model defaults and rediscovers skills, without an opening prompt
or automatic model turn. Substantive old conversations and their related
artifacts remain available through `/resume`; roots with canonical mode metadata
are also retained without messages. Empty files and valid legacy metadata-only
roots without a saved mode are excluded and removed best-effort, while damaged
or substantive history is never safely empty. This is a fresh session,
not a full process/configuration reload: workspace contents, global settings
(including defaults saved by `/model`), and process-wide theme/startup policies
are unchanged. Cleanup failures that propagate prevent the next startup, and
fresh-start failures use the existing error reporting and terminal restoration
rather than returning silently to the old session.

Input is modal in a vim-like way, and the frontend always starts with the
input box disabled (Normal). `i` enables the input box (Insert) only while the
interactive composer is editable; `Esc` disables it again while preserving the
draft. Insert focus therefore implies an editable composer. Starting any turn
or other engine-backed operation leaves Insert for Normal and locks the
composer until the operation settles, after which the user presses `i` to begin
the next draft. Mode, focus, activity, warnings, profile, and accounting live in
the footer; the rounded composer exposes only prompt-local flags, while the
ordinary shared hints row advertises actions valid for its current state.

While the input box is disabled, including throughout an in-flight operation,
`j`/`k` and Up/Down scroll the conversation, `gg` and Home jump to the top,
and `G`/`End` re-pin the view to the bottom; PageUp/PageDown remain available
for larger steps. A downward transcript scroll that reaches or crosses the
rendered bottom also restores live-tail follow; Enter and other typing keys are
inert. A quick double `Esc` from either mode enters select mode (`j`/`k` move
between transcript items, `y`/`yy` copy, `za`/`zc`/`zo` fold). Normal and Select
share the `zm`/`zM` turn-fold and `zR` unfold-all chords described above, including
in inspect panes. These and `za`/`zc`/`zo` are the complete set of fold commands;
leaving select mode returns to Normal.

On supporting terminals, Zevria requests DEC alternate-scroll mode 1007 for
the lifetime of the alternate screen. The terminal translates wheel motion
into ordinary Up/Down input only while that screen is active and mouse tracking
is not captured. Wheel input therefore follows the current owner's existing
bindings: it scrolls wrapped transcript lines in Normal and whenever the
composer is locked, moves the selected transcript item in Select, and retains
focused-control navigation only in editable Insert, command menus,
session/question dialogs, and Plan approval. A wheel-generated Down that
reaches the transcript bottom therefore re-pins the live tail without an
explicit `G` or `End`, including while a turn is streaming. Zevria deliberately
does not enable mouse capture, so native terminal drag-to-select and copy remain
available; keyboard navigation is the fallback when mode 1007 is unsupported.

A leading `/` or `$` opens the command palette when the caret is within the
first sigil-led token. The palette is available for both fresh input and an
active transcript recall; its filter uses only the text between the sigil and
caret, so text after the caret is ignored. Accepting a highlighted row splices
the selected invocation through the caret, preserves the remaining suffix, and
places the caret after a separating space. For example, `/|hello world` can
become `/compact hello world` without losing the draft.

Unmodified `Enter` immediately submits an exact zero-argument built-in:
`/resume`, `/new`, `/compact`, `/implement`, or `/implement-fresh`. `Enter` only
accepts ensemble commands and `$skills`, which may need arguments, and `Tab`
only accepts every row. A non-whitespace suffix therefore keeps an accepted
built-in in the composer for editing rather than submitting it as a command.
The first space or newline before the caret closes the palette and restores
ordinary editor/navigation bindings. Pressing `Esc` in fresh input removes the
active completion prefix without clearing the preserved suffix; pressing `Esc`
while the palette is open during a recall cancels the complete recall and
restores the exact saved draft, cursor, and input mode. `Ctrl+Enter` remains the
general explicit submission binding, while plain Enter adds a newline for
multiline prompts and arguments when the palette is closed. Bracketed paste
arrives as one atomic editor event, so embedded newlines remain part of the
classified submission.

Composer layout greedily wraps whitespace-delimited visual words without
splitting punctuation away from the surrounding non-whitespace grapheme run.
When a width-fitting word moves to the next visual row, the internal separator
that selected the soft break is omitted from display but remains byte-for-byte
in the draft; explicit leading and trailing whitespace stays visible. Only a
word wider than the composer falls back to hard wrapping, and that fallback
splits exclusively at extended-grapheme boundaries, never within one wide
grapheme.

While the composer is editable in Insert, `Ctrl+Left` and `Ctrl+Right` move by
editor tokens across punctuation, whitespace, and explicit newlines: Unicode
alphanumeric graphemes and `_` form words, while other graphemes are
separators. `Ctrl+K` deletes from the caret to the current logical line end
without removing its newline or joining lines. `Ctrl+Shift+K` deletes the
current newline-delimited logical line together with its following delimiter,
or with its preceding delimiter when deleting the final line. A caret directly
before `\n` belongs to the preceding logical line. Terminals with an enhanced
keyboard protocol can report `Ctrl+Shift+K` distinctly; legacy terminals may
collapse it to the same event as `Ctrl+K`, in which case the frontend can only
perform the reported `Ctrl+K` behavior.

`Ctrl+Z` undoes and `Ctrl+Y` redoes local draft edits only while the composer
is editable in Insert, including an open completion menu or an empty draft.
They never submit input or alter accepted transcript history. Consecutive
character typing (including spaces) is one undo step; consecutive Backspace
is another. Grouping is action-based, not timed. Cursor/menu navigation,
mode/focus changes, and different editing actions close the current group.
Enter/newline, multiline terminal or clipboard paste, completion acceptance
or dismissal, either line-deletion command, and image attachment are individual
transactions. New content edits after undo discard redo; navigation, failed
edits and no-ops do not. Empty history stacks are harmless.

Ctrl+C's local nonempty-draft clear is undoable, including when a clipboard
read is pending. Undo restores only completed draft text, grapheme-safe caret,
registered image occurrences/ranges, and image ordinals—not canceled clipboard
work, engine operations, or application lifecycle actions. Canceling a pending
paste without changing content adds no undo entry. Undo/redo increment the live
draft generation instead of replaying an old generation, so stale clipboard
results and worker acknowledgements remain invalid even if visible content
matches again. Completion navigation and preferred-column state are reset on
replay; wrapping and scrolling are recomputed from the restored content.

History is memory-only and local to each composer/draft, bounded to 100
transactions across both stacks. Image snapshots share `PromptImage` backing
rather than duplicating encoded bytes. Accepted submissions, executed commands,
session restoration and other lifecycle resets discard history. Recalled
prompts start a fresh editing baseline; canceling recall restores the original
draft and its history. Rejected submissions and slash-mode changes restore the
saved history with a closed typing group; non-destructive mode selection and
temporary pane navigation preserve it. Acknowledgement identity includes
content, cursor, image ordinals and live generation, never history bookkeeping.
Question dialogs, pickers and Plan approval retain their own input ownership
and do not gain composer undo. In worker panes, Insert-mode Ctrl+Y is always
redo; confirmation uses `/confirm` or Normal-mode Ctrl+Y/`c` with the existing
eligibility and unsent-draft checks.

Fresh input and recalled rows use one classifier. Ordinary input becomes a
trimmed message; an exact known `$skill [arguments]` becomes a typed skill
turn; and exact `/ensemble-plan <prompt>` or `/ensemble-review <prompt>` input
becomes a typed ensemble run. `/orchestrate <prompt>` becomes a typed request
modifier and is also supported on recall. `/build`, `/plan`, `/resume`, `/new`,
`/compact`, `/implement`, and `/implement-fresh` are fresh-only built-ins: during a recall, immediate
Enter submission reaches the existing local notice instead of replacing
history. Unknown commands, unknown skills, and ensemble commands missing a
prompt are local validation errors that leave the composer and active recall
untouched. A raw leading space is the literal escape hatch:
` /ensemble-plan text` and ` $skill text` are persisted as ordinary trimmed
messages rather than classified invocations.

Select mode exposes `Ctrl+E` on user prompt rows, persisted skill-invocation
rows, and an ensemble entry's command row (never its worker rows). Prompt
addresses are zero-based over only user-message and skill-invocation records;
ensemble starts never enter that numbering and use their stable run ID. Once
recalled, either target supports the same replacement matrix:

| recalled target | plain or leading-space-escaped text | exact `$skill [args]` | exact Plan/Review ensemble command |
| --- | --- | --- | --- |
| prompt ordinal, including a skill row | message turn | skill turn | fresh ensemble run |
| ensemble run ID | message turn | skill turn | fresh ensemble run |

The leading-space marker is not stored in JSONL. Consequently, recalling an
older literal message whose stored text begins with a now-recognized `/` or
`$` reclassifies it; add a leading space again to keep it literal. This is an
intentional provenance tradeoff that avoids changing the transcript format.

The core resolves either target without mutation, prepares validation, Plan
state, optional edit compaction, launcher configuration, and worker
descriptors against the retained prefix, then performs one durable
append-or-replace commit. For an edited message or skill, any folded automatic
checkpoint is part of the same tail rewrite; `CompactionCompleted` is emitted
before `TurnStarted`. For an ensemble replacement, a fresh run ID and fresh
worker IDs are committed before any worker launches. `TurnStarted` accepts
message and skill replacements, while `EnsembleStarted` accepts ensemble
replacements. Only at that event does the TUI prune the old rows and worker
panes. Validation, cancellation, configuration, worker discovery, compaction
preparation, or rewrite failure before acceptance leaves the original tail
intact in memory and on disk. Pre-acceptance refusals emit `TurnRejected` and
never append synthetic Error records, for append as well as edit commands.
Failures after acceptance belong to the newly durable replacement and never
restore discarded history.

Restoration rejects any malformed skill or Plan replay, including records in a
potentially discarded tail. Edits of valid history remain supported, including
rewinding a valid Ready Plan: preparation uses the retained prefix rather than
letting the discarded Ready state gate the replacement. Both reducers validate
the complete proposed branch before its durable acceptance. An invalid
uncommitted proposal is an ordinary rejection and does not poison the healthy
current branch. After a commit, both domains are installed together; unexpected
reduction failure is terminal. Successful replacement invalidates provider
continuation and prepared counts, refreshes skill policies and live queries,
re-estimates context usage, and recomputes automatic-compaction arming.

The first accepted Plan prompt appends `PlanRecord::Started` and enters
`Planning`. Clarifications and partial responses complete normally and remain
`Planning`; `TurnCompleted` has no approval meaning. The model explicitly
finishes through `submit_plan`. After its short confirmation succeeds, the
engine appends `PlanRecord::Ready` containing the complete canonical Markdown,
projects it to
`.zevria/plans/<session-id>/<plan-id>-<first-title-slug>.md`, emits
`TurnCompleted`, and then emits `PlanStateChanged::Ready`. A projection failure
is only a warning because the transcript is canonical. Projection writes occur
only at Ready transitions, including recovery that commits a new Ready artifact;
restoring an already committed artifact does not project it again.

The native Ratatui transcript renders the call itself as one compact
`submit_plan · <title> · <status>` row. It never renders the raw arguments or
canonical Markdown there, hides the successful acknowledgement, and surfaces
only unsuccessful result text. The complete Markdown remains visible once in
the dedicated, selectable Plan artifact entry.

The Ready snapshot renders a dedicated, selectable Plan artifact plus three
versioned actions: implement here, implement fresh, or revise. `Plan ready` or
`Submitted plan` lives in the footer; the choice border retains only the exact
artifact version and approval controls. Every action
sends `ResolvePlan { expected: PlanVersion, decision }`; stale versions are
rejected without changing state. Revision retains the ID, projection path, and
last submitted artifact and increments the next artifact's revision. `Esc`
enters that revision state and returns the composer to Normal mode; `p` reopens
the three-choice dialog for the retained version. `/implement` resolves it in
the current conversation and `/implement-fresh` resolves it through a fresh
typed handoff, even after additional unfinished revision discussion. An accepted
Standard Build prompt while Planning still abandons the unfinished thread;
simply selecting Build does not. Explicit orchestration is rejected while Planning
or Ready, without revising, abandoning, or approving the Plan. Ordinary submissions while Ready are rejected,
and mode commands cannot substitute for the version-checked decision.

Current-session approval atomically appends `Resolved` and a typed `Handoff`
record, emits a semantic handoff row, and starts an ordinary Build turn. Fresh
approval emits `FreshPlanHandoffRequested`; the manager creates an ordinary Build
session and sends `StartFromPlan` with that same typed value. Both current and
fresh handoffs always select Build with a new Standard request boundary,
regardless of an earlier orchestration request or whether the artifact was Ready
or Published. No frontend
constructs a magic prompt or turns the handoff into generic text. A failed fresh
startup leaves the source Resolved and retryable.

`Started`, `Ready`, `RevisionRequested`, and `Resolved` records are not model
visible. `Handoff` is the sole workflow record projected into model history.
An implementation resolution may follow `Ready`, `Published`, or a matching
retained artifact in `Planning`; no draft revision prose is promoted into the handoff.
Construction/restoration validates and installs state from typed transcript
records, and hosts seed frontend presentation from the engine-backed snapshot.
Thus `Planning`, pending Ready approval, and resolved handoffs survive restart
without an engine startup event. Durable mode selection is restored separately:
Ready forces Plan, but retained/Published artifacts and older resolved handoffs
do not override a later explicit selection. Startup never scans or repairs
Markdown projections: edited or stale files remain untouched and deleted files
remain absent, including after a crash between Ready persistence and projection.
Manual Markdown changes never become approval, revision, or handoff input; the
transcript artifact remains authoritative. A subsequent accepted Ready revision
can overwrite the same projection path with canonical artifact content.
Editing any prompt or ensemble row atomically truncates later workflow records
and installs state reduced from only the retained prefix, so workflow records
in the discarded tail cannot gate or alter the replacement.

`[session.plan]` configures `max_artifact_bytes` (default and hard ceiling
131072), `allow_subtasks`, and `allow_skills`. These settings can only narrow
Plan's optional workflow capabilities; they cannot add structured mutation
tools or native Build children. Request-local orchestration cannot override these settings.
Plan always receives `command`, whose inspection-only use is enforced by
instructions rather than a shell sandbox.

## Model/UI metadata boundary

Tool execution produces two structurally separate outputs:

```text
model_output: String   -> Rig ToolResult Message -> Conversation -> ModelRequestItem
FileChanges extension  -> ToolResultMetadata     -> SessionEvent / transcript / TUI
```

The conversation's single source of truth is `Vec<TranscriptItem>`;
`SessionEngine::history()` is its plain-message projection. Each
`ModelRequestItem` carries a Rig message and, for a provider assistant response,
an optional `ProviderReplay` ledger. That ledger is provider wire state, not
display metadata. For Responses-protocol output it is also the sole durable source of
the assistant message: model's validated replay constructor fallibly derives one
canonical Rig message from the ordered native items. Transcript admission moves
the validated message and ledger without cloning or re-derivation, caches that
message inside the `TranscriptItem`, and serves the same borrowed message to the
engine, tools, history projection, and TUI. Callers cannot construct a provider transcript item from an independent
message/replay pair.

`ToolResultMetadata` has no conversion into `Message`, `Text`, or
`ToolResultContent`. The engine copies only `ToolExecutionResult::model_output`
into the result message and extracts known Rig result extensions into a
separate display sidecar.

Completed result batches are persisted atomically as one JSONL object. The
object retains the normal top-level Rig message fields and adds the reserved
`zevria_tool_result_metadata` key, including typed subtask outcomes. Skill tool
results also carry hidden `zevria_skill_applications` entries correlated by local
call ID. Direct skill applications use a standalone `zevria_skill_invocation`
envelope with a validated name, ordered text/image arguments, and an application.
An activation embeds its full snapshot; reapplication identifies the pinned name.
There are no standalone activation records or invocation version fields. Pins
remain hidden metadata; only the compact invocation projects as a user message,
with ordered `zevria_skill_directive` records supplying model-facing bodies.
Plan workflow records occupy a standalone `zevria_plan` object. `Ready` stores
the complete artifact, so replay never consults assistant prose or a generated
Markdown file; `Handoff` stores typed metadata and its engine-generated
model-visible prompt. Provider assistant messages instead persist a versioned native item ledger
under `zevria_provider_replay`, with an optional sibling `zevria_display_attempt`
link (no duplicate canonical message fields):

```json
{"zevria_provider_replay":{"provider":"openai.responses","version":1,"source_profile":{"provider":"openai","model":"gpt-5.6-sol"},"items":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Done."}]}]}}
```

Hosted-search checkpoints use a separate `zevria_web_search_attempt` line. They
are full during the crash window before link commit. Committing the replay and
its `zevria_display_attempt` link atomically rewrites the checkpoint to elide only
its derivable presentation, for example:

```jsonl
{"zevria_web_search_attempt":{"version":1,"id":"A","profile":{"provider":"openai","model":"m"},"response_id":"resp_1","outcome":"completed","activity":[{"item_id":"ws_1","output_index":0,"status":"completed","action":{"type":"search","query":"q"}}],"revision":5,"presentation":[],"presentation_elided":true,"terminal":{"0":"completed"}}}
{"zevria_provider_replay":{"provider":"openai.responses","version":1,"source_profile":{"provider":"openai","model":"m"},"items":[{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"q"}},{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Done."}]}]},"zevria_display_attempt":"A"}
```

`presentation_elided` defaults to false and is omitted when false, so failed,
interrupted and unlinked checkpoints keep their existing full wire shape. The
flag requires a completed attempt with empty stored presentation and a later
exact-ID linked native replay. Restore reconstructs readable reasoning, answers
and native-tool bindings from that ledger, never by text or proximity. Attempt
activity and lifecycle/terminal evidence are retained even when the final ledger
omits a live observation. Ordinary assistant display links have no native ledger
and do not elide presentation.

`openai.responses` is the persisted wire-protocol identifier and is unrelated
to a configured provider key such as `openai`, `deepseek`, or `glm`. Exact
source-profile history uses native items losslessly. A foreign target receives
a deterministic portable projection containing visible text/refusals and
re-correlated function calls/results; reasoning, encrypted/opaque data,
unknown native output, message/item IDs, and provider call IDs are omitted.
Replay-only opaque checkpoints cannot cross profiles without confirmed portable
conversion. Roots with more than one selectable catalog profile force local-summary
compaction, even when all assigned roles currently share a profile. Genuinely
single-profile roots and single-profile Explore or Builder children may use configured remote
Responses compaction.

Deserialization validates the envelope and reconstructs the cached canonical
message. Unsupported replay versions and malformed current-v1 records, including
records without `source_profile`, are hard resume errors rather than malformed
lines that recovery may skip. Mixed legacy message-plus-replay records,
malformed current envelopes, and replays that cannot produce assistant content
are rejected; there is no plain-assistant fallback. A
successful launch's `ToolResultMetadata` additionally carries an optional
`subtask` field correlating the row with its child subsession. The transcript's
`model_history` accessor returns only inner messages, never metadata. A
current-format incomplete final line without a newline can be recovered as one
whole append if all compacted attempts still have their linked replays;
recognizable unsupported formats and orphaned compacted attempts are never
discarded as crash debris.

Child transcripts live under `.zevria/subsessions/<root-session-id>/`, outside
the sessions directory, so `latest_session_file` and `--continue` see only
root sessions. On resume the composition root reloads child transcripts into
inspect-only historical panes with the same body-plus-footer layout and shared
configured-profile fallbacks as live children.

## Add a wire adapter

Configured provider keys already share the built-in Responses adapter. Adding a
new key or model requires only TOML. A genuinely new wire protocol would need a
compiled adapter:

1. Create a crate depending on `zevria-session-api`, `zevria-model`, the lower
   value owners it uses, and `rig-core`; do not depend on the engine.
2. Define only protocol-specific configuration/runtime types in that crate.
3. Implement `ModelProvider` for an owned router or adapter. It must:
   - resolve `ModelRequest.model_role` to an immutable profile before dispatch
     and keep that selection fixed for all native retries;
   - send `ModelRequest.instructions` as the sole top-level instructions and
     project ordered `DeveloperInstruction` skill input verbatim; reject raw system
     messages instead of lifting them into request properties;
   - validate maintenance input structurally: no ordered directives, with the
     no-tool maintenance policy carried by `request.instructions`;
   - advertise every registered tool when `allowed_tool_names` is `None`, or
     only the explicit names when it is `Some`;
   - preserve that policy snapshot across provider-native retries and
     continuations;
   - treat `reset` as "local history has diverged from any provider-side
     state": the next `complete` must send the full request history, never
     resume a native continuation.

```rust,ignore
impl ModelProvider for MyProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let message = self.client.complete(request).await?;
            progress.stream_updated(message.clone());
            ModelResponse::plain(message)
        })
    }

    fn reset(&mut self) {
        // Local history has diverged from any provider-side continuation
        // state (a failed turn, or subtask results committed between
        // requests); the next complete() must send full request history.
    }
}
```

Inject the shared `ToolServerHandle` if the provider advertises tools. A
provider must not execute tool calls, write transcripts, or emit terminal/UI
events. Select it in app by constructing `SessionEngine<MyProvider>`;
no core switch statement is required. Do not introduce a vendor enum merely to
name another endpoint that already speaks Responses.

Tests should use a scripted local transport and pin protocol conversion,
streaming, retry limits, reset behavior, and provider-native continuation.

## Add a Rig tool

Implement Rig's existing `Tool` trait in `zevria-tools` (or another compiled
crate), then register it in app's appropriate root/child/worker registry:

```rust,ignore
let tools = ToolServer::new()
    .tool(CommandTool::new(workspace.clone()))
    .tool(TaskTool)
    .tool(LaunchSubtasksTool::new(launcher, workspace.clone()))
    .tool(EditTool::new(workspace.clone()))
    .tool(WriteTool::new(workspace.clone()))
    .tool(DeleteTool::new(workspace))
    .tool(SkillTool)
    .run();
```

Pass cloned handles to both provider and engine. This guarantees that the
advertised definition and executable implementation come from the same
registry. Tool errors should be descriptive: the engine returns them to the
model as correlated tool-result text rather than failing the session.

Test the tool independently, then add an engine test for schema visibility,
call correlation, ordering, and error output where relevant.

## Add a frontend

A frontend needs only command/event channels. It does not need provider access,
the tool registry, transcript ownership, or terminal knowledge in core.

```rust,ignore
let (command_tx, command_rx) = unbounded_channel();
let (event_tx, mut event_rx) = session_event_channel(256);
let mut engine_task = tokio::spawn(engine.run(command_rx, event_tx));

loop {
    tokio::select! {
        result = &mut engine_task => {
            // Distinguish Ok(Ok(())), Ok(Err(replay_error)), and Err(join_error).
            observe_runtime_exit(result);
            break;
        }
        Some(update) = event_rx.recv() => match update {
            SessionUpdate::Lifecycle(event) => reduce_lifecycle(event),
            SessionUpdate::Streams(batch) => apply_changed_streams(batch),
        },
    }
}
```

Send `SessionCommand::Turn(TurnCommand)` work and select on the single ordered update
receiver. Reduce every lifecycle event losslessly and apply each stream batch
as changed-target state; the receiver has already coalesced superseded values
and fenced stale ones. Do not retain a second stream watcher or recreate merge
logic in the frontend. Treat `TurnStarted` as the authoritative user-message
and durable acceptance
boundary. Locally lock every engine-backed action before acknowledgement so
duplicate input cannot race the engine; while pending, `Ctrl+C` targets the
unnamed active operation, and after promotion it targets the learned turn ID.
Treat `PlanStateChanged` as the authoritative workflow snapshot, do not copy
the artifact into dialog-only state, and send versioned `ResolvePlan`
decisions while disabling duplicate confirmation until the workflow settles
or fails.

App's `runtime::RunningSession` owns provider startup, transcript restoration and
repair, the engine and supervisors, lease retention, background-failure observation,
ordered shutdown, and empty-transcript cleanup. A frontend borrows read-only
restoration data, takes the sole event receiver and a cloned command sender, then
drops those endpoints before awaiting runtime shutdown. Neither the authoritative
cleanup path nor the runtime's task/lease ownership is publicly mutable. App's
`acp_host` implements `zevria-acp`'s object-safe runtime factory over that handle;
this keeps provider and production-tool dependencies out of the ACP frontend crate.
Validated application configuration likewise exposes only read-only startup views,
so its source values cannot diverge from derived routing and compaction policy.

The Ratatui implementation demonstrates the separation:

- [`app.rs`](../crates/tui/src/app.rs) — public façade, input coordinator, and
  effectful cross-domain reducer, with focused state modules under
  [`app/`](../crates/tui/src/app/);
- [`composer.rs`](../crates/tui-input/src/composer.rs) — grapheme-aware editor and
  command/skill completion state;
- [`frame_layout.rs`](../crates/tui/src/frame_layout.rs) — private frame allocation
  and lower-surface height policy, kept with App;
- [`chrome.rs`](../crates/tui-widgets/src/chrome.rs) — bounds-checked painters for the protected workspace row and adaptive gap,
  gutters, accents, brackets, prompt chrome, and the horizontally inset status
  rectangle;
- [`workspace_header.rs`](../crates/tui-widgets/src/workspace_header.rs) — startup-path
  state, direct Git `HEAD` discovery/refresh, and adaptive one-row rendering;
- [`zevria-theme`](../crates/theme/src/lib.rs) and its [modules](../crates/theme/src/) —
  immutable selected semantic tokens, strict concrete documents, deterministic
  OKLCH generation, contrast/CVD validation, and the unchanged built-in fallback;
- [`render.rs`](../crates/tui/src/render.rs) — borderless transcript, lower
  surfaces, and cache-aligned visible-window rendering;
- [`status.rs`](../crates/tui/src/status.rs) — semantic status views and
  accents, adaptive display-cell compression, and explicit default-background
  span rendering;
- [`text.rs`](../crates/tui-widgets/src/text.rs) — shared grapheme-safe terminal-width
  helpers, including leading-ellipsis path truncation, used by global chrome,
  footer, and modal titles;
- [`layout.rs`](../crates/tui/src/layout.rs) — incremental layout cache so a
  frame re-renders only changed entries and draws only the visible window;
- [`syntax.rs`](../crates/tui-widgets/src/syntax.rs) — shared Syntect grammar lookup,
  Zevria theme mapping, and stateful highlighting for Markdown fences, command
  previews, and structured file diffs;
- [`diff_render.rs`](../crates/tui-widgets/src/diff_render.rs) — grammar-aware
  structured file-change rendering that is complete for `write`/`delete` and
  bounded by rendered wrapped rows for `edit`;
- [`runtime.rs`](../crates/tui/src/runtime.rs) — terminal, channels,
  clipboard side effects, and the session-view manager that owns global
  workspace chrome and routes tagged child events into inspect-only panes;
- [`markdown.rs`](../crates/tui-widgets/src/markdown.rs) — Markdown parsing and layout.

The sibling [`zevria-acp`](../crates/acp/) frontend builds an ACP V1 Agent over the same channels. It owns session registration, protocol correlation, append-only stream segmentation, transcript replay, form elicitation, and Plan decisions, but receives provider-neutral runtime handles from app. It never initializes Ratatui/crossterm and never depends on provider or production-tool implementations.

Headless frontend tests can consume the event receiver directly; no terminal
or provider-specific types are necessary.

## Compatibility boundaries

Build/Plan selection is durable session-local engine state, distinct
from the immutable in-flight request mode and from the Build/Plan model identities.
Mode-only changes do not require a subsequent prompt to survive close/resume.
The Plan workflow is independently reconstructed from standalone `zevria_plan`
transcript records, so pending approval is neither ephemeral nor inferred from
message text. Ready forces Plan without approving it; retained or Published
artifacts do not overwrite an explicit selection. New sessions and both typed
Plan implementation handoffs start Standard Build. Orchestration is typed,
request-local opt-in, not a persisted mode; it adds no model role or assignment,
and worker Plan/Review remains unchanged. Older workflow events, saved Plan files, and skill
lifecycle formats are not migrated. Unsupported history rejects the entire open
before restoration or writes; no partial session is activated. Bare message/error
JSONL shapes and current reserved tool-result sidecars remain distinct from typed
workflow records. The
Rust adapter APIs and visible Ratatui workflow may evolve with the compile-time
architecture.

The session workflow refactor changes the Rust API and command/event protocol:
commands are grouped into Turn/Control/Manage, `handle_turn` replaces the contextual
command API, and pre-acceptance refusals emit `TurnRejected`. All in-workspace
consumers use this protocol. Saved mode selection must be preserved by transcript
replacement, compaction, and restoration independently of artifact replay. Only
supported persisted formats are accepted; backward compatibility is not required,
and message text or historical Plan artifacts are not a mode-migration mechanism.
