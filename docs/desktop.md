# Zevria Desktop

Native GPUI Kit frontend for Zevria, running the **same host and engine in-process**
as the CLI. It is not a webview, terminal emulator, app-server client or second
implementation of provider/tool behavior. The package and binary are
`zevria-desktop`, in `crates/desktop`, with the workspace's dependency versions
and single root `Cargo.lock`.

The source was imported from the sibling desktop's sole commit,
`a322e14b3a2805e458168a70b9b78113afd0ab7c`, without merging unrelated history.
The sibling checkout is not needed to build or run the workspace.

## Build and run

Requirements: Rust able to build the locked dependencies and a macOS native Rust
build environment. A configured Zevria provider is needed for real sessions, not
for the automated tests. The pre-merge frontend was validated on **macOS 27.0.1
arm64**, **rustc 1.100.0-nightly (2026-08-23)**. The local Command Line Tools
configuration builds both profiles; a `.app` bundle is not supplied.

From the Zevria repository root:

```sh
cargo run -p zevria-desktop --locked --release -- /absolute/path/to/workspace
cargo run -p zevria-desktop --locked --release -- /absolute/path/to/workspace --continue
# -c is the short form; omit WORKSPACE to use cwd (a root cwd opens the picker).
cargo run -p zevria-desktop --locked --release -- --help
```

Usage is `zevria-desktop [WORKSPACE] [--continue|-c]`. Use `--` before a workspace
whose name begins with `-`. With two workspace binaries, select `-p zevria` for
the CLI or `-p zevria-desktop` for the desktop rather than bare `cargo run`.

**The native GUI supports macOS only.** On Linux/Windows the package builds its
GPUI-free CLI/session model and tests, plus a stub executable that reports the
unsupported platform and suggests the Zevria CLI. `--help`/`-h` works on every
platform. The desktop's GPUI Kit, math-rendering and tracing-appender dependency declarations
and its actions, controller, engine, launch and UI modules are macOS-gated. Native
Linux/Windows builds do not need GPUI; the shared host's existing logging
dependencies are unchanged.

Start from a terminal. Finder launches have a minimal environment; command-based
API keys may need a suitable PATH. The host's shell preparation, configuration,
models.jsonc, workspace guidance, skills, tools, policy enforcement, leases and
transcript formats are unchanged.

On first run `Config::load` creates the ordinary Zevria config skeleton and
returns an actionable error. The window shows that error rather than exiting:
**Show config in Finder**, fill in the generated files, then **Try again**. The desktop does
not create another configuration schema. Logs default to
`~/.zevria/logs/zevria-desktop.log` (or the configured `[log]` directory).
The old `zevria-app.log` is left alone; update any scripts using the old binary
or log name.

## Design language

### One working thread, three voices

The transcript is one left-aligned working thread within a centered column, not
message bubbles or a stack of cards. Its line and nodes record what happened,
in provider order:

- **Pencil:** a dashed thread and Charter italic 13/21 in graphite pencil ink
  for provider-supplied thinking. Expanded notes are indented 24 px under the
  gray Thinking label; the folded preview is also italic. Plan documents use
  upright Charter 15/24 instead.
- **Carbon:** lifecycle nodes and Menlo 12/18 for commands, paths, code, literal
  output and diffs. Expanded tools use recessed 8 px wells, with input before
  output and then the native diff. Output opens at 16 lines; **Show all N lines**
  expands in the transcript rather than creating another output scroll trap.
- **Ink:** the solid thread and SF Pro (`.SystemUIFont`) 14/22 for answers;
  prompts use the same size with medium weight and a hanging **You** label.
  You/Task, Zevria and System labels use their semantic role inks; the Plan
  label uses the Plan workflow ink. Lavender belongs to the Zevria answer label,
  not thoughts; answer headings use `content.heading`.

Build mint and Plan blue mark live work, mode controls and Send. A green ✓ means
success, ✗ means failure, ⊘ means denied and ◼ means stopped or interrupted.
Shapes and words always accompany state colors. Tool labels remain secondary
ink; shell syntax, not the label, carries color. Background tags use info ink.

SF Pro, Charter and Menlo are public macOS-bundled font families, with native
fallback. Transcript typography is centralized as `Type { size, leading }` in
`ui/style.rs` (all values below are size/line height in pixels):

| Role | Size/leading | Use |
| --- | --- | --- |
| `READING` | 14/22 | Answers, prompts, empty-state and screen body text |
| `DOCUMENT` | 15/24 | Charter Plan documents, in the thread and review sheet |
| `PENCIL` | 13/21 | Italic Charter thinking previews and expanded notes |
| `CODE` | 12/18 | Menlo command headers, wells, code, completions and mono errors |
| `SECONDARY` | 13/20 | In-thread notices, live tail, web/ensemble text, subtask titles |
| `HEADINGS` | 18/24, 16/22, 15/21 | Markdown h1, h2, h3+; Plan card title uses h2 |
| `LABEL`, `META` | 12/16, 11/14 | Gutter/tool labels and tabular metadata (unchanged) |

Kit's window/rem base remains 14 px; its mono size follows `CODE`. Sidebar,
title/status bars, composer text (14/20), question dock, sheets and Kit controls
keep their chrome sizes. Prose and thinking measure up to 680 px; machine work
measures up to 880 px. The 1000 px column (96 px gutter plus 24 px gap and content)
centers on wide panes and fills narrower ones with a 16 px inset. The gutter,
thread and composer share one axis at every width. Spacing follows a 4 px grid;
turn boundaries add a 32 px gap and a hairline. Tags, controls, carbon wells, the
composer and decision sheets have distinct 4/6/8/12/14 px radii.

The sidebar pins **This session**, using its first prompt, with subtasks nested
beneath it. Earlier sessions are single-line titles with aligned relative times.
Arrow keys navigate those rows; Enter opens one. Below a 900 px window the
sidebar auto-hides; **⌃⌘S** overrides it. Below a 760 px main column the gutter
labels move above content. A Plan sheet overlays instead of squeezing the main
column below 560 px. The minimum window is 720×520.

Questions **dock at the end of the thread in place of the composer**, without a
scrim. The transcript stays scrollable, and the dock scrolls internally above
55% of the window height. Drafts survive answering/dismissing. Only the
app-level **Stop the current turn?** replacement confirmation remains modal.

### Thinking is supplied text, not an invented explanation

Only readable provider text creates a **Thinking** disclosure. Empty,
encrypted-only, signature-only, redacted-only and unknown typed parts create
neither a heading nor placeholder thoughts. Anthropic `thinking_display =
"omitted"`, Responses `send_reasoning = false` and models without readable
summaries can therefore show no section. The UI does not infer missing thoughts.

- Live thinking starts **expanded**. Earlier thinking in the same running turn
  stays open. At turn end, untouched sections fold to their first non-empty
  supplied line (Markdown markers removed) and a supplied-text word count.
- Explicit expand/collapse choices win through later deltas and tail-to-commit
  identity transfer. Restored sessions start folded. Projection replacement
  retires old view choices; the next turn never inherits a previous tail's fold.
- Folding waits while you are scrolled away from the latest output, then applies
  when tail following resumes. Interrupted and unsaved live sections stay open
  so they explain the stop. Saved interrupted output is folded on restore.
- For message-only streams such as Anthropic, a correlated Stop or failure keeps
  the readable partial **in this window only**, labeled **Stopped, not saved to
  this session**. It is not engine history, is not persisted and is absent after
  restore. Responses interrupted attempts retain their existing persisted data.
- Stop is a neutral **◼ Stopped** node, not a red error. Incomplete attempts say
  **Stopped before the response finished** or **The response failed before it
  finished**. Actual failures remain red. Copy payloads are unchanged.

Thinking Markdown stays pencil-toned: headings are semibold at the pencil size,
inline code and unhighlighted code blocks use pencil on the panel, and only
links retain `content.link` for affordance. No extra vertical rule is added
beside the existing dashed thread. In the TUI, reasoning Markdown inherits its
muted italic tone throughout, including bold titles, headings, inline/fenced
code and underlined links; ordinary answer Markdown keeps its accents.

Thinking headers are full-width, at least 28 px tall, focusable native Button
roles with an expanded state, the accessible name **Thinking**, and the preview
as their description. Enter and Space activate them. Copy and text selection
remain available; answer code fences also have a language label and Copy action.

The thread's live tip and the status bar share one vocabulary: **Waiting for the
model**, **Thinking**, **Writing**, **Running command** (or another tool name),
**Reconnecting** with attempt/countdown, **Compacting context**, and **Stopping**.
Opaque-only progress cannot be called thinking, so it remains waiting.

### Commands: display without routing, copy exactly

Both frontends share the renderer-free `zevria_presentation::shell` display rule.
At command positions (including after shell separators and inside substitutions),
`rtk`, its recognized global options and an optional `proxy`/`run` are hidden only
when another command word follows. Standalone `rtk`, `rtk --version`, argument
text such as `echo rtk`, `./rtk` and uppercase `RTK` remain unchanged. Quoted
strings, comments and heredoc bodies are not stripped. The lexer is tolerant of
incomplete/streaming input; it is a presentation heuristic, not a shell parser.

Desktop command headers, every TUI command row/fold header, both frontends' job
notices, and the TUI jobs list/inspect header use the stripped display. The TUI
runs its existing Bash grammar on that text, so the real program gets function
ink. Desktop maps shell spans onto existing semantic tokens: programs to
`syntax.function`; flags, numbers and expansions to `syntax.constant`; strings
and heredocs to `syntax.string`; operators/keywords to `syntax.keyword`; comments
and routing to `syntax.comment`; leading assignments to secondary ink. Ordinary
arguments keep primary ink. Path headers mute the directory and retain primary
ink for the file name.

Desktop's expanded Input retains the exact `sh` fence with routing dimmed in
comment ink. Desktop Copy input/code-block Copy, TUI `y`/`yy`, stored transcripts,
execution and model-facing text all retain the original bytes. ACP tool titles
and job notices also remain exact, as do agent-supplied titles in TUI ACP worker
rows. Native terminal mouse selection naturally copies rendered, stripped text;
the TUI's own copy actions are the exact-source route.

### Appearance, focus and motion

With no `[theme]` selector, Desktop follows the native macOS appearance live:
**Zevria Graphite** in dark appearance and **Zevria Graphite Light** in light
appearance. The light sibling is near-neutral paper, not cream: `#FBFAF8` plane,
`#F2F0EC` wells, `#FFFFFF` sheets and `#221F1B` ink. Dark Graphite keeps its
`#201E1B` plane and `#63DFB1` Build mint unchanged. A named theme pins one palette
in **both** frontends; the TUI default remains Zevria Dark. See [themes](themes.md).

Components consume semantic roles. Pencil, thread and focus tones are derived
once per palette; pencil seeks APCA magnitude 75 between secondary and primary
ink, thread seeks 3:1 against the plane, and focus uses the selection color.
Reference palettes retain the shared 27-reference schema and validation contract.
Native appearance changes update Kit, Markdown, syntax/diff colors and the
color-keyed math raster inputs, without changing transcript or provider data.

The only ambient motion is the shared ◐◓◑◒ work glyph on the existing 250 ms
clock. **Reduce Motion** freezes it at ◐ and makes Plan-sheet opening instant;
disclosures are instant in either setting. Text fields use Kit's tinted border
plus one concentric 3 px halo at 50% alpha, touching the border and accounting for
its width. The composer has a uniform 1 px border with a 12 px radius, no mode
stripe and no second focus outline; mode stays on the gutter selector/compact
toggle and Send. Custom disclosures use a concentric 2 px selection-ink ring
with a 2 px offset (outer radius = control radius + 4 px). Ancestors leave room
for the halo instead of clipping it. Icon controls have accessible names;
thinking and tool headers expose their expanded state. GPUI Kit has no Increase Contrast signal,
so that separate macOS preference is not implemented.

Clicking blank space moves focus to the window, clearing disclosure, Copy,
sidebar and Kit button focus indicators and blurring the composer. Window
shortcuts keep working. Clicking anywhere inside the composer panel focuses it
again; typing alone does not. Focus rings still appear on clicked controls, not
only during keyboard navigation. The window also holds resting focus while the
session is not Live, so shortcuts and menu actions work on start, stop and
no-workspace screens and during session switches.

### Screenshot gallery

The opt-in gallery renders the **real DesktopWindow** without a provider or
credentials, using macOS's text system and the Metal headless renderer:

```sh
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
cargo test --release --locked -p zevria-desktop --features fixtures --test gallery
# Optional output-directory override (absolute paths are easiest):
ZEVRIA_DESKTOP_GALLERY_DIR=/tmp/zevria-gallery \
cargo test --release --locked -p zevria-desktop --features fixtures --test gallery
```

PNGs are written to the repository's `target/desktop-gallery/`. The 1240×840 and
720×840 logical viewports are captured at the native test renderer's 2× scale.
Empty, working, question, Plan and completion scenes also have wide Graphite
1920×1080 captures named `{scene}-1920x1080.png`, showing the centered column.
Thinking-live, question-dock and Plan-overlay scenes also have dark/light
720×520 minimum-window captures.
The gallery covers configuration errors, no workspace, starting, stopping,
failed/idle/empty sessions, working prose and tool loops with an expanded native
diff, Plan review, questions, replacement confirmation and completions. Working
scenes, thinking-live, thinking-restored, thinking-interrupted, thinking-unsaved,
code-heavy answers, docked questions and Plan review also render under Graphite
Light. The `math` scene restores
the shared Hölder proof fixture at 1240×3200 and 720×3200, plus a light-palette
1240×3200 capture. It contains all 15 inline and 11 display formulas, including
large delimiters, boxes and an aligned derivation with a 2 mm row gap.

For an opt-in, **read-only** capture of an existing session:

```sh
rtk proxy env CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  ZEVRIA_DESKTOP_GALLERY_SESSION=/absolute/path/to/session.jsonl \
  cargo test --release --locked -p zevria-desktop --features fixtures --test gallery
```

This adds `session-1240.png` and `session-720.png`. The gallery calls
`zevria_transcript::transcript::load` and projects validated items without
starting a host/provider, acquiring a lease, appending, repairing, or modifying
the file. No session is captured unless that environment variable is set. The
executable runs on the macOS main thread and requires a compatible Metal device. It is excluded from
ordinary `cargo test`; the `fixtures` feature exposes only dev fixture entry
points, not a production session mode.

Kit 0.9's syntect-only highlight-status type is an empty compatibility stub.
The adapter supplies the portable status document for Kit builds that support
it, but native diffs actually consume the mapped `success`/`danger` colors.
No tree-sitter dependency is enabled merely to fill that stub.

## Math

All desktop **Markdown** views share one stable math extension registration:
transcript prose, reasoning, Markdown tool details/notices, Plan documents and
the Plan review panel. HTML-backed tool output, errors, plain text and startup
screens retain their current behavior. The TUI is unchanged.

### Syntax and copying

- Inline `\(…\)` works in paragraphs, headings, link labels and table cells.
  Backslashes pair with the next byte: `\\)` does not close a formula, and
  `\\[2mm]` inside an aligned derivation stays raw TeX. Inline content can
  continue onto the next line in the same paragraph. An unmatched opener stays
  a Markdown character escape, rather than swallowing the rest of a message.
- A line containing only `\[` (after up to three spaces) opens display math.
  It can interrupt prose without a blank line and works inside lists/quotes.
  A line ending in an unpaired `\]` closes it; TeX may precede the closer on
  that line. An unclosed fence extends to the container/document end like an
  unclosed code fence, and falls back to the existing code-block presentation.
- Whole-line `\[…\]` and `$$…$$` are displays. Trailing non-whitespace declines
  the block, so `\[1\] Smith et al.` is still ordinary Markdown. Mid-line
  `\[…\]` remains escaped brackets, not math.
- Single-dollar math follows Pandoc's rule: an opener must precede
  non-whitespace; a closer must follow non-whitespace and must not precede a
  digit. `$5 and $10` and `costs $5-$10` remain currency; `$x$, $y$` and `$5$`
  are math. Unusual prose such as `$FOO$BAR` can still be ambiguous.
- Existing `$$`/`$$$` fences, inline multi-dollar math, indented code, code spans,
  and fenced code (including a `math` language label) retain their syntax.
  A continuation line that looks like a list item can end the inline paragraph.

The parser never rewrites message bytes. Every custom node's Plain and Source
copy representations contain its exact original source, **including delimiters**,
and its accessibility label contains the TeX. Inline math selects as one atomic
object; display math selects as one block. Copy buttons and whole-message/Plan
copy still use their unchanged raw payloads. Kit's ordinary clipboard Copy trims
outer whitespace; Source-format select-all otherwise uses the original document.
Invalid, unsupported, empty or oversized inline math shows atomic source text,
which Kit fits to the line. Failed/unclosed displays decline the plugin and use
Kit's existing code block. There is no implicit TeX repair.

### Native rendering and limits

RaTeX **0.1.15**, a pure-Rust KaTeX port, parses and lays out math in-process.
The parser plugin runs parse → layout → validation → display-list conversion
inside `catch_unwind`, after a TeX size check. Kit runs document parsing on its
background task, except small (≤4 KiB) full replacements that may parse
synchronously. A 1,024-entry typeset memo caches successes **and failures**,
including repeated inline probes and streaming tail reparses.

Math uses 1.21× the surrounding font size, an alphabetic inline baseline, and
the effective inherited text color (including links, headings and muted text).
RaTeX's default black means inherit; explicit TeX colors/backgrounds are honored.
A private alpha sentinel on the base ink distinguishes inheritance from explicit
opaque black, so `\color{black}` and `\colorbox{black}` stay black on dark themes.
Inline advances use layout metrics, not ink bounds; overhanging ink adds no
spacing. Inline math wider than 480 logical pixels scales down. Displays have
small vertical padding, center within the column, and scroll horizontally when
wider, without converting vertical wheel input into horizontal motion.

At paint time, tiny-skia rasterizes the embedded KaTeX TTF outlines, rules,
rectangles and paths directly into a device-resolution, transparent, straight
BGRA `RenderImage`. GPUI paints CJK, emoji and missing KaTeX glyphs at RaTeX's
positions through native text fallback; no system font files are copied into
memory. No webview, Node.js, worker process, network, TeX installation, SVG atlas
or PNG encoding/decoding is involved. Offscreen measurement creates no rasters.
The app-owned LRU keys formula/mode, em, device scale and color, verifies full
TeX on a hit, and calls `App::drop_image` when evicting an image.

Named, tested budgets:

| Resource | Limit |
| --- | --- |
| TeX source per formula | 16 KiB |
| Formula layout | 200 em wide, 100 em total height |
| One raster | 4,096 device pixels per side, 4,000,000 pixels total |
| Decoded raster LRU | 32 MiB or 1,024 entries, whichever comes first |
| Typeset memo | 1,024 entries |
| RaTeX built-in guards | 1,000 macro expansions, nesting 32, 65,536 output items |

Device scales or extreme overhangs that exceed the raster budget show source
rather than leaving a blank box. CPU pixels and GPUI atlas tiles are released
on eviction. Raster antialiasing is grayscale rather than subpixel; fallback
Unicode spacing uses RaTeX's approximate metrics. Very large working sets can
thrash the bounded cache. Initial typesetting/rasterization is synchronous but
bounded; the planning prototype measured about 1 ms warm to typeset the complete
26-formula proof, and about 7 ms to rasterize it at DPR 2 / 19.4 px em. Those are
prototype observations, not an end-to-end real-provider latency guarantee.
Formula contents are never logged by this module.

### Parser maintenance

Markdown-rs 1.0.0 comes from `liukaizheng/markdown-rs`, pinned to Git commit
`e629e7e4370cac5cc6dac39971c77feba3e3aaf9` through root `[patch.crates-io]`.
There is no vendored source or local-path override. Two isolated constructs add
source-faithful TeX math, and the existing raw-text tokenizer handles dollar
policy and failed-scan memos. The original `math_text`/`math_flow` switches gate
the patch; no public options, serde or AST formats change. `to_mdast` and
`to_html` are unmodified, and math-disabled parsing retains upstream behavior.

See [the provenance and maintenance ledger](markdown-parser-patch.md) for the
exact source revision, syntax, license and update procedure. Application-specific
proof/parser tests live in `crates/desktop/tests/markdown_math.rs` and run in the
existing CI on all platforms without GPUI. The fork maintains its own generic
fixtures and locked workspace CI; its historic SWC/Serde test lock does not
constrain Zevria's Serde versions or add SWC to this workspace. Same-line display
candidates can retract during streaming; the ledger records that incremental
parser boundary separately from the fully covered proof fixture.

## Included controls

- Open workspace (Cmd-O), new session (Cmd-N), Sessions sidebar and
  `--continue`. If there is no latest session, continue starts a new one with a
  notice. Replacing a busy session from the sidebar offers **Keep working** or
  **Stop and switch**.
- Live/restored transcripts, selectable user text, retained Markdown, collapsible
  reasoning, web activity, native tool rows, structured task/reconciliation
  lists, command fences, file-change counts and expandable diffs, Plan artifacts
  and handoffs, validated prompt-image thumbnails, errors and compaction dividers.
- Multiline composer: **Enter** sends, **Shift-Enter** inserts a newline. Shared
  slash/skill completions support click, arrow selection, Tab and Enter. Built-in
  completion Enter executes the selected built-in; skill completion only inserts.
- Build/Plan selection, next-Build-prompt Orchestrate subtasks switch, Stop / Cmd-. / Esc.
  Esc first belongs to an open question, review panel or completion. Cancellation
  targets the pending operation or the learned active turn, never a guessed ID.
- Plan review: Implement, Implement in fresh session, Revise / recovery Close,
  Copy plan as Markdown and reopen Review plan. Every decision includes the authoritative version.
  After Escape dismisses the sheet, Send submits ordinary feedback or `$skill` as
  a Plan revision of that reviewed version, never as approval or implementation.
  Rejection restores the draft and keeps review hidden. Cmd-Enter implements only
  while the review sheet is visible.
- Text, single-select, Other, multi-select and optional-skip questions. Native
  answers use strings; ACP multi-select uses arrays. Dismiss is available only
  when the request permits it. Terminal/close correlation cannot dismiss a newer
  unrelated question.
- Read-only subtask inspection and a breadcrumb back to the root. Child activity
  never takes over root busy state or root question ownership.
- Shared profile/reasoning/context/response/network status and persistence/Plan
  warnings; 250 ms clock updates run only while root work is pending/active.
- Exact primary/output copy values remain available even when display is bounded.

Additional keyboard controls have discoverable View/Session menu items:

| Keys | Action |
| --- | --- |
| ⌥⌘→ / ⌥⌘← | Expand/collapse the focused thinking or tool block; from the composer, the latest thinking |
| ⌥⇧⌘→ / ⌥⇧⌘← | Expand/collapse all thinking in the visible pane |
| ⌥⌘↓ | Jump to latest and resume tail following |
| ⌘↩ | Send the open question answer or implement the open Plan |
| ⇧⌘M | Switch the next-turn Build/Plan mode |

Shift-Tab remains reverse focus traversal. Esc belongs first to an open
replacement confirmation, dismissible question, completion or Plan sheet;
a non-dismissible question cannot be dismissed with Esc. **⌘.** and the Stop
control still cancel the active root turn, including while a question is open.

Supported shared grammar: ordinary text, `/orchestrate <prompt>`, `$skill [args]`,
`/compact`, `/implement`, `/implement-fresh`, `/plan`, `/build`, `/new`, `/resume`.
Model/reasoning, skill-management, worker-review and `/ensemble-*` commands retain
the draft and show an explicit not-yet-available notice. Input while busy is not
queued as another turn. A submitted draft stays until acceptance; failures and
cancellation restore it without replacing a newer draft (Recover draft keeps
both when necessary).

## Architecture and ownership

```text
GPUI window + native components
  -> SessionController (Idle / Starting / Live / Stopping / Failed)
  -> zevria-host::RunningSession
       -> production engine, providers, tools, supervisors, persistence, lease
  -> desktop WorkspaceModel (root, read-only children, question, registry)
       -> zevria-presentation::SessionModel per pane
            -> shared reducer, admission, Plan workflow, semantic blocks, tail
```

| Owner | Responsibility |
| --- | --- |
| `zevria-host` (formerly `crates/app`) | Read-only configuration, production registries/provider/engine composition, skills, leases, supervisors, root-session discovery, transcript loading and ordered shutdown. Only this crate owns core. |
| `zevria-presentation` | Shared semantic blocks and native/ACP projection, lifecycle/telemetry, Plan workflow, command/skill/completion grammar, `SessionModel`, typed admission, restoration and live-tail derivation. No renderer, host, theme or engine dependency. |
| `zevria-theme` | Immutable semantic sRGB tokens and concrete user themes, unchanged OKLCH generation and contrast/CVD validation. Generic token groups map to renderer color types. |
| `zevria-tui-widgets::theme` | Thin, lazy Ratatui color adapter; preserves startup freeze and terminal call sites. |
| TUI `App` / `SessionViews` | Terminal editor/recall drafts, selection, viewport, folds, overlays and existing worker-review routing. `App` applies `Reduction` hooks instead of interpreting lifecycle events again. |
| Desktop `WorkspaceModel` | GPUI-free root/child/question routing and command metadata; each pane is the shared `SessionModel`. |
| Desktop `SessionController` | Starting/live/stopping/failed ownership, replacement, event/exit tasks, commands, desktop drafts, session catalog and graceful quit. |
| Desktop `ui/math/` | Source-faithful math plugins, bounded in-process em layout, direct KaTeX rasterization and evictable images; no provider/persistence dependencies. |
| Desktop `ui/` | Native retained Markdown/plain text, gutter-led tool/diff/image rows, virtualized list invalidation, composer, sidebar, Plan/question surfaces and status. |

`zevria-host` was previously the workspace's `zevria-app` package; it is not the
renamed desktop. The desktop is a leaf workspace frontend: no package may depend
on it, it cannot reach any TUI crate, and it has no direct Ratatui/Crossterm
production dependency, including target-specific dependencies. The manifest-boundary
tests discover all `crates/*` automatically and enforce these rules. Neither the
host nor desktop has a Ratatui/Crossterm dependency path.

The CLI's `cargo build -p zevria` graph contains no GPUI or RaTeX. Neither does
the desktop's non-macOS compilation graph. Math and all of its runtime
dependencies are macOS-only. The desktop's test-only Markdown dependency allows
portable parser regressions without adding a renderer to other targets.
macOS `--workspace`
builds do include GPUI and unify additive features in shared dependencies, such
as extra image codecs and regex support. Cross-platform workspace CI includes
the desktop; CLI release check/test jobs exclude `zevria-desktop` to retain the
shipping feature set and disk footprint. Workspace CI disables incremental
compilation and dev/test debuginfo to absorb the native frontend's artifact growth.

There is no RPC or app-server layer. The unmerged out-of-process `zevria-app`
branch was not used or modified. No instruction text, tool registration order,
request construction, routing policy or transcript envelopes were changed;
cacheable prompt prefixes remain under the existing regression tests.

### Important invariants

- Host `take_restored()` is a one-time move. Heavy root/child/journal data does not
  have to be loaded again by a frontend; the runtime keeps its cleanup paths and
  ownership lease. The CLI consumes the same payload as the desktop. The terminal
  resume picker receives an injected host catalog callback, not a host dependency.
- `SessionModel::reduce` reports exactly the old acceptance/settlement/Plan/mode
  hook points. Error rows, Plan artifacts and edit-tail truncation are neutral;
  terminal drafts, selection and viewport actions remain terminal-local. Existing
  characterization assertions were preserved, not changed to accept new behavior.
- Pending cancellation uses `None`; promoted work uses the authoritative turn ID.
  Stale/foreign progress and terminal events cannot settle another operation.
- Stream fences belong to `SessionEventReceiver`. A stale **unread** preview is
  discarded by a lifecycle fence; a genuinely new post-Intermediate preview must
  still be accepted. A reducer-level blanket ban would break legitimate tool-loop
  continuations and hosted-part updates.
- Mode selection checks both request identity and requested mode; Ready always
  projects Plan approval independently of restored ordinary mode metadata.
- One process-wide multithread Tokio runtime owns the engine. GPUI foreground
  tasks await its handles and the existing receiver directly, at most 64 updates
  per wake, without a relay thread, forwarding queue or RPC. Initial restoration
  projection is `Send` and runs off-main-thread; session discovery and large diff
  parsing use GPUI's background executor.
- Transcript rows use stable projection epoch/block IDs, fingerprints, list
  splices and remeasurement. Markdown state is retained and updated cumulatively;
  asynchronous parser and diff-completion notifications remeasure the correct row.
  Ordinary transcript text is not truncated by the tool-preview display bounds.
  Turn-continuation flags participate in structural fingerprints; clock-only
  redraws do not scan history. Tail BlockData is cached per data update, plan
  Markdown clones only when its version changes, and removed block views are
  pruned. Tail keys include epoch, call header, kind and non-tool ordinal;
  compatible commits retain both disclosure choice and Markdown entity. Pane
  switches retain each pane's choices, parsing and scroll anchor; a projection
  replacement retires them. Ambiguous/mismatched boundaries fall back to policy,
  not another block's state.
  Switching width class remeasures rows so labels and virtualized heights agree.
- Desktop styling never enters request construction, tool registration, prompts
  or transcript envelopes. The provider's cacheable prompt prefix is unchanged.

### Shutdown

Window close and Cmd-Q do **not** quit immediately. The controller stops the pump
(dropping its sole receiver), drops cloned senders/exit/tick tasks, awaits the
host's ordered engine/supervisor shutdown and lease release, flushes the named
log guard, then quits. Replacements use the same path, including replacements
during startup. Cleanup failures are surfaced rather than starting another stack
early; a runtime exit does not permit starting another engine before cleanup.

OS-forced termination only gets GPUI's short best-effort quit fallback. It cannot
guarantee normal shutdown; durable append/recovery and stale-lease handling remain
host responsibilities. Do not rely on a `.app` run loop returning after quit.
Closing while startup is still constructing the host waits for that construction;
slow external credential commands can therefore delay close.

## Tests and validation

Run from the workspace root on macOS:

```sh
rtk cargo fmt --all -- --check
rtk cargo check --release --locked --workspace --all-targets
rtk cargo build --release --locked -p zevria
rtk cargo test --release --locked --workspace
rtk cargo test --release --locked -p zevria-desktop --test markdown_math
rtk cargo test --release --locked -p zevria-desktop
rtk cargo test --release --locked -p zevria-desktop --features fixtures --test gallery
rtk cargo test --release --locked -p zevria --test crate_boundaries
rtk cargo test --release --locked -p zevria-provider -p zevria-host --features cache-diagnostics
rtk cargo clippy --release --locked -p zevria-desktop --all-targets -- -D warnings
rtk cargo clippy --release --locked -p zevria-desktop --all-targets --features fixtures -- -D warnings
rtk cargo build --locked --release -p zevria-desktop
rtk proxy target/release/zevria-desktop --help
rtk cargo tree --locked -p zevria-desktop -i ratatui
rtk cargo tree --locked -p zevria-desktop -i crossterm
rtk proxy cargo tree --locked -p zevria --prefix none | rg -ci '^(gpui|ratex)'
rtk proxy cargo tree --locked -p zevria-desktop --target x86_64-unknown-linux-gnu --prefix none | rg -ci '^(gpui|ratex)'
rtk proxy cargo tree --locked -p zevria-desktop --target x86_64-pc-windows-msvc --prefix none | rg -ci '^(gpui|ratex)'
rtk cargo tree --locked -p zevria-desktop -e features -i rustls
```

The inverse terminal trees should report that the packages are absent from the
desktop graph; Cargo's nonzero absent-package result is expected. The CLI and
non-macOS desktop GPUI/RaTeX counts must be `0` (`rg` returns 1 for no matches). Native rustls must enable only `aws-lc-rs`, not `ring`; recheck
this after any GPUI Kit bump because enabling both providers can make the
WebSocket TLS builder panic. `ring` in the lockfile alone is not a violation:
the wasm-only `gpui-pre-reqwest` dependency can use it.

For the same disk limits as CI, set `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0` before validation.
The five individually non-empty default-feature provider-prefix release gates
are listed in `.github/workflows/release.yml` and must still pass independently
of desktop feature unification.

Automated coverage includes renderer-free routing/question/grammar/fingerprint
checks, binary help/argument handling, headless **real native controls**, two
controller tests using a separate real Tokio runtime, and a local HTTP SSE
fixture exercising host start, submit, completion through `WorkspaceModel`,
ordered shutdown and resume. Real-I/O controller tests explicitly allow
nondeterministic external wakes in GPUI's test scheduler; ordinary UI tests use
synthetic events and a command channel. Linux/Windows run the GPUI-free CLI,
routing/question and local-SSE tests, including the unsupported-platform stub
check. No extra local targets/toolchains are needed for this; shared CI covers
those platforms. Broad package-acquisition commands such as `cargo fetch` or
unfiltered metadata can still fetch other platforms' crates. Cross-target
inspection from a macOS host can also acquire GPUI/platform archives before
narrowing the compilation graph; an offline failure there does not mean the
non-macOS stub compiles GPUI.

### Focused regression coverage

Native control tests cover live/restored/interrupted thinking, retained row and
pane identity, deferred folding, keyboard controls and question ownership.
Layout/focus regressions check the centered column and composer axis, sidebar
and width changes, blank-click blur/refocus, and Plan Copy in overlay and docked
panels. Compact-window tests cover completion selection, radios/Other/skip,
Reduce Motion and row remeasurement.

Parser coverage includes disabled-mode goldens, the complete inline/display
math proof and incremental parsing boundaries. Native math tests cover source
copying, raster allocation, baseline rounding, Unicode fallback, resource limits,
LRU eviction and typeset memoization. The active parser pin, provenance and
maintenance obligations are recorded in [Desktop Markdown parser fork](markdown-parser-patch.md).

Headless controls, local-SSE fixtures and Metal gallery captures do not establish
human visual acceptance or a real-provider GUI run. macOS appearance switching,
VoiceOver, native window interactions and real-provider sessions still require
the manual checklist below.

## Manual acceptance (not performed)

Automated headless tests and the local SSE fixture **do not constitute a manual
real-provider GUI acceptance run**. Check these on a scratch workspace before a
release:

- [ ] First-run config error, Show config in Finder and Try again.
- [ ] Drag the title strip; double-click to zoom; check traffic-light placement.
- [ ] Maximize and full screen keep one centered axis.
- [ ] Click a chevron, then empty space: the ring clears and ⇧⌘M still works.
- [ ] Toggle the sidebar with ⌃⌘S; resize to 720×520; inspect Graphite Light and
  a generated named theme. Change macOS appearance while streaming; the default
  pair must switch live and a named theme must stay pinned.
- [ ] Keyboard-only pass: sidebar arrows/Enter; Tab and reverse Shift-Tab;
  Enter/Space on thinking; ⌥⌘→/←; expand/collapse-all; ⌥⌘↓; ⇧⌘M; ⌘↩ decisions.
  Check every visible focus ring, including virtualized and narrow-window rows.
- [ ] VoiceOver spot check: Thinking has its Button role, name, supplied preview
  description and expanded state; Copy and Jump to latest have meaningful names.
- [ ] Test Reduce Motion on and off: the live tip stays at ◐ and Plan opening is
  instant when enabled.
- [ ] Open workspace (including ⌘O on the no-workspace screen), new session,
  streamed Markdown and native command/edit rows.
- [ ] Real Anthropic session: live thinking starts expanded; manual collapse
  survives deltas and commit; Stop mid-thinking keeps the marked **unsaved**
  partial before a neutral Stopped node. Restore and confirm it is absent.
- [ ] Real Responses session: live summary thinking is visible, not Waiting;
  Stop mid-thinking retains the persisted interrupted attempt. Restore and
  reopen its folded supplied text. Test a model/config without readable thoughts:
  no empty Thinking header or invented content should appear.
- [ ] Complete a turn: untouched thinking folds; explicit choices persist.
  Scroll up before completion and verify no jump/fold until Jump to latest.
  Enter another turn and confirm it does not inherit the previous fold.
- [ ] Plan → Ready → Implement; repeat with Implement in fresh session.
- [ ] Text/select/Other/multi-select/optional questions in the dock: transcript
  scroll stays available, ⌘↩ sends, Esc only dismisses when allowed, and the
  composer's draft returns unchanged. At 720×520 inspect the Plan overlay and
  question dock without obscured controls.
- [ ] Launch and inspect a subtask; return to the root while it streams.
- [ ] Resume from the sidebar and `--continue` (including no prior root).
- [ ] Close/Cmd-Q while a turn is active, then resume cleanly.
- [ ] Scroll away during streaming, expand diffs/reasoning, copy input/output.
- [ ] Open the Hölder-proof session from the sidebar; scroll, select/Cmd-A/copy,
  drag across prose and atomic formulas, resize to compact, and check the Plan
  panel. Inspect math baselines, SF Pro/Charter sizing, display centering,
  aligned 2 mm row gaps, boxes, large delimiters and narrow horizontal scrolling.

No external provider credentials were exercised for this acceptance checklist.

## Troubleshooting

- **Config skeleton/error:** use Show config in Finder and Try again; provider/model errors
  are host errors, not a desktop-specific configuration layer.
- **Engine stopped:** inspect `zevria-desktop.log`, then Start new session. Startup
  and cleanup errors remain visible; session replacements do not overlap engines.
- **Build/test-support resolution:** keep the root `Cargo.lock` and use `--locked`.
  When intentionally changing Kit features, resolve from the workspace root rather
  than hand-patching transitive versions. The validated graph retains taffy 0.15.0
  and unicode-properties 0.1.3 alongside Kit's test-support dependencies.
- **Disk pressure on macOS:** native workspace builds add GPUI artifacts. Disable
  dev/test debuginfo and incremental compilation as above; use `-p zevria` when
  only working on the CLI. Rust-analyzer also indexes/builds the native dependency.
- **Key-command environment:** launch via the terminal and check the command's
  PATH; credentials are still resolved exclusively by the host/provider layers.
- **Unsupported platform:** Linux/Windows support only the GPUI-free tests and
  stub binary. Use `zevria`; native GUI support requires lifting the target gate
  and adding platform GUI development packages and validation to CI.

## Explicit limitations

- Math is KaTeX-compatible, not a TeX engine: unsupported commands show source,
  not an approximation. Native math/gallery require macOS and Metal. Raster
  overhang, device dimensions and caches have the budgets listed above; Unicode
  fallback metrics and grayscale antialiasing can differ from surrounding text.
- No live ensemble-worker panes or worker confirmation/review/abandon controls.
  `/ensemble-plan` and `/ensemble-review` are disabled. Restored unfinished Plan
  ensembles can be observed through root rows and cancelled, but confirmation
  still requires the TUI; see [ensemble workflows](ensemble.md).
- No model/reasoning picker or skill-management UI (identity/status and startup
  skill completions only). Unsupported built-ins are explicitly rejected locally.
- No transcript recall/edit UI, image attachment input or `@` file mentions.
  The shared model already supports transcript editing; restored images display.
- One window and one live session; no tabs or concurrent-session UI.
- No `.app` packaging, signing or notarization; no native Windows/Linux GUI.
- No theme-file hot reload or completed human accessibility/keyboard audit.
  Default palettes follow macOS live; named palettes remain pinned. Compact
  layout, question docks and Plan overlays are covered at 720×520 by native
  tests; OS title-strip interactions and real-provider acceptance still need
  the manual checks above.
  Large-session projection is off-thread; no real-provider GUI latency benchmark
  or human visual acceptance is claimed.
- Tool input previews are capped at 24,000 characters; output initially shows
  16 lines and can be expanded in place. Parsed diffs are capped at 128 KiB,
  and add/delete diff previews at 1,000 source lines. Raw copy remains exact.
  Diff viewports follow the displayed line count, clamped to 120–420 px. Worker journals are host-loaded
  but individual worker panes are deliberately absent.
- Message-only stopped/failed partials are deliberately unsaved window state:
  restore cannot recover them. No supported Chat Completions `reasoning_content`
  adapter is added here. The display whitelist does not change the residual
  OpenAI canonical/replay `reasoning_texts` path, which is cache-prefix sensitive.
- Two known projection/transport limitations remain out of scope: Responses
  cancellation under channel backpressure can lose an already-read event, and
  ordinary ACP projection can place the answer before joined thoughts. This
  redesign does not change provider ordering or transport semantics.
- Kit's public test context does not expose its internal native
  `simulate_appearance_change` accessor. Automated tests exercise the exact
  appearance adapter; a real macOS preference change remains a manual check.
- Hosted macOS runner behavior and stable-toolchain GPUI builds remain subject
  to the shared CI run; record the toolchain and target for local validation.
