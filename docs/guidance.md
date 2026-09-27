# Automatic AGENTS.md guidance

Zevria automatically captures supplemental file guidance when a session opens or
resumes. The model does not need to discover the files or call a tool to read
them. This applies to terminal and ACP roots, fresh Plan implementation handoffs,
native Ensemble workers, and both Explore and Build subtasks.

For example, put this in your startup workspace's `AGENTS.md`:

```markdown
# Project conventions

- Keep public interfaces backward compatible unless the task says otherwise.
- Add regression tests for behavior changes.
- Explain validation that could not be run.
```

## Discovery and precedence

Only two named files are considered, in this order:

1. `~/.zevria/AGENTS.md`: global defaults, using the selected runtime's home
   (valid absolute `HOME`, or the native Windows user profile).
2. `<startup workspace>/AGENTS.md`: project guidance, overriding conflicting
   global guidance, independent of conversation history.

There is no ancestor, Git-root, descendant, alternate-config-directory, include,
or package search. Opening in a repository subdirectory does **not** find the
repository-root file. Project guidance is at the workspace root, not in
`.zevria/skills`. `ZEVRIA_CONFIG` changes the ordinary configuration location and
selects its sibling `models.jsonc`, not global guidance. If no valid home can be
resolved, global guidance is unavailable with a startup diagnostic; project
capture still proceeds. Windows and WSL global roots remain separate.

For ACP, the workspace supplied for that particular session is used, not the
agent process's working directory. Distinct ACP workspaces have independent
project guidance. External/third-party ACP agents own their own guidance
behavior; Zevria does not inject a second copy into them.

## Capture, inheritance, and resume

Capture is synchronous and occurs only at opening/resume. Ordinary prompts,
prompt edits, model and mode changes, tool continuations, and compaction use the
same cached values without guidance filesystem reads. Edits to either file take
effect only after another opening/resume. There is no setting, CLI flag, watcher,
reload command, skill, or model tool for this feature.

Explore and Build children inherit the parent's opening/resume snapshot, even if a child
is queued or launched after files change. They do not discover child-directory
guidance, read guidance files, inherit active skill authority/resources, or
repeat the parent's warnings. Each newly opened native Ensemble worker captures
independently, so it can see a newer revision than a running parent and can
legitimately issue its own startup warning.

On resume, current guidance is rebuilt before the next normal provider dispatch;
historical application/file guidance text is not restored. Missing, whitespace-only, or
rejected files contribute no body. A failed capture never retains an obsolete
body. Immediate post-resume compaction prepares current application/file guidance
in its maintenance instruction set, even before a normal turn, without activating
skills or rereading files.
Prompt edits use the current opening snapshot rather than obsolete file guidance.

The rendered `instructions` are byte-identical within a workflow across ordinary
turns, skill activations, tool continuations, retries and reconnects. Mode/synthesis
switches and skill-management mutations can change them; edits and compaction
still intentionally reset continuation. Resume remains a cache boundary for
changed guidance/instructions, not for skill directive positions: persisted
skill bodies and revocations reload at their exact historical positions. When
instructions, tools and policy are unchanged, the input prefix is preserved.
Obsolete guidance is not reproduced, and no particular server-side cache retention
or recomputation schedule is promised.

## File safety and text rules

- Each file has a hard **128 KiB (131,072 raw bytes)** limit, before decoding,
  normalization, or wrapping. Exactly-at-cap files are eligible; cap-plus-one
  files are rejected. Nothing is truncated.
- Files must be regular files and valid UTF-8. Leading whitespace, internal
  Unicode, CRLF, indentation, Markdown fences, frontmatter, and directive-like
  text are preserved. Only trailing whitespace is trimmed; a whitespace-only
  result contributes exactly empty text.
- Markdown, frontmatter, includes, and directive-looking contents are not parsed
  into engine records or executed.
- On Unix, relative or absolute symlinks are accepted only when the resolved project
  target stays inside the canonical workspace, or the global target stays inside
  the canonical `~/.zevria` root. Canonical aliases are identities, not extra
  discovery roots. Containment uses path components, not textual prefixes.
- The Unix backend opens the resolved target relative to an opened root, with
  no-follow, nonblocking component opens, descriptor regular-file checks, an
  actual cap-plus-one bounded read, and identity/size/modification checks.
  Observable resolution/open/read races are rejected. Metadata is a race
  detector, **not a fully transactional snapshot**: it cannot detect every
  concurrent same-size rewrite.
- Windows uses owned handles and component-relative `NtCreateFile` opens, with
  handle-derived identity and version checks. Local NTFS/ReFS drive paths and
  verbatim disk paths are supported. Reparse traversal (even a contained alias),
  UNC/mapped-network roots, streams and device paths fail closed. See
  [Windows protected-read boundaries](windows.md#protected-read-boundaries).
- Platforms other than Unix and Windows have no safe backend; present guidance
  is warned about and skipped. There is no unrestricted-read fallback. Missing
  guidance remains silent.

Missing and whitespace-only files are silent. Unreadable, invalid-UTF-8,
oversized, nonregular (including directories, devices, or FIFOs), escaping,
dangling/cyclic, changing, and otherwise unsafe present files are warned about
and skipped. Each scope is independent: failure of one does not block the other
or abort an otherwise valid session. Bounded startup diagnostics identify the
scope, escaped source path when available, and failure category, never the file
body. They use existing terminal/ACP startup notice paths and are logged once.
Repair the file and open/resume another session to capture the repaired version.

The loader does not create directories, write files, enumerate unrelated
locations, or execute content. Reads are byte-bounded but **filesystem latency
is not bounded**; a slow filesystem can delay opening. No such I/O occurs on
ordinary turns.

## Instruction authority, persistence, and privacy

`session.preamble` supplies the Application guidance section of the complete
engine instruction set, replacing only product identity and engineering practice;
an empty preamble omits that section, not engine-owned command conventions.
File guidance is rendered in the File guidance section, global then project, using captured
`guidance:global` and `guidance:project` components with stable scope/source
wrappers and explicit file-body boundaries. Source-path control characters are
escaped. The engine sends this complete set as top-level `instructions`, including
on user-role gateways. The wrapper does **not** turn the user-controlled file body
into engine policy; file text remains supplemental guidance.

The [engine protocol](instructions/engine-protocol.md) is the sole authority statement:
workflow policy constrains all guidance, project file guidance overrides conflicting
global defaults, and application/file/skill guidance is otherwise complementary.
The workflow's JSON declaration carries code-enforced capabilities. File guidance
cannot select modes, alter saved checkpoints, activate skills, authorize implementation,
or expand tools; conversation and summaries cannot change that boundary.
Guidance is **not a sandbox** and does not change the command tool's ambient
permissions. Prompt wrappers cannot guarantee model obedience; tool registration
and code checks remain the enforcement boundary.

Guidance bodies **and model-facing source paths** are transmitted to the
configured provider but are not persisted as engine instruction records.
Transcripts contain neither `zevria_instruction_prefix` nor `zevria_directive`;
v1 compaction checkpoints contain no `instruction_snapshot`. Skill bodies and
revocations are a separate persistence exception: hidden `zevria_skill_directive`
records store v1 payloads at their historical positions and re-render text
on load. Saved sessions containing unsupported skill directive versions must be restarted. The instruction set and these directives remain hidden from ordinary
TUI/ACP rows; no separate instruction-state file is used.

This is not blanket erasure: full historical skill activation pins and ordered
skill directives intentionally persist, ordinary conversation and generated summaries may quote instruction text,
and provider-side retention is outside this boundary. Existing files are not
erased or rewritten. Instruction-bearing older histories are rejected unchanged;
start a new session. Do not put secrets in guidance files.

A file within the byte cap can still exceed a model's instruction budget. The
combined preamble, guidance, workflow, skills, and tool definitions remain
subject to normal request admission. Irreducible instructions are rejected
before generation when they cannot fit, never silently dropped or truncated.
