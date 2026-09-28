# TUI interaction contract

The terminal frontend is a projection of session/workflow authority, not a source of
implicit approval. UI state and diagnostics never enter provider instructions,
tool registration, model messages, or tool-result content.

## Ownership and capabilities

Resolve a surface once before handling user input. Ignoring an unsupported input
ends dispatch; it does not send the input to an underlying surface. Infrastructure
(resize, focus, clocks, engine events and correlated asynchronous results) is not
user input. Modifier and press/repeat/release policy is normalized before routing.

| Surface | Capture | Ctrl-C | Navigation target | Pane shortcuts |
| --- | --- | --- | --- | --- |
| Composer/completion | Pane-local | Clear its draft/pending paste, then pane cancellation | Editor or completion list | Yes, except completion owns Tab |
| Root transcript/selection | Pane-local | Cancel eligible work; quit only when idle | Transcript | Yes |
| Live worker transcript/selection | Pane-local | Cancel only bound eligible worker input | Transcript | Yes |
| Historical/child transcript | Read-only | No root cancellation or quit | Transcript | Yes |
| Plan review | Pane-local | Hide, without a workflow decision | Reviewed transcript; arrows select choices | Yes |
| Question | Global | Correlated Dismissed response | Form/editor | No |
| Model picker | Global | Cancel correlated management request; retain correlation until settled | List/details/editor | No |
| Session/skill picker | Global | Dismiss only this surface | List/details/editor | No |

Global paint order, from lowest to highest, is skills, sessions, models, questions.
Suspended surfaces retain state. Only the resolved owner can install the cursor.
Text in a dialog belongs to the dialog, never the pane composer.

Editing, work submission, control eligibility, transcript rewriting, management,
and navigation are independent capabilities. Root and live-worker drafts remain
editable during work. Historical/frozen panes cannot compose. New work admission
locks before acknowledgement; a hidden Ready Plan still blocks new work.

## Transactions and activation

Submission binds an immutable draft and monotonic content generation to its
operation/target. Acceptance cannot clear a newer draft, even if edit/undo returns
to identical text. Rejection restores only into the untouched replacement slot;
otherwise both drafts are retained for explicit recovery. The UI advertises a
saved rejected draft; with an empty composer in Normal mode, `r` restores it.
Recovery takes precedence over the fresh-Plan `r` retry alias, while Enter remains
the explicit fresh-handoff retry. A `z` prefix cannot activate recovery accidentally.
Cursor position is not
an acknowledgement identity. Clipboard delivery additionally checks origin,
request, generation and cursor; cancellation does not release an executing native
clipboard operation's permit.

In completion, unmodified Enter accepts the highlighted row and immediately
submits it only if the accepted entry is a built-in and the completed **whole
draft** classifies as a parameterless built-in. This applies to root and live-worker commands, including `/implement`,
`/confirm`, and irreversible `/abandon`, under their existing eligibility checks.
Tab and Ctrl-I only complete text. `/orchestrate`, `/ensemble-plan`, `/ensemble-review`, and
`$skill` entries and workspace file references remain completion-only on Enter,
even with an existing prompt
suffix. Nonblank control suffixes and attached images remain intact without
automatic submission. Enter with no matching row is inert; outside completion,
Insert-mode Enter inserts a newline. Only press events activate, not repeats or
releases; selecting or highlighting a row never runs it.

Ctrl-Enter classifies the actual draft without accepting the highlighted row and
submits an eligible work item or control. Invalid classification retains the
complete draft. Enter activation uses the same submission path: busy, recall,
read-only, pending clipboard/control, exact Plan-version/worker-revision and
acknowledgement protections are unchanged. Root and worker command namespaces
are disjoint. Leading space escapes command classification; worker `//` escapes
a literal leading slash.

Successful prompt dispatch returns the pane to **Normal** input mode immediately,
without changing the selected Build or Plan mode or waiting for
engine acknowledgement. This includes ordinary and image-only prompts, skills,
ensembles, recalled-prompt replacements, and live-worker feedback. The composer
cursor is hidden and navigation keys act on the transcript. Press `i` to resume
editing, including while work runs, subject to existing locks. Later
acknowledgements, failures, and completion events do not override that chosen
focus. Empty, invalid, or blocked attempts retain their focus and complete draft;
management and control slash commands keep their existing focus behavior. Root
drafts still stage at dispatch, while worker drafts clear only on a matching
acknowledgement and only if unchanged.

`/build` and `/plan` are mode controls; Shift-Tab toggles only those modes.
`/orchestrate <prompt>` is a one-request Build modifier, not a mode command or
skill. It strips only its own prefix and never reparses the remainder as a
command or skill. It accepts ordered text/images, including image-only arguments;
a bare command retains the draft. Plan/worker/capability/concurrency rejection
also retains or recovers the complete draft through normal admission handling.
It neither selects Build nor approves/revises a pending Plan.

History and recall derive the `/orchestrate ` prefix from typed request metadata,
not model-visible prompt text. Replacing that recalled prompt while keeping the
prefix creates a fresh orchestration obligation; removing it creates a Standard
request. Literal Standard prompts beginning with `/` or `$` are displayed/recalled
with a leading-space escape so recall cannot accidentally activate a command or
skill. Request activation/correction directives themselves are hidden, noneditable
engine records, not extra user prompts.

Plan review defaults to **Revise**. Arrows clamp; numbers and `n` only select.
Enter explicitly activates the selected eligible action against its exact version.
Esc/Ctrl-C hide initial or recovery review without deciding. `p` reopens Ready or
retained review. Repeated snapshots preserve local dismissal/choice; a new version
can open a new review. Hiding an already-dispatched decision does not retract it.
Recovery Revise returns to the existing revision flow. Publication, prose,
protocol completion, passive highlighting, text-only completion and dismissal
are never confirmation or implementation authority. Explicit keyboard activation
of an eligible choice or parameterless command is a user action, not implicit
approval from those passive events.

Insert Ctrl-Y is always redo, including completion and an empty redo stack.
Worker `c`/Ctrl-Y confirmation shortcuts require Normal focus, an empty draft,
and an eligible exact proposal revision. Ineligible confirmation is not advertised
as an executable shortcut. A Normal-mode composer is not labeled locked merely
because it lacks editing focus. Pending durable transcript edits do retain a
stronger draft/paste/undo lock until acceptance or rejection.

## Workspace file references

`@` opens **Files** in editable Insert-mode root and live Plan-worker composers,
including while work runs. It activates at the start of the draft or after
whitespace, across multiline prompts and within `$skill` or `/ensemble-plan`
arguments. Multiple references are independent. Email addresses (`a@example.com`)
and escaped sigils (`\@literal`) do not activate. Historical/frozen panes and
global dialogs retain their existing ownership rules; this adds no new command
namespace or work-submission capability.

Search matches the decoded prefix before the caret, while acceptance replaces the
**entire active reference token**, including an existing filename suffix. Full
workspace-relative paths distinguish duplicate basenames. Matching is deterministic
and case-insensitive: exact basename, basename prefix, path substring, then fuzzy
subsequence windows, preferring tighter matches and finally relative-path order.
An empty query uses stable path order rather than recent-use history.

Generated references use `/` separators on Windows, Linux, and macOS, from
discovery through ranking and insertion (for example, `src/main.rs`). The
workspace header uses the same slash-based presentation (`C:/work/zevria` or
`~/work/zevria`) with component-aware home abbreviation. These are display/text
boundaries only: workspace identity and filesystem operations retain native
paths. Literal backslashes in Unix filenames are preserved and remain part of
the basename, not directory separators.

References are ordinary prompt text: `@src/main.rs`, or `@"docs/my file.md"` when
whitespace, quotes, or backslashes require quoting. Inside a quoted reference,
`\"` encodes `"` and `\\` encodes `\`; decoding reverses the escaping. While
editing a query, backslash escapes the next character (including whitespace).
Acceptance adds a space or reuses following whitespace, retains text outside the
active token, preserves registered image occurrences, and is one atomic undo/redo
step with caret restoration. A query range never crosses an image attachment.

Up/Down, PageUp/PageDown, and Home/End navigate the clamped completion list.
Enter, Tab, and Ctrl-I **only insert** a file reference; they do not submit or run
a control. With no selectable file they are inert. Ctrl-Enter submits the actual
draft under existing eligibility checks, not the highlighted file. Unresolved or
manually typed references stay literal. Esc dismisses Files without deleting text,
including during recall; a later query/caret change may reopen it. Slash/skill
activation and cancellation keep their existing behavior.

Discovery uses the startup workspace passed to `SessionViews`, not an inferred
Git root or a later process working directory. A single lazy background worker
per UI session owns traversal and matching, sharing its index between eligible
panes and coalescing the latest request. It respects `.ignore`, `.gitignore`,
applicable parent rules, Git excludes, and configured global Git ignores; Git is
not required. Non-ignored dotfiles, hidden configuration directories, and binary
files are included. `.git` metadata and directory references are excluded.
Directory symlinks are never followed; file symlinks are offered only if their
canonical targets are regular files inside the workspace. Non-UTF-8 and
control-character paths, broken links, and outside-root links are omitted.

Index limits are **100,000 files**, **32 MiB of stored path data**, **200,000 yielded
traversal entries**, and **depth 64**, with at most **50 returned matches**. Diagnostic
storage is bounded counters, not an accumulating list of filesystem errors.
Loading/refreshing, no-match, unavailable, and partial-index states are distinct;
partial status remains visible below a short list. Reaching a traversal/depth
boundary is conservatively reported as partial even if nothing further exists.
Reopening refreshes the index; cached suggestions may remain available during
refresh and a still-present selected path retains its highlight. There are no
filesystem watchers. Paths and ignore rules can change between refreshes.

Requests/results are correlated by service lifetime, stable pane identity, popup
activation/request ID, composer generation, caret, and query range. Esc, draft
changes/restoration/submission, pane retirement, and ownership transitions retire
obsolete responses. Late results never reopen a popup, modify another pane, or
steal focus. Rendering paints prepared suggestions only; it never walks or matches
the workspace. Teardown cancels cooperatively without blocking the event loop on
an OS filesystem call that might not return immediately.

**No file contents are attached, snapshotted, or automatically read.** There is no
line-range syntax, directory completion, external-path completion, new ACP content
type, or transcript format. Discovery data, suggestions, and diagnostics remain
transient UI state outside `ComposerDraft` and edit history. Installing results
does not change clipboard/submission acknowledgement identity. Only accepted or
manually typed reference text enters the existing `UserPrompt`; provider
instructions and their cacheable prefix remain unchanged.

## Geometry and event acceptance

PageUp/PageDown use `max(1, measured visible rows)` of the declared viewport.
Normal/Select page the transcript; editors page editing rows; completion/pickers
page lists; details page content. Plan review pages the reviewed transcript.
Home/End belong to the same target. Lists clamp. Resize/activation invalidate
measurement, not reusable content; repeated navigation before redraw uses the
last valid allocation, or the one-row fallback after invalidation. Model reasoning
choices and skill selections are revealed in their measured viewports. Skill detail
paging pans details without moving a hidden selection; filter text is revealed
within its dialog. FrameLayout alone allocates the application frame.

Normal-mode `[` / `]` navigate to the nearest turn start strictly above / below
its logical viewport top. From mid-turn, `[` returns to that turn's beginning
before reaching the preceding turn. Exact-boundary presses skip the current start;
there is no wraparound, and a missing destination preserves both position and
follow intent. These are Normal-only catalog bindings, advertised in Navigation
help but not the compact footer. Brackets remain literal editor text; Select-mode
Ctrl+U/Ctrl+D retain their existing user-message traversal and scope semantics.

Destinations are displayed prompt boundaries: native `NativeHeader::Prompt`
entries (ordinary, request and skill prompts, approved Plan handoffs, and numbered
ensemble commands), or grouped user blocks in headerless/ACP entries, including
images and non-editable prompts. Assistant content, tools/results, Plan artifacts,
workflow metadata, diagnostics, errors, compaction dividers and worker-status rows
are not turn starts. Selection body ranges and rendered label text are not boundary
sources. Layout resolves entry extents and block decoration geometry, excludes
leading separators and hidden zero-height targets, and merges stops sharing one
folded start. Navigation preserves folds rather than expanding them.

Successful turn jumps remain Normal, leave draft/cursor/selection untouched, detach
bottom-follow, and align the destination header with the **first conversation
content row**, not the status/header area. A conversation-local trailing blank
extent permits this even for a short final turn or a transcript shorter than the
pane. Real content rows remain separate for painting, bottom-follow and downward
re-pin; shared Viewport clamping is unchanged for all other surfaces. Ordinary
scrolling/Home/End retire explicit alignment; `G`/End restores ordinary follow.

Explicit alignment retains semantic identity across resize, folding, diagnostics
and streaming changes, and takes precedence over older body scroll anchors.
Invalid geometry defers resolution until a usable render; multiple jumps remain
ordered, while later explicit navigation cancels stale pending jumps. Block targets
follow presentation identity; special entry targets are scoped to the projection
epoch. Tail edits, restore/replacement and pane retirement retire invalid targets
and pending intent rather than reusing positional targets in a new projection.

Metadata requires an exact active turn, stream/progress requires Running phase,
management requires an exact pending request, and late child/review updates need
explicit stable-identity fences. Acceptance is pure and precedes tail/retry/content
mutation. Hosted metadata has its own tail policy and does not erase streams.

## Projection, restore and boundaries

Logical messages, stable presentation identities, provider call IDs, transcript
edit ordinals and authoritative workflow versions are distinct. A shared
projection-local allocator supplies native, ACP, hosted and child block IDs, without
bit-packed namespaces; whole replacements receive a new projection epoch. Selection and
folding retain message boundaries. Presentation-looking Plan content is not an
authoritative artifact. Unknown outcomes are not success, and lifecycle is not
outcome. Cancelled/interrupted is warning; abandoned/dismissed is muted.

Restore replaces a projection and installs authoritative workflow, mode, model,
reasoning and persistence settings. It resets operations, requests, clipboard
delivery, tails, edit/recall/chords and geometry. Restore must not replay live
commands, starts or clocks. ACP replay keeps transactional staging/commit/rollback.

## Implementation status and remaining migration

The interaction changes above are implemented and have regression coverage. The
approved architectural plan is **not yet complete**:

- Global overlays now use one controller and an immutable owner/layer snapshot;
  model management has explicit phases. A comprehensive resolved action/capability
  snapshot, consumed/ignored disposition type and fully generated help remain open.
- The binding catalog covers shared navigation/completion and draft recovery, but
  other pane/dialog controls and some hints still use local definitions. These must
  migrate together, rather than merely relocating handwritten help.
- Block allocation and replacement epochs are unified. Six `HistoryEntry` categories,
  positional special-item/message folds, and positional span endpoints still need
  the unified logical timeline and stable-entry/ordered-span migration.
- Worker review/completion evidence shares live/restore reduction and authority
  rules. Native/ACP/hosted adapters are not yet all expressed as one projection-update
  reducer. Transactional ACP replay remains intact.
- Root submission slots protect newer drafts but still correlate through the existing
  session pending/active lifecycle; the proposed complete typed operation/request/
  target transaction token remains to be introduced.
- Block preparation now lives in `layout/prepare`; layout no longer imports painting.
  Remaining protocol-specific preparation and shared header/notice treatment still
  need consolidation. The module ownership map below is the target, not a claim that
  every migration is finished.

## Responsibility migration map

| Owner | Responsibility / former location |
| --- | --- |
| `runtime` | Terminal loop, channels, clipboard service, typed effect execution |
| `workspace` | SessionViews, panes, global overlays, pane/event routing from runtime |
| `input` | Normalization, resolved surface policy, actions, capabilities and hints |
| `app` | Pane façade plus session/workflow/draft/review/edit/conversation/fold/view state |
| `presentation` | Semantic content, provenance, identities, tool/status descriptors |
| `projection` | Native/ACP/persisted adapters and shared presentation updates |
| `layout` | Preparation, row measurement, cache fingerprints and selection geometry |
| `render` | Paint prepared content and chrome; no protocol parsing or input policy |
| `tui-input` | Reusable editor, classifier/completion and question form |
| `tui-widgets` | Text, semantic styling, viewport and painting primitives |

Required flows: input → owner/action → transition → typed effect → runtime;
engine/persistence → adapter → projection → semantic content; immutable UI state
→ measured layout → paint → owner cursor. No layout-to-paint dependency.

## Characterization and validation

Worker acknowledgement/freeze tests, native/ACP hosted revision tests,
FrameLayout/tiny-terminal tests, dialog ownership/navigation tests, projection
identity tests and provider wire fixtures provide characterization evidence. Old assertions for busy locks, completion execution,
implicit Plan Revise, wrapping choices and global cancellation are intentionally
superseded by this contract. Provider fixtures must not be regenerated to conceal
wire changes. Provider-facing result text is checked separately from display-only
diagnostics, including a sentinel that must never enter model history. Live/restored
worker baseline fallback and revocation have parity regressions.

See the implementation report for checks actually executed. Real-terminal enhanced
keyboard/legacy Tab–Ctrl-I behavior, native clipboard access, manual light/dark and
narrow/short-terminal checks, and dedicated performance measurements remain
unperformed; automated render tests are not substitutes for those checks.
