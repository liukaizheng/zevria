# Interactive ACP ensemble workflows

Zevria is an **ACP client** when it launches ensemble workers. The opposite direction, `zevria --acp`, is documented in [acp-agent.md](acp-agent.md). Both use the same SDK but have separate lifecycle and capability policies.

- `/ensemble-plan <prompt>` opens independent, interactive Plan workers. Each publishes a proposal for review. Only after every remaining participating worker's exact revision has been explicitly confirmed does the host publish the final Plan. A run originally selecting exactly one worker publishes that confirmed Markdown unchanged; runs selecting multiple workers use root verification and synthesis.
- `/ensemble-review <prompt>` remains a separate, source-read-only findings workflow: workers return terminal reports and the root independently verifies the evidence. It has no worker composers or worker confirmation gate.

These are typed frontend commands, not model tools. Workers receive the request and workspace, not the root conversation. Skills, nested ensembles and implementation are disabled for Plan workers and root synthesis. Explore panes remain independent investigations under the same shared inspection/scratch contract.

## Worker input cards and activity

Live, frozen and historical ACP panes show sent inputs as quiet literal cards.
Each input has one `● You` header, including image-first and image-only inputs.
Feedback, retry, recovery and ordinary continuation origins appear as muted
header labels when supplied by event metadata, never as prefixes in the body.
Text, indentation, blank lines, code fences and ordered image placeholders stay
literal; selecting/copying a content block does not include the header or badge.

The header shows the latest host phase: `queued`, `dispatched`,
`recovering · attempt N`, `cancelling`, `failed` or `interrupted`. A cancellation
badge records a request, not proof that cancellation finished. After successful
settlement the transient badge disappears. **Settlement is not confirmation**:
only explicit host confirmation receipts confirm a proposal, and later explicit
implementation authorization is still required. Use `d` to inspect the muted
lifecycle history instead of accumulating queued/dispatched lines in the normal
transcript.

Meaningful review outcomes remain visible as toned notices: confirmation,
baseline and freeze receipts; confirmation withdrawal and interruption warnings;
and unsuccessful feedback with actionable context. A successful round without a
surviving fresh complete Markdown proposal explains that republication is required
before confirmation. Failed initial work never claims an older successful proposal
exists. Snapshot and journal notices are correlated so the same outcome is not
reported twice. Tool rows retain their individual titles/details and copy payloads;
status words use semantic colors, and user accents stop before assistant activity.

## Plan review lifecycle

Proposal publication, prompt completion, worker confirmation, and ensemble completion are different boundaries:

1. Startup and one ACP prompt run for the initial input generation.
2. An explicit nonempty inline Markdown publication creates a host revision token containing the worker ID, input generation, artifact revision and exact Markdown digest. Checklist updates and ordinary prose are not publications. Provider plan IDs are metadata, not approval identities.
3. Successful settlement with a fresh publication makes that snapshot reviewable when no prompt, queued feedback, question or permission remains outstanding. Successful prose-only discussion is valid, but requires a fresh complete Markdown publication before confirmation.
4. `/confirm` records a receipt for the displayed host revision. Optional `/baseline` confirms that exact revision and selects it as the single synthesis foundation; it can also mark an already-confirmed worker without replacing its original confirmation receipt. Confirmation remains revocable while any other worker is unconfirmed. `/unconfirm` withdraws confirmation and any baseline mark; accepted feedback does the same.
5. Once at least one worker remains and every remaining worker is confirmed, the final `/confirm`, `/baseline`, **or abandonment** and the complete ordered outcome set are committed together in one `WorkersConfirmed` record. Its persisted `final_confirmation` field retains its name and may contain any of these controls. Abandoned entries retain only sanitized lifecycle metadata; surviving entries retain exact snapshots/receipts. Controls ordered before this seal affect eligibility; later controls are rejected. Failed workers are never implicitly excluded.
6. Participating workers persist their terminal outcomes, then the root publishes `AgentRunFinished` and commits `ReportsReady`. For an originally single-worker Plan run, the host directly publishes the sealed confirmed Markdown. For multi-worker Plan runs, the root inspects facts, reconciles plans and captured choices, asks a substantive unresolved preference question only when needed, and calls `submit_plan` to **publish**, not request another approval.

The root turn stays active throughout review. Model prose, ACP `end_turn`, structured Plan updates, native `submit_plan`, permission answers and Claude `ExitPlanMode` never confirm a worker. Publication does not enter Build or initiate a fresh implementation session. A later explicit version-checked `/implement` or `/implement-fresh` is the implementation authorization. Ordinary non-ensemble Plan still uses Ready approval.

### Direct single-worker publication

The persisted `EnsembleStart` selects the path: workflow Plan and exactly one originally selected worker. A two-worker run reduced to one survivor by abandonment still synthesizes; changing current worker configuration never changes a saved run's path. Single-worker Review is unchanged and still uses root verification.

Direct publication uses only `confirmation.snapshot.plan.markdown` from the durable worker seal. It preserves every Markdown byte, including leading whitespace, CRLF, trailing spaces and the presence or absence of a final newline, in the artifact, Markdown projection, and current/fresh implementation handoff. An H1 or the usual final-plan sections are not required. A nonempty H1 supplies display-title metadata when present; otherwise the title is `Confirmed worker plan`. This never inserts or rewrites a heading.

There is no root model call, root token-count admission, command inspection, reconciliation, question, or `submit_plan` on this path. Revise the proposal through worker feedback **before** confirming: no second model will inspect or rewrite it. `/confirm` and `/baseline` both publish the exact confirmed revision once sealed, with or without a baseline mark. Captured choices, image evidence, summaries and worker lifecycle records remain durable; they do not replace or supplement the plan text.

Empty or oversized final Markdown is not publishable. The final artifact ceiling remains **128 KiB**, alongside per-worker evidence, image serialization and transcript-record limits. An unpublishable proposal reports a review error while feedback remains possible, before sealing; it is never truncated to fit. Ordinary `submit_plan` and multi-worker synthesis still enforce the canonical title and sections. The direct artifact is `Published`, not `Ready`, and does not open an approval modal or authorize implementation.

### Optional synthesis baseline

During live, unsealed Plan review, `/baseline` confirms the displayed eligible proposal and selects it as the one baseline. It is not a provisional mark while planning. Marking another worker moves the selection without withdrawing the previous worker's confirmation. `/unbaseline` removes only the selection and explicitly retains confirmation. Marking an already-confirmed worker preserves its original receipt and separately records the exact marking control, even after resuming on a different root turn.

Both commands are slash-only and idle-only: Normal-mode Ctrl+Y and `c` remain confirm-only; Insert-mode Ctrl+Y is draft redo, never confirmation. Busy, unfinished, stale, abandoned and sealed proposals cannot be marked. An eligible retained disconnected proposal can be marked under the same rules as confirmation. `/unbaseline` checks the displayed confirmed revision and rejects a nonselected worker. Repeating the same accepted request ID returns its original result; a new mark of the already-selected worker reports that it is already the baseline.

The mark's lifetime is coupled to confirmation: accepted feedback/retry, `/unconfirm`, retained-plan removal, payload ineligibility and abandonment clear both. Failed feedback never restores either automatically. Connection changes alone clear neither. The final confirming control seals **immediately**, including a `/baseline` that transfers selection: there is no extra selection window, approval step or implementation authorization. In synthesis runs, no report receives preference precedence without an explicit mark. Direct single-worker publication is independent of baseline selection and always preserves the confirmed Markdown.

### Feedback, failures and freshness

Every live Plan worker has an independent draft that remains editable **while that worker is busy**. Press `i` to enter Insert mode and Esc to return to navigation. Queued/active input, accepted-but-unsettled generations, and feedback/retry submissions awaiting acknowledgement block another work submission, not drafting. Starting or settling work does not force Insert mode to end. Historical, abandoned and sealed panes remain read-only; the root ensemble turn does not determine worker drafting capability.

In an editable Insert composer, Ctrl+Z undoes and Ctrl+Y redoes draft text, caret and image edits—even with an empty draft or an open command menu. Consecutive typing and consecutive Backspace each form a run; navigation or another editing action ends it. Paste, newline, completion, line deletion and attachment are separate steps. Ctrl+C's local draft clear is undoable, but cancellation is not. History is independent per worker and survives temporary navigation and rejected submissions. Acceptance clears only the unchanged submitted draft; editing a replacement draft preserves its text, images and history. Rejected drafts that cannot be restored without overwriting newer text are retained: clear the composer, return to Normal, and use the advertised `r` recovery action. Normal-mode Ctrl+Y and `c` still confirm only an eligible proposal with no unsent draft; `/confirm` remains the explicit Insert-mode command.

Accepted feedback is synced in the root transcript before acknowledgement, advances the input generation immediately, revokes confirmation eligibility, and queues serially on that worker's existing ACP session. Core still durably accepts generations and drains accepted/restored work serially; frontend work-submission admission does not change the engine queue. One worker never runs overlapping prompts. Repeated accepted request IDs do not dispatch duplicate prompts.

A successful feedback round must explicitly republish the complete Markdown proposal before it can be confirmed. Intentional identical republication qualifies; replay on load/resume does not. If a follow-up fails, crashes, is cancelled locally or receives a provider timeout, its partial draft is not promoted. The preceding successful plan is retained with a visible warning that the feedback was **not incorporated**, and may be explicitly confirmed even when disconnected. Old confirmation is never restored automatically. A failure cannot erase an earlier successful-prose republication requirement or bypass later queued feedback. Matching removal invalidates the current proposal rather than resurrecting historical evidence.

Initial failures without an eligible plan remain blocked for retry, explicit abandonment, or whole-ensemble cancellation. A worker failure never authorizes partial synthesis or poisons healthy siblings. Explicit `/retry` queues a new recovery-continuation input generation on the same established session; automatic live structural recovery preserves the current generation and its recovery budgets.

Worker-local cancellation is durably bound to the current active input, or the first queued input if none is active. Restoring a cancellation accepted before actor delivery settles that input without starting a provider or sending its prompt. It cancels one input generation, not an entire restored queue, and does not itself abandon the worker. A delayed cancellation cannot stop a later generation or discard subsequent queued feedback. Cancellation is a request, not a rollback of provider work that already settled; confirmation remains a separate explicit action.

While busy, the pane can compose and explicitly submit eligible `/cancel` and `/abandon` controls with Ctrl+Enter; these do not admit a second feedback prompt. Ctrl+C is focus-local: in the composer it clears a nonempty draft first, then requests eligible worker cancellation when empty. From the transcript or selection it requests worker cancellation without touching the unfocused draft. One Ctrl+C does not guarantee immediate idle: cancellation must settle, and later accepted/restored inputs may still need to drain.

### Permanent worker abandonment

In a live Plan worker pane, press `i`, type `/ab`, and press **Enter** to complete and dispatch `/abandon`, permanently excluding the worker for this run. **There is no additional confirmation dialog**; this existing behavior is unchanged. To complete without dispatching, use **Tab or Ctrl-I**, then explicitly submit the actual `/abandon` draft with Ctrl+Enter. Passive highlighting never executes the control. Enter dispatch requires a valid whole parameterless draft: nonblank suffixes or images remain in the composer without automatic submission. This control remains eligible during startup or queued/running work as well as blocked/disconnected and confirmed-but-unsealed review, subject to the exact coordinator binding and request deduplication.

The UI and coordinator treat abandonment as an urgent control independently of work submission. Abandonment discards queued work, stops only that worker, and excludes **both its plans and all captured answers**, decision IDs and accepted-but-unavailable markers from synthesis and reconciliation. Independent equivalent evidence captured by a survivor remains included. There is no undo within the run, and a sealed synthesis set cannot be edited.

Abandoning the last participating worker cancels the ensemble: no synthesis, `ReportsReady`, canonical publication, approval or implementation follows. `/cancel` only cancels an input; `/unconfirm` only withdraws confirmation; root Ctrl+C still cancels the entire ensemble. Abandonment is not deletion, rollback, or provider failure: transcripts and provider-owned artifacts remain inspectable, and permissions already exercised are not undone.

## Worker-pane controls

Select a root worker row and press Enter to open its pane; Ctrl-O returns to root and Tab/Ctrl-I reopens the latest child where pane navigation is permitted. Completion owns Tab/Ctrl-I, and global capturing dialogs suspend pane shortcuts.

| Action | Control |
| --- | --- |
| Edit a live worker draft, including while busy / return to navigation | `i` / Esc |
| Submit the actual feedback or exact slash command, subject to its own eligibility | Ctrl+Enter |
| Complete and dispatch the highlighted valid parameterless command, subject to eligibility | Enter |
| Complete the highlighted popup command without sending | Tab / Ctrl-I |
| Dismiss active completion | Esc |
| Insert newline outside active completion, in Insert mode | Enter |
| Undo / redo an editable Insert-mode draft | Ctrl+Z / Ctrl+Y (never confirms) |
| Confirm displayed eligible revision | `/confirm`, or Ctrl+Y / `c` in Normal mode |
| Withdraw a revocable confirmation and any baseline mark | `/unconfirm` |
| Confirm and select the exact revision as the single baseline | `/baseline` |
| Clear the baseline mark, keeping confirmation | `/unbaseline` |
| Retry blocked/disconnected work | `/retry` |
| Request cancellation of one eligible worker input, including while busy | `/cancel` |
| Permanently exclude this worker's plan and captured answers | `/abandon` |
| Clear a nonempty focused draft first | Ctrl+C in its composer |
| Cancel eligible worker input without touching an unfocused draft | Ctrl+C in the worker transcript/selection |
| Cancel eligible worker input after the focused draft is empty | Ctrl+C in the worker composer |
| Cancel the entire ensemble | Ctrl+C in the root transcript/selection, or empty root composer |
| Recover a retained rejected draft into an empty composer | `r` in Normal mode |
| Page / jump within the focused viewport | PageUp/PageDown / Home/End |

Popup Enter completes the actual highlighted row, preserves any suffix and images, and dispatches only when the completed whole draft classifies as a parameterless command. Tab and Ctrl-I only complete text. Selecting a row, protocol completion, and text-only completion never dispatch or confirm anything. Enter with an active filter but no matching row does nothing, including for `//x`; outside completion it inserts a newline. Ctrl+Enter classifies the actual draft without accepting a highlighted completion first. Invalid parameterless-command suffixes or images leave the draft intact without automatic execution. Dispatch uses the existing submission path and independent eligibility checks: `/confirm` and `/baseline` require an exact eligible revision, `/unconfirm` requires a confirmation, `/unbaseline` requires the selected confirmed revision, and `/cancel` requires cancellable worker input.

Confirmation shortcuts refuse to discard unsent drafts. Worker Ctrl+C never escalates into whole-ensemble cancellation. Drafts remain until matching durable acceptance and survive rejection; acceptance clears the complete text/image draft only if its target, generation, and content still match the submitted snapshot. Worker completion is scoped: root slash commands, skill invocations, mode switching and transcript replacement are unavailable. Use `//` for literal slash-prefixed feedback and Ctrl+Enter to send it while idle, or discuss command syntax in a sentence.

Panes distinguish queued/dispatched input, awaiting confirmation, prose-only republication requirements, reopenable confirmation, failed-feedback fallback, disconnected review and frozen completion. Root rows display participating confirmed counts and a separate abandoned count; abandoned rows remain selectable. A selected worker has a `baseline` row/pane badge and the root count includes `baseline: <label>`. Historical inspect-only panes use root-authoritative selection, not stale or missing worker-side audit messages. Earlier proposals and discussion remain in worker history. Copy, selection, diagnostics (`d`) and global structured-question routing remain available. Abandoned, sealed, historical, Review and Explore panes are inspect-only; restored unfinished Plan panes gain controls only after binding to the resumed review coordinator.

## Configuration and resource lifetime

`[ensemble]` selects agents in synthesis order and configures:

- `max_concurrent_agents`: fair **active-work** capacity for Plan startup/recovery and prompts. Idle review and confirmed workers release capacity while keeping healthy processes/connections alive. All workers can reach review even when their number exceeds this limit.
- `review_startup_timeout_seconds` and `review_turn_timeout_seconds`: **Review-only** deadlines. Old ambiguous timeout field names are rejected, not aliased.
- `cancel_grace_seconds`: bounded cleanup after an explicit stop, not a Plan review deadline.
- `max_synthesis_bytes_per_agent`: serialized mandatory evidence ceiling. Exact Markdown, confirmation provenance, optional baseline metadata and host-captured choices are not truncated. Eligibility reserves a prospective marking receipt and serialized baseline flag even before selection, so marking remains an alternative to ordinary confirmation. Oversize confirmation is rejected with the measured size and limit.

Plan has no host startup, recovery, prompt or user-wait deadline. A hung attempt can occupy capacity until locally cancelled or abandoned; providers/tools may still return their own timeout errors. Idle process count may exceed active concurrency.

Each `[ensemble.agents.<name>]` supplies an executable, arguments, environment overrides, label, exact `plan_mode`/`review_mode`, workflow config-option maps and `login_hint`. Commands launch directly without a shell and inherit the parent's environment. New configurations select Codex, Claude and Zevria; configured subsets and explicit definitions remain authoritative. The default native Zevria entry is:

```toml
[ensemble.agents.zevria]
label = "Zevria"
command = "zevria"
args = ["--acp", "--ensemble-worker"]
plan_mode = "plan"
review_mode = "review"
env = {}
login_hint = "Configure Zevria's providers in models.jsonc and all five mode assignments with reasoning_level in config.toml before running an ensemble."
```

Native worker model assignments (including reasoning levels) live in `[modes]` in
`config.toml`; provider catalogs and supported `reasoning_levels` live in
`models.jsonc`. Plan uses Plan and Review uses Review. Explore/Builder children
retain their own configured roles and levels, even when profiles are shared with
the root. Native worker resumes require complete version-1 Build/Plan selections
and reject unavailable saved models or reasoning levels without fallback.

The built-in Zevria executable pair resolves to the running binary. Custom paths/arguments remain authoritative. `ZEVRIA_CONFIG` is inherited without credential copying and selects both the ordinary TOML file and its sibling `models.jsonc` provider/model catalog. Native worker transcripts and leases live in `.zevria/ensemble-sessions`, separate from root discovery; worker Markdown convenience projections are disabled.

Codex's default safety mode is `read-only`, with `plan_config_options = { collaboration_mode = "plan" }`; Review uses `agent` and an empty workflow map. `CODEX_PATH=codex` is resolved against the original PATH before npm startup to avoid a transitive CLI shadowing the authenticated executable. Existing definitions are not silently upgraded.

Every new/recovered session re-enforces safe mode and exact configured workflow options before prompting. Missing/unsupported values or later drift stop the affected attempt. `session/new` is bootstrap-only; established conversation recovery prefers advertised resume and otherwise advertised load. Unsupported or ambiguous recovery blocks work instead of silently creating a fresh conversation. Idle restoration does not send synthetic `continue`. Interrupted dispatched feedback is not blindly resent; durable accepted-but-undispatched input is queued once after recovery. Structural transient prompt errors retain bounded same-session continuation; budgets are interaction-scoped for Plan. Review retains its terminal timeout/relaunch policy.

## Provider safety and native handoff

The ACP client exposes no filesystem, terminal or interactive-auth service. Agents must already be installed and authenticated. Native analysis roles use the [shared inspection policy](instructions/inspection-policy.md): **source-read-only with temporary investigative execution**. Task-relevant reads outside the workspace, including `~/.zevria/logs/zevria.log`, absolute/home paths, parent-relative paths and external symlinks are permitted subject to OS permissions. The workspace is the command's default cwd on every invocation, not a read boundary. Reading evidence may send it to model requests and persisted logs.

Task-related downloads, independent source copies, transformations, scripts, builds, tests and package installation are permitted only in newly created, unique, agent-owned private OS-temp scratch directories, with all outputs, dependencies, caches, logs and subprocess temporary files contained. Inspect scripts/configuration first and avoid write-through links or archive traversal. Shared temp roots, other workers' directories, project `tmp` directories and original source trees beneath OS temp are not scratch. Original project files, ordinary home/configuration, global caches/toolchains and remote state remain protected. Cwd or environment settings alone are not isolation; skip operations whose side effects cannot be established sufficiently. These restrictions are **behavioral**, not an OS sandbox; external processes and the command tool have ambient application permissions. Use OS-level isolation for untrusted agents.

Scratch notes and draft files are disposable evidence, not canonical Plan publications or confirmation/implementation authority. Native Zevria workers still use `submit_plan` and structured ACP Plan updates. Clean up only owned scratch before publication when practical; hard termination can leave residue and resume does not guarantee scratch survives. In multi-worker Plan and all Review runs, the parent independently inspects source evidence before synthesis; scratch creation or a build cannot replace that step. Direct single-worker Plan publication deliberately skips this second inspection.

Restart/reopen native workers to adopt compiled module changes. Saved native sessions rebuild the current instruction set under the engine protocol's authority boundary. Worker Plan/Review and synthesis Plan/Review each select their own role module plus the inspection capability, without stacking ordinary mode prose. Skill directive format is v1; sessions containing unsupported skill directive versions require a fresh session. Continuation sends `continue`, not a regenerated launch envelope, and historical prompts are not rewritten. A fresh ensemble is the reliable way to receive the full revised envelope, particularly for third-party ACP agents. Their own policies and sandboxes remain authoritative; Zevria does not promise external-agent support for these operations.

Ordinary Plan permission policy chooses only one-shot responses: `allow_once` for read/search/fetch/execute, one-shot rejection for unsafe/unknown kinds. Execute can still mutate if an agent ignores its instructions. Review deliberately selects `allow_once` for inspection choices and never `allow_always`; missing one-shot grants cancel the worker.

Claude support is explicitly enabled by `plan_handoff_transport = "claude_code_exit_plan_mode"`, not inferred from its name. A temporary-looking path never enables external mutation permissions; scratch permission is conditional on the agent's own active policy, separate from this handoff. Only metadata-validated Write/Edit/MultiEdit operations on direct regular `.md` children of the configured Claude plans directory or workspace `.claude/plans` can receive a one-shot grant. Every available path must agree; symlinks, nested paths and unrelated mutations are rejected. Directory creation occurs only immediately before an eligible grant. Artifacts are provider-owned, retained during cleanup, and not automatically ignored in source control.

Artifact permissions queue FIFO in receive order. At most one granted mutation owns admission until matching validated terminal evidence; cancellation, responder completion and process exit do not release ownership. Terminal replay cannot release a newer grant. The artifact ledger survives feedback generations and connection attempts. A missing ACP permission record does **not** establish that a native Write did not execute: every observed path-bearing mutation requires a validated terminal result, granted or not. Pathless abandoned preparations remain diagnostic rather than fallback candidates.

A native proposal is generation-local. Aborted/superseded exits become stale identities, not blockers for later feedback; expected late terminal output cannot resurrect them. New generations receive fresh completion signals while retaining unresolved mutation evidence. `ExitPlanMode` provider status is separate from proposal availability: a host-rejected exit may legitimately report `failed`, including while file capture is still settling. That output is neither Markdown proof nor a new handoff failure.

The host validates the exit identity, payload and exact offered one-shot rejection, fences new artifact admissions, and sends **`reject_once` before waiting for artifacts**. An independent settlement task durably records the rejection, continues accepting validated terminal notifications, resolves the source, and persists a `native_plan_captured` event containing the frozen plan and typed host provenance (generation, exit tool, source, and file tool/path/SHA-256 digest where applicable). Only durable capture signals completion and starts bounded prompt-stop handling. Neither `end_turn` nor an outstanding capture is treated as a settled review round. Permission handlers and the previous prompt must finish or the connection must close before another prompt starts.

Source selection is deliberately narrow:

- Valid explicit native Markdown is authoritative, retaining native normalization and consistency checks. Malformed or contradictory explicit data is an error, never a reason to substitute a file.
- Only an absent payload permits file fallback. `planFilePath`, if supplied, must identify this worker's observed artifact in the current generation. Without a hint there must be exactly one distinct eligible path; sequential Write/Edit/MultiEdit operations on one path count as one candidate.
- A candidate's latest observed mutation must have completed successfully. Older-generation artifacts, unobserved paths, failed latest mutations, pathless preparations, file existence and directory recency are not evidence. The host never scans `.claude/plans` to discover a source and never treats assistant prose as native proof.
- Only `Write.rawInput.content`, including its permission-request representation, supplies authoritative whole-file content evidence. Conflicting Write inputs are rejected. ACP display diffs do not establish whole-file scope, even with `oldText: null`: Write previews may be partial, Edit results may expand replacement fragments with context, and MultiEdit may contain separate hunks. Every diff path is still validated and its display payload retained in the transcript. Neither diff reconstruction nor `rawOutput`/`toolResponse` supplies approval evidence.
- After the latest Edit/MultiEdit completes successfully, the entire edited document comes from the safe filesystem snapshot, not an older Write payload or a diff fragment. At read time the host revalidates the allowed directory and direct `.md` child, walks no-follow directory handles, rejects nonregular or multiply linked targets, checks identity/change evidence, and reads at most **128 KiB**. Content must be nonempty UTF-8 Markdown and agree with authoritative whole-file evidence when available for the latest mutation. Detected replacements/changes fail closed. Metadata checks reduce races but do not make the external filesystem transactional. Platforms without safe contained reads report fallback unavailable; explicit payload capture remains supported.

The captured bytes and provenance are journaled together; replay uses that snapshot even if the provider file later changes or disappears. Fallback is host-side and sends no repair envelope or extra model prompt. Launch prompts, stable tool schemas, provider configuration and their cacheable prefix are unchanged. Capturing remains an **unconfirmed proposal**: it never exits Plan mode, approves implementation, or bypasses version-checked confirmation/implementation authority.

Native settlement uses `cancel_grace_seconds` as a bounded window, followed by normal bounded stop/close handling. This is not an overall Plan deadline. Missing/ambiguous sources, unsafe files, pending terminal evidence, stale handoffs, cancellation/disconnection, timeout and permission/capture persistence failures produce distinct diagnostics. Failed rounds cannot make their candidate eligible; prior successful proposals and user decisions retain the existing review rules. Unresolved or ambiguous mutation evidence is never cleared merely to make a retry succeed.

Ambiguous Claude artifact ownership after a whole-process restart is currently blocked conservatively; an eligible retained proposal can still be confirmed. Full durable artifact-scheduler reconstruction is not available. Fake adapters establish host behavior, not production-version compatibility: exercise exact Codex/Claude adapters in a disposable workspace before relying on a new combination.

## Structured questions and synthesis evidence

Workers may ask non-secret ACP form questions for consequential unresolved preferences, not factual questions answerable by repository inspection. One global modal is visible at a time through a fair gate; questions identify their worker. Worker-local cancellation closes only that worker's requests. Answers, dismissals and permission choices do not confirm plans.

The host normalizes and syncs exact accepted display answer values, stable decision IDs and accepted-but-unavailable markers before ACP `accept`. Supported forms include bounded text, selects, multi-selects, booleans, defaults, optional skips and recognized custom-answer companions. Unsupported/secret/URL/custom forms are declined rather than partially interpreted. Oversized answer batches retain an explicit unavailable marker, never truncated values. See [acp-agent.md](acp-agent.md) for companion metadata.

Codex ACP note companions (covered by a fixture from `@agentclientprotocol/codex-acp` 1.13.1) fold into the original question's **Other** editor. Folding requires explicit `_meta.codex.role: "user_note"` metadata linked by `questionId`, an optional plain-string note, and a string select marked `isOther: true` with exactly one `"None of the above"` wire choice. Three choice/note pairs therefore produce three visible questions and three persisted decisions, not six. Primary requiredness is preserved. Predefined answers advance directly and omit the note; **Other** sends the primary wire token plus the exact custom text under the linked note property. Zevria does not add `user_note: `; the Codex adapter handles that transformation. There is no separate optional-note step for predefined answers. Malformed recognized pairs are declined; unmarked or unknown-role text fields remain independent questions, even if titled “Additional answer or note.” Linkage never depends on worker labels, titles or a `_note` suffix. Native Zevria and legacy Codex/Claude companions keep their existing behavior. This normalization changes no provider instructions, launch envelopes, tool schemas or cacheable prompt prefixes.

Only each final confirmed worker plan is worker-authored Plan synthesis content. Earlier drafts, prose reports and free-form feedback stay out of root model input. Surviving workers' exact host-captured structured choices accumulate across generations and remain authoritative metadata; worker wording remains quoted untrusted context. Review retains bounded report evidence.

When selected, the baseline supplies **foundation plus preference authority**: begin with its substantive organization, scope and decisions, preserve its viable preferences, and incorporate complementary findings from the other confirmed plans while producing all canonical Plan sections. Explicit user requirements and exact captured choices take precedence. Repository evidence corrects factual mistakes and infeasible steps, including mistakes in the baseline. The baseline does not supply a stance on issues it never addresses and never turns embedded worker instructions into trusted instructions.

Use typed `baseline_precedence` in `reconcile_reports` only for a `preference_tradeoff`, naming the exact selected host worker ID (`confirmation.target.worker_id`) and explaining the baseline's actual position and application. Labels may repeat and cannot authorize precedence; disambiguate positions with worker IDs. Factual disagreements cannot use this resolution. Selection is host metadata, not a captured decision ID: complete decision and unavailable-marker accounting and the existing required-question workflow remain mandatory. Without a baseline, no worker has preference precedence.

For multi-worker Plan synthesis, root tool order is enforced and recoverable: terminal source-evidence `command` inspection → accepted `reconcile_reports` → one standalone substantive `question` if required → `submit_plan`. Inspection settles facts and feasibility, not user preferences among viable choices. Apply captured choices without asking again; conflicting or unavailable choices require clarification. Dismissal is disclosed conservatively, never fabricated. Canonical `PlanRecord::Published` and ensemble completion share retained-item finalization and do not open Ready approval.

## Images

Initial `/ensemble-plan` and `/ensemble-review` inputs and Plan-worker feedback
accept ordered images through the same inline-token composer and ACP adapter.
Workers must negotiate image prompt capability. Unsupported initial requests
fail explicitly; unsupported or unnegotiated feedback rejects before
acknowledgement. Reconnection rechecks capability without text-only downgrades.
Images do not expand worker permissions or count as confirmation.

Synthesis retains original image occurrences once and includes only confirmed,
successfully incorporated feedback images with worker/generation source labels,
outside the quoted report JSON. Abandoned and unincorporated feedback is excluded.
Potential image evidence is checked against shared count/byte and root-record
budgets before feedback/retry acceptance and confirmation. Synthesis runs also
check provider-specific root context budgets; direct single-worker Plan runs skip
root admission and token counting, not image or record safety limits. Incomplete
workers contribute known proof to this projection; the complete request is
checked again before sealing, preferring exact endpoint token counts only when
root synthesis will run. Failed
image inputs retain their bytes across explicit retry, even if a session was
allocated before dispatch failed. `ReportsReady` persists the complete
multimodal message. See [image input](image-input.md) for limits and sensitive
history retention.

## Durability and format compatibility

Root transcript review v1 is authoritative for accepted controls, generation order, eligibility, withdrawals, the single baseline selection, correlated results and the atomic all-worker seal. A baseline transfer is one accepted target marking record; displacement is derived by the shared run reducer, never a second uncorrelated root clearing record. These control records are not model messages. Worker JSONL v1 under `.zevria/agent-runs/<root-session>/<ensemble-run>/<agent-run>.jsonl` is authoritative for ACP evidence and mirrors host transitions with stable identities. Acceptance, publication, settlement, confirmation and sealing are sync boundaries; token previews remain lossy but review snapshots and results are reliable.

Unsealed review reconstructs host state in root record order before restoring controls. Root transfer/clearing overrides stale or absent worker mirrors, including for historical badges. Baseline marking and clearing in worker JSONL are host audit mirrors, not worker-origin execution evidence. Finalization emits the original confirmation, exact stored marking receipt if selected, then sealing and the terminal outcome. Root abandonment is authoritative even if its worker audit mirror or outcome is missing. Restoration never starts an abandoned provider, redispatches its queued feedback, promotes its late execution suffix, or merges its archived answers into synthesis. If every worker was abandoned before a crash, recovery completes cancellation rather than synthesizing. Existing log identities and complete records remain strictly preflighted; no sidecar is created solely to repair abandonment. Confirmed workers remain reopenable. A sealed set without `ReportsReady` restores exact frozen outcomes without relaunching providers or asking again. For multi-worker runs, valid `ReportsReady` resumes durable verification/reconciliation/submission stages. For originally single-worker Plan runs, it directly publishes the frozen confirmed Markdown without reconstructing synthesis stages. An already published artifact is retained and only missing completion state is appended: no duplicate publication, revision increment, synthesis or approval modal. Direct publication carries typed run/worker/host-revision provenance; replay rejects foreign, abandoned, incomplete or unconfirmed sources, mismatched revisions or Markdown, duplicate publication, and invalid ordering. Recovery never infers consent or repairs corrupt complete records.

Root-review and worker-journal v1 carry complete validated image prompts, negotiated image capability, and successfully incorporated feedback image evidence. Missing optional baseline fields still mean exactly “no baseline”; `baseline: false` is never emitted in unmarked synthesis. Older owned formats are deliberately incompatible; do not silently migrate or trim complete unsupported records. Both root and worker records enforce a 64 MiB read/write ceiling, including control/input mirrors and base64 expansion.

Unsupported worker formats and proof-only ensemble histories are deliberately incompatible. Readers reject complete corrupt/unsupported records without changing bytes or inventing confirmation. Only an incomplete tail of the current format is recoverable crash debris, including an interrupted `response_display` append. A complete invalid payload is still an error when only its outer record delimiter is missing. Referenced worker identities and formats are preflighted before writable restoration.

Response-display v1 metadata contains attempt v1 terminal evidence with canonical unsigned decimal JSON object keys (for example, `"terminal":{"1":"completed"}`). This evidence round-trips through the tagged worker v1 record without a format migration. Invalid, noncanonical, overflowing, and duplicate indexes are rejected, not merged or discarded. Display metadata remains presentation-only: it changes neither review authority, reports and decisions, nor model input or the provider's cacheable prompt prefix.

Worker-history errors identify the file and line and distinguish decoding, version, and journal-order/identity validation failures. Explanations are bounded and do not include raw prompt or protocol payloads. A blocked preflight is not a runtime interruption that restarting the unchanged binary will repair: preserve the existing root and worker logs and resolve the reported decoding, validation, or access issue before resuming. Do not delete records, truncate complete evidence, start replacement work merely to hide the error, or manually resend dispatched feedback. Failed preflight starts no actors and commits no recovery transitions or terminal ensemble failure. After successful preflight, dispatched-but-unsettled input is interrupted rather than resent; accepted-but-undispatched input remains queued exactly once. Resuming with a corrected binary does not fabricate settlement, proposal eligibility, or approval: explicit retry and confirmation rules still apply.

Logs can contain source material, exact non-secret answers and sensitive protocol diagnostics. Create-only `.gitignore` guards reduce accidental commits but are not confidentiality or retention controls. Raw diagnostics are bounded (256 KiB per line, 8 MiB per logical run); required normalized evidence is not silently dropped. Stop all root/native workers before upgrading old lease-coordinator binaries or using destructive `zevria clean`; never manually remove active lease files.
