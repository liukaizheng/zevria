# Local skills

A skill packages reusable Markdown instructions—a commit convention, review
checklist, or release procedure. The engine owns one application workflow:

**Discover → resolve a name → prepare an application → commit it → project instructions.**

Instructions are local configuration, not sandboxed plugins. Reading a resource
is a separate, contained operation: it never activates a skill or grants tools.

## Four core concepts

| Concept | Responsibility |
|---|---|
| `SkillSnapshot` / `SkillDefinition` | Validated content and its full digest; a definition adds optional runtime locations |
| `SkillCatalog` | Immutable installed definitions, all candidates, configuration, fixed roots and diagnostics |
| `ActiveSkills` / `SkillApplication` | Historical per-session snapshots and the reducer that activates or reapplies them |
| `SkillContext` | Captured catalog, historical pins and effective permission, sharing one name resolver |

A snapshot contains a validated name, normalized complete body, metadata with
one authoritative description, optional provenance, and one `SkillDigest`.
The digest covers all snapshot content, including metadata and provenance, with
a deterministic domain-separated encoding. Deserialization validates every
field and recomputes the digest; unknown fields and tampering are errors.
Runtime filesystem paths are not persisted as arbitrary read authority.

`ActiveSkills` is a name-sorted **historical ledger**, not the currently enabled
instruction set. First activation captures the full snapshot. Reload, file
changes or removal, disable/re-enable and resume never silently replace it.

## Guidance is separate from skills

[Automatic AGENTS.md guidance](guidance.md) reads `~/.zevria/AGENTS.md` and
`<startup workspace>/AGENTS.md` at opening/resume. It is supplemental session
guidance, not a skill package or activation. Reloading skills does not refresh
AGENTS.md. Explore inherits captured guidance while remaining skill-free;
native workers independently capture guidance with skills disabled. Guidance's
per-file cap is 128 KiB; skill main sources remain capped at 64 KiB.

## Fixed discovery locations

Zevria searches **only**:

- global: `~/.zevria/skills`
- project: `${startup_workspace}/.zevria/skills`

The startup workspace is not a Git ancestor or a later shell working directory.
`ZEVRIA_CONFIG` selects the ordinary TOML configuration and its sibling
`models.jsonc` for session routing; its directory cannot move skill discovery.
Offline skill commands need neither providers nor the model file, but reject
stale `[providers]`/`[modes]` TOML tables with a pointer to `models.jsonc`. Missing directories are empty. An unavailable runtime home produces a bounded
diagnostic and project-only discovery. Native Windows falls back from a valid
absolute HOME to the user profile; WSL has its own Linux home. Project skills stay
shared in `.zevria/skills`, never under the runtime-specific `.zevria/windows` state root. Roots cannot be configured: ancestor,
extra/shared, `.agents`, `.codex` and `.claude` roots are unsupported.

Both layouts work:

```text
~/.zevria/skills/commit.md
workspace/.zevria/skills/team/review/SKILL.md
workspace/.zevria/skills/team/review/agents/openai.yaml
workspace/.zevria/skills/team/review/agents/zevria.yaml
workspace/.zevria/skills/team/review/references/checklist.md
workspace/.zevria/skills/team/review/scripts/check.sh
workspace/.zevria/skills/team/review/assets/icon.svg
```

Flat `.md` files are recognized only as direct children of a fixed root.
Packages may be nested within the depth limit. Discovery stops at `SKILL.md`,
even if it is malformed; package references are not scanned as additional
skills. Hidden descendant directories are skipped.

### Selection and aliases

One validated name selects at most one definition:

1. A valid project definition overrides a valid global definition.
2. Canonical manifest aliases are deduplicated before ambiguity checks. Aliased
   native roots retain project provenance when fully resolved.
3. Distinct valid same-name definitions within one scope are ambiguous; none
   wins there. A valid global definition may still be selected.
4. Malformed main documents do not shadow a valid lower definition.
5. Disabling a name never promotes a shadowed alternative.

Directory aliases must stay within the canonical fixed root. Outside-root
candidates are rejected with diagnostics. Canonical targets are identities,
not additional search roots; a canonical source supplies the derived name.
An incomplete scope cannot certify an unambiguous winner; only a fully
resolved lower scope may supply a fallback.

### Discovery bounds

| Resource | Limit |
|---|---:|
| Fixed roots | 2 |
| Directory depth within a root | 6 |
| Directories per root | 2,000 |
| Entries per root | 20,000 |
| Retained candidates | 2,048 |
| Complete main source | 64 KiB |
| Each auxiliary metadata source | 32 KiB |
| Aggregate main/metadata source bytes | 32 MiB |
| Retained diagnostics | 256, with omission counts |

Scopes and paths are processed deterministically. Blocking discovery runs off
the async session executor. Programmatic catalog construction is also fallible:
duplicate names and invalid configuration are rejected.

## Markdown and YAML metadata

A flat document:

```markdown
---
description: Create a well-formed Git commit
---

Check `git status` and `git diff` first, then ...
```

A package document:

```markdown
---
name: review
description: >
  Review changes for correctness,
  compatibility, and test coverage.
metadata:
  short-description: Review a change
policy:
  allow_implicit_invocation: false
---

Read references/checklist.md with skill_read when more detail is needed.
```

Description and complete body are required and nonempty. Names contain 1–64
lowercase ASCII letters, digits, `-`, or `_`. Flat filename/name agreement is
required. Packages honor a valid explicit name; otherwise the complete
canonical package directory basename is used, not its file stem.

The private YAML adapter supports BOM, CRLF, block/folded and quoted strings,
comments, and nested mappings. Recognized duplicate fields and invalid main
field types are errors. YAML must be valid as written: quote descriptions
containing a colon followed by a space. Oversize sources and bodies are
rejected, never truncated. The deserializer-only `serde-saphyr` dependency has
include/filesystem/property/robotics features disabled. Source bytes and decoder
budgets are bounded; unsupported tags, aliases and merge keys are rejected.

Optional `agents/openai.yaml` and `agents/zevria.yaml` recognize:

```yaml
interface:
  display_name: Change review
  short_description: Check correctness and compatibility
  default_prompt: Review the proposed change
  brand_color: '#4285F4'
  icon_small: assets/icon.svg
policy:
  allow_implicit_invocation: false
dependencies:
  tools:
    - type: mcp
      value: example-service
```

Explicit fields merge in order: frontmatter, OpenAI sidecar, Zevria sidecar.
Missing fields do not erase lower values; explicit `false` is preserved.
Invalid cosmetic fields degrade individually. An unreadable or malformed
policy-bearing sidecar or invalid policy makes fresh activation explicit-only,
even if another sidecar contains an affirmative value.

Interface text, assets, default prompts and dependencies are inert metadata.
Default prompts are never automatically submitted or appended to arguments.
Nothing installs tools, sets environment variables, changes roots, runs scripts,
resolves dependencies or grants permissions.

## Configuration and reload

Defaults:

```toml
[skills]
enabled = true

[[skills.rules]]
name = "release"
enabled = false
```

Rules are ordered and name-based; the last matching rule wins, with at most
4,096 rules. Unknown settings, including prompt budgets and root overrides,
are rejected by both full and skill-only configuration parsing.

Name disabling affects fresh activation, reapplication and materialization,
including historical pins and both scopes. Historical records remain intact;
re-enabling restores the same snapshot. Package policy cannot override user
disabling, workflow restrictions or the actual activation tool capability.
Build's root skill capability is unchanged by request-local orchestration. Skills,
including a skill named `orchestrate`, cannot activate it. Direct skill invocations
are Standard requests, not continuations of a prior orchestration obligation.
Plan retains its captured
`session.plan.allow_skills` restriction. Children and native ensemble workers
remain skill-free regardless of their nominal mode.

Configuration and catalog loading occur at startup and explicit idle reload.
The two root locations and mode permissions are captured once per session.
Reload does not recapture HOME, the workspace, provider routing or Plan
permission. Other running sessions retain their view until their own reload.
The management revision covers definitions/candidates, configuration, fixed
paths and captured canonical roots, and diagnostics—not just selected bodies.

Enable/disable requires an existing regular config file. All clients share a
formatting-preserving TOML writer, adjacent `.skills.lock` cooperating lock,
source-content check and same-directory atomic replacement. Comments, unrelated
settings/secrets, rule order and file permissions are preserved. The final
matching rule is updated, or a new rule appended. External skill-setting changes
require reload; unrelated external changes are preserved. Preparation, lock or
content-conflict failure installs nothing. Noncooperating editors should avoid
racing the final check-and-rename window.

## Complete visibility and name resolution

The model receives the **complete eligible catalog** in top-level `instructions`:
sorted entries containing only `name` and `description`, independent of pins.
Configuration disablement renders a selection-unavailable sentence; `[]` means
no currently eligible skills. Descriptions are
escaped matching metadata, shortened safely to 1,024 UTF-8 bytes. There is no
entry-count truncation, rendered-size cap, pagination fallback, source selector,
revision or digest in this prompt projection. Detailed diagnostics stay on
management/startup surfaces. Selection and effective-body lifecycle rules live in
[skill-selection.md](instructions/skill-selection.md), not in workflow docs or directive banners.
The `skill` description owns call mechanics; the workflow JSON's `skills` field
and tool allow-list carry the enforced capability.

Admission and explicit completions share one resolver:

1. Check workflow/tool capability and name enablement.
2. Prefer an existing historical pin.
3. Otherwise resolve the selected installed definition.
4. For a fresh model-origin application, enforce explicit-only package policy.

Consequently, enabled historical pins keep their original descriptions in explicit
completions after source change/removal. The prompt catalog resolves installed
names with model origin and **empty pins**, so explicit-only and removed skills
remain absent even after activation. Their enabled pins can still be reapplied.
Ambiguous, shadowed and invalid candidates do not independently become invocable
completions. Completion names are unique and use pinned metadata where present.

The model is instructed to apply clearly matching skills before normal work,
even for ordinary requests such as `commit the changes`, and to choose the
minimal applicable set from actual user intent. Visibility alone is not an
activation. TUI and ACP do not semantically rewrite ordinary text into skills.
Selection is model-led, not a keyword router. Actual workflow and registered
activation capability gate disclosure; skill-free children receive no catalog.
An enabled empty catalog means no currently eligible skills, not no files.

## The five-stage application workflow

1. **Discover:** install an immutable catalog. Reload replaces it, never pins.
2. **Resolve a name:** validate a user/model-selected name against the captured
   context. Admission carries no revision or source-selection binding.
3. **Prepare:** the engine prepares `Activate(snapshot)` or `Reapply(name)`
   with the shared reducer, policy checks and full prospective instruction-state
   capacity check. No speculative pin is installed yet.
4. **Commit:** persist the full application in its direct invocation or tool-result
   record together with resulting directives at their exact ordered positions.
   Rejected direct admission leaves
   history unchanged. Completed tool work that cannot persist retains a coherent
   in-memory bundle and blocks generation until recovery; it is not silently undone.
5. **Project instructions:** emit only changed effective bodies or revocations as
   hidden ordered skill directives. Resume reloads their recorded positions and
   effective state. Unchanged reapplication or reconciliation repeats no body and
   never changes the catalog or rendered `instructions`.

| Event | Historical ledger | Effective instructions |
|---|---|---|
| First use | Capture complete snapshot once | Append body only; fixed instructions unchanged |
| Reapply | Require the existing pin by name | No repeated unchanged body |
| Reload or file removal | Keep all snapshots | Refresh installed catalog in instructions; retain enabled pinned bodies |
| Disable or enter a skill-disabled mode | Keep pins | Revoke effective bodies and deny application |
| Resume | Restore pins and ordered directives | Reuse historical positions; reconcile only changed policy |
| Re-enable or return to a permitted mode | Reuse recorded snapshots | Restore those pins, not current disk content |

Configuration writes and transcript persistence remain separate boundaries;
a later persistence failure does not roll back a successful config write.

### Typed `$name [arguments]` and edits

`$review inspect the patch` is explicit TUI selection. A leading space forces
literal text. ACP uses a name-based extension; generic `session/prompt` text
never treats `$name` as typed skill syntax.

The model-facing application is bodyless:

```text
Apply the active skill "review" to this request:

inspect the patch
```

Display/recall use the compact `$review` form. Both are derived from the name
and ordered text/image arguments, including image-only input. The embedded
application adds no extra row or prompt ordinal.

**Editing the same skill's first invocation preserves its recorded complete
snapshot, including metadata, even if the installed file changed or disappeared.**
An edit to a different name resolves normally against the retained prefix and
current catalog. Later same-name invocations reuse the prefix's pin. Edits anchor
at the actual prompt, retaining preceding standalone directives and a preceding
turn's skill body; reconciliation appends necessary replacements. Failed
replacement admission leaves the original tail intact.

### Model tools

- `skill({ skill, args? })` activates or reapplies a named skill.
- `skill_read({ skill, resource, cursor? })` reads live auxiliary package text.

`skill` registration supplies only its schema/description. Raw tool-server
dispatch fails: only the engine can admit activation. The engine strictly parses
arguments, prepares the application and produces a bodyless acknowledgement.
`skill_read` remains an ordinary context-bound tool. Skill-capable root modes
keep these tools even when discovery is empty; runtime resolution enforces
name enablement and package policy. Children/workers register neither tool.

A response containing `skill` must contain **only `skill` calls**. A mixed batch
is rejected entirely before any command, mutation, subtask, Plan tool or activation
executes. Skill calls run in assistant order against a prospective ledger.
Repeated names activate once, then reapply. Capacity is cumulative; a later
failed call does not erase earlier accepted applications. Wait for the resulting
instructions before task execution/completion. Reapply for new matching requests;
a typed invocation already supplies that application. Failed activation must not
be claimed as success. Acknowledgement prose has no persistence authority.

## Context, persistence and compaction

The engine renders one complete `InstructionSet` as the Responses `instructions`
value: engine protocol, application/file guidance, the workflow JSON declaration
and role module, applicable capability modules, and eligible catalog.
It is not a transcript record. Skill bodies and revocations alone persist as typed,
hidden ordered input, rendered with `Skill directive:` markers and no repeated protocol banner.
An endpoint may opt into [user-role compatibility](responses-compatible.md) for
these skill directives only; the fixed instruction set retains top-level authority.

Direct use orders reconciliation directives, the invocation, then its new body.
A tool-result batch precedes its resulting directives; the full batch commits
atomically, with every live item written as a JSONL record. Full bodies, digests,
metadata and provenance remain in historical `SkillInvocation` activation pins
and `ToolResults.skill_applications`. Directives have no prompt-ownership wrapper. They never become frontend rows, prompt previews or retained user
candidates. Revocation does not securely delete bodies already saved or sent.

The dedicated invocation envelope is:

```json
{"zevria_skill_invocation":{"name":"demo","arguments":[],"application":{}}}
```

This illustrates only the envelope; arguments and application use their validated
serializers. There is no invocation UUID, version field or duplicated message
mirror. Tool-result records carry `zevria_skill_applications`, each with a local
`call_id` and application. Ordinary provider correlation stays in the message
and tool metadata; it is not duplicated in the skill sidecar.

Skill directives use a dedicated, single-key envelope:

```json
{"zevria_skill_directive":{"version":1,"payload":{"kind":"skill_body","name":"demo","digest":"…","body":"Exact pinned body"}}}
{"zevria_skill_directive":{"version":1,"payload":{"kind":"skill_revocation","name":"demo","reason":"disabled by the current skill or workflow policy"}}}
```

The digest is illustrative; a body directive must match its full activation pin.
Snapshot identities use `zevria.skill.snapshot.v1`; catalog revisions use
`zevria.skill.catalog.v1` and remain computed hashes, not literal version numbers.
The canonical inputs and field order are unchanged. Old snapshot digests are
rejected, not rehashed on load. `zevria.skill.source-binding.v1` remains unchanged.
Only `version` and `payload` persist. Loading re-renders `text` deterministically,
without storing a duplicate banner. Unsupported directive versions and extra
fields reject the record.

Replay correlates actual result blocks, metadata and applications within each
batch. Every successful skill result requires exactly one application; failed,
denied or cancelled results require none. Duplicate/missing/extra relevant IDs
are errors. Applications reduce in actual result order. Direct name/application
agreement, snapshot integrity, duplicate activation and unknown reapplication
are validated independently of acknowledgement wording.

Instruction directive format **v1** uses `SkillBody { name, digest, body }` and
`SkillRevocation { name, reason }`. Body directives contain only enable/end markers
and the pinned body; revocations contain only a revoke marker and reason. The
selection module owns lifecycle rules. Saved sessions with unsupported skill directive
versions are rejected with “unsupported directive version; start a fresh session”.
Standalone directive validation checks canonical structure/text; transcript
replay checks all three fields against the authoritative pin. A body alone
cannot reconstruct a full-snapshot digest. Checkpoints are **v1** and contain
neither instruction snapshots nor redundant historical pin identities. Live and
resumed projection fold the persisted directives at each checkpoint boundary in
reducer order. Full transcript replay reconstructs all historical pins, including
disabled ones, and validates ordered directives against them.
Provider replay and normalized worker-log headers each use **v1**.

The rendered instruction set counts as fixed input overhead, alongside tools.
Ordered skill directives count as conversation input, including superseded history
until compaction. An irreducible instruction state that does not fit is rejected
before prompt/Plan/pin commitment. Compaction does not truncate catalogs or bodies.
Management remains available to disable names or reload. Catalog mutations
invalidate request measurements and change instructions, without changing the
prompt-cache key; these changes require safe full provider replay.

Local summaries, remote compaction and model conversion carry **no ordered
directives** and advertise no tools. Their maintenance instruction set contains
captured application/file guidance and a no-tool summary policy, without a skill
catalog. Summary prose cannot recreate instruction authority. Resume renders
current application/file/workflow guidance and the installed catalog, restores
ordered directives, and reconciles effective bodies against saved pins and current
policy rather than changed package files. Disabled pins stay inactive after
reconciliation and skill-free workers acquire no catalog. Maintenance works even
before the first resumed turn, without enabling skills.

Within one workflow, instructions remain byte-identical across turns, activation,
revocation, tool continuations, retries and reconnects. Mode/synthesis switches,
changed files on resume and skill-management mutations can change them. A
Standard → orchestrated → Standard Build sequence does not change fixed
instructions, catalog, or tools: typed request boundaries/corrections append to
ordered input independently of skill pins and lifecycle directives. Deliberate
resets for edits/compaction remain. Resume reproduces
historical directive positions, preserving the pre-shutdown input prefix when
instructions, tools and policy are unchanged. It does not reproduce obsolete
guidance, and server cache retention/recomputation remains outside this guarantee.
Only current-format histories are supported. A current-format crash can interrupt
a trailing directive append; after valid lifecycle recovery, the next prompt
reconciles missing directives from the retained pins and current policy.

Application/file/workflow/catalog instructions are not persisted as typed engine
records. Skill directives are a deliberate exception: their bodies already
persist verbatim in full activation pins, and revocation reasons are also recorded.
Separate typed request directives persist request behavior and bounded correction
state, never skill activation or body authority. Compaction reprojects the active
request contract independently of effective skill bodies.
There is no separate instruction-state file; directive records live in the JSONL.
Conversation or summaries may quote guidance, old files are not erased, and
provider retention is separate.

Retired `zevria_instruction_prefix`/`zevria_directive` records, unsupported checkpoint
versions, v1 checkpoints carrying `instruction_snapshot`, and other unsupported or malformed
reserved records reject the entire open, including
read-only opens, before writable restoration. There is **no compatibility
reader, migration, adapter or upgrade backup**. Original bytes and existing
backups remain untouched. Start fresh or use a matching older binary. Automatic
crash recovery is limited to an incomplete final JSON line with no newline and
a valid surviving lifecycle. Incomplete trailing `zevria_skill_directive` records
are recoverable: losing a directive does not change pin authority, and the next
prompt derives it from pins and current policy. A completed invalid inner envelope
is never repaired away, even if its outer record is truncated. Recognized retired
records and pin-bearing lifecycle tails are not disposable debris. Ordinary
message strings/tool arguments are data, not searched for lifecycle markers.

## Live resources and containment

`skill_read` requires an active, enabled, package-backed pin. Flat and
programmatic body-only definitions gain no resource authority. Provenance binds
the package to a fixed scope and relative manifest with a private engine-generated
source binding. It is not a public selector. The reader verifies that binding
against the current fixed root, including its opened-handle identity; a root
retarget or replacement at the same path cannot redirect an old pin to another
tree or same-name package.

The current main document's normalized **name and body are compared directly**
to the pin. A metadata-only edit is allowed even though it changes the full
snapshot digest. Missing or changed main instructions preserve the pinned body
but make resources unavailable. There is no fallback to another scope.

On Unix, one opened directory handle anchors component-by-component `openat`
reads with no-follow checks. Windows uses component-relative `NtCreateFile` opens
and handle-derived identity/link metadata. Its initial protected-input support is
local NTFS/ReFS drive paths (including verbatim disk form); reparse traversal,
network/mapped-network roots, streams and devices are rejected even during source
discovery. Unix contained discovery-alias behavior is unchanged. Symlink components, absolute/platform-prefixed
paths, `.`/`..`, controls, directories, devices and FIFOs are rejected. Main
`SKILL.md` reads, including hard-link aliases, are rejected. Unsupported
platforms fail closed without an ambient-read fallback.

Resources must be UTF-8 and at most 1 MiB; JSON text pages are at most 32 KiB.
Resource cursors bind the snapshot digest, relative resource, live file identity,
content digest and offset. Changed content requires restarting pagination.
New complete reads see live auxiliary edits while the snapshot remains pinned.
Resource results are ordinary compactable data, not archived package content,
script execution authority or a reproducible execution environment.

## TUI and offline CLI management

`/skills` opens a metadata-only manager with fixed locations, revision, counts,
all candidate statuses, diagnostics and historical pins. There is no root editor
or arbitrary-path activation.

- Up/Down or `j`/`k`: select a candidate.
- Enter: inspect **all candidates for that name**.
- Space: enable/disable the selected name across both scopes.
- `r`: reload captured roots and skill configuration.
- `/`: enter a lexical filter; Enter applies it, Esc cancels it.
- `b`: return to the unfiltered list.
- PageUp/PageDown: scroll metadata/diagnostics; Esc or `q`: close.

**Management returns the complete filtered view, not pages.** Per-field shortening,
discovery bounds and diagnostic omission counts remain. Nameless malformed
candidates are searchable by scope/manifest/diagnostic rather than invented names.
Completions come from the engine's explicit-resolution projection and refresh
on installed changes and turn completion. Drafts carry only names/arguments;
the engine resolves them at admission. Child/worker panes remain skill-free.

Offline commands need no provider credentials or network:

```sh
zevria skills list [--json]
zevria skills inspect <name> [--json]
zevria skills validate <path> [--json]
zevria skills enable <name>
zevria skills disable <name>
```

List/inspect use defaults if config is absent and do not create a first-run
skeleton. `--json` emits **one complete metadata view**, with no main body,
continuation cursor or public source ID. Inspect takes a validated name.
Validation accepts only a native root-level `.md`, package directory or its
`SKILL.md`; containment is checked before reading, and nested auxiliary/outside
paths are rejected. It emits a bounded valid/diagnostics report and returns
nonzero for invalid sources or diagnostics. Mutations require existing config
and share the transactional writer. Root flags, unknown flags and combinations
with `--acp` or `--continue` are rejected.

## Session management, ACP and Ready Plans

Core management uses correlated `Manage(Skills { request_id, request })` commands
and `SkillsResult` events. Requests are `List { query }`, `Inspect { name }`,
`Reload { expected_revision }` and `SetEnabled { expected_revision, name, enabled }`.
Queries use the captured installed context, including while busy. Mutations
require an idle, writable root and the expected revision; busy work is rejected,
not queued. Fatal preparation retains the old catalog. An unchanged revision
is a no-op. Successful changes install first, then emit bounded `SkillsChanged`
invalidation and the correlated result. Reload never replaces pins.

ACP advertises **version 1** in `agentCapabilities._meta["zevria.skills"]` for
`_zevria/skills/list`, `/inspect`, `/invoke`, `/reload`, `/config/write` and the
`/changed` notification. Every operation is session-bound and rejects root or
workspace overrides. Invoke accepts validated `name` and ordered text/image
`args`, using the session's actual mode. All other versions are rejected without
an adapter. See [ACP wire fields and examples](acp-agent.md#local-skill-extensions).

**Ready-Plan skill revision is atomic.** ACP sends the narrow
`RevisePlanWithSkill { expected, name, args }` path. The engine validates the
expected Ready version, name, enablement, workflow/tool permissions and complete
prompt capacity before committing `RevisionRequested`, the invocation and its
directives together. Missing/disabled skills, stale versions, capacity rejection
or cancellation before commitment leave the Plan Ready and create no invocation
or pin. This neither approves nor implements a Plan, changes mode-selection
rules nor bypasses ensemble/workflow gates.

Ordered arguments, including image-only input, survive display, recall, edits
and persistence. ACP's 64 KiB argument prose limit is separate from shared image
limits. Submitted image bytes persist with the invocation; see
[image input](image-input.md) for privacy and limits.
