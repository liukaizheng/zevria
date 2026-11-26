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

The native GUI builds on **macOS** and **x64 Windows (MSVC)**. Linux builds the
GPUI-free session model and a stub that directs you to the `zevria` CLI. A
configured provider is needed for real sessions, not for automated tests.

### Windows x64

Install a Rust toolchain with the `x86_64-pc-windows-msvc` target, the Microsoft
C++ build tools and a Windows SDK. From the repository root (PowerShell):

```powershell
cargo build --locked --release -p zevria-desktop --bin zevria-desktop --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\zevria-desktop.exe 'C:\Projects\My workspace 雪'
.\target\x86_64-pc-windows-msvc\release\zevria-desktop.exe .
.\target\x86_64-pc-windows-msvc\release\zevria-desktop.exe --continue
cargo run --locked --release -p zevria-desktop -- --help
```

Double-click `zevria-desktop.exe` or run it with no arguments to choose a
workspace. The extraction directory is **not** opened as a workspace. Pass a
path (including `.`) to open it directly. With `--continue` and no path, Zevria
looks for the latest session in the current directory. The app opens without an
extra console window. Its File, Session and View menus are inside the window.

[Git for Windows Bash and native RTK](windows.md) are prerequisites for **command
tools**, not for opening the GUI. Command-based credentials still need their
commands available in the launch environment.

### macOS

Install Xcode Command Line Tools and a Rust toolchain that can build the locked
workspace. From the repository root:

```sh
cargo run --locked --release -p zevria-desktop -- /absolute/path/to/workspace
cargo run --locked --release -p zevria-desktop -- /absolute/path/to/workspace --continue
```

Omitting `WORKSPACE` on macOS uses the current directory, except at a filesystem
root. No `.app` bundle is supplied. Launch from a terminal when command-based
credentials need its PATH.

### Arguments and configuration

Usage is `zevria-desktop [WORKSPACE] [--continue|-c]`. Use `--` before a workspace
whose name begins with `-`. `--help`/`-h` works everywhere. Select `-p zevria` for
the CLI or `-p zevria-desktop` for the desktop when building from source.

On first run, the desktop shows the shared Zevria config skeleton and setup
instructions. Use **Show config in File Explorer** on Windows or **Show config
in Finder** on macOS, fill in the generated files, then **Try again**. Config
and logs use the same home resolution as the CLI (`HOME` or, on Windows,
`USERPROFILE` by default). The default config is `~/.zevria/config.toml` and the
desktop log is `~/.zevria/logs/zevria-desktop.log`, unless overridden. The
configuration schema, provider setup, policies and transcript formats are shared
with the CLI. The old `zevria-app.log` is left alone.

### Workspace state

Desktop shares the CLI's `<workspace>/.zevria/` layout on Windows and macOS,
with `sessions/`, `subsessions/`, `plans/`, `agent-runs/`, and `ensemble-sessions/`
directly beneath it. Continue and resume do not look under the legacy
`.zevria/windows/` location. To reuse state there, follow the
[optional offline transfer](windows.md#optional-offline-transfer-of-legacy-windows-state).
The CLI's [`zevria clean`](architecture.md#offline-workspace-cleanup) purges the
five unified directories without removing the legacy subtree or project skills.
Stop all sessions and workers and back up any state you want to keep before
running it.

### Unsigned Windows development ZIP

With Python 3.12 and Git available, run `python tools/package-windows-desktop.py`
on Windows. It builds the same locked release-profile x64 MSVC executable,
packages only `zevria-desktop.exe`, then extracts it and verifies the x64 GUI
header and captured CLI probes with an isolated home. The ZIP is written to
`target/desktop-dev/zevria-desktop-dev-x86_64-pc-windows-msvc-<commit SHA>.zip`.
Local builds can include uncommitted changes; the SHA identifies the checkout.

For a CI build, manually dispatch **Cross-platform checks** with `desktop_dev_zip`
enabled. This runs only the Windows build/package job and uploads the ZIP to that
run's **Artifacts** section. Pushes and pull requests run tests and do not produce
a Desktop ZIP. The ZIP is **unsigned and for development testing only**: no
installer, published release or CLI distribution changes.

## Design language

### One working thread, three voices

The transcript is one left-aligned working thread within a centered column, not
message bubbles or a stack of cards. Its line and nodes record what happened,
in provider order:

- **Pencil:** a dashed thread and upright serif 13/21 in theme-derived pencil
  ink, quieter than answer text, for provider-supplied thinking. Expanded notes
  stay indented under the **Thinking** label. Thought previews are single-line
  plain text, whether the thought or its message is folded. Plan documents use
  the larger upright serif 15/24.
- **Carbon:** lifecycle nodes and monospace 12/18 for commands, paths, code, literal
  output and diffs. Expanded tools use recessed 8 px wells, with input before
  output and then the native diff. Output opens at 16 lines; **Show all N lines**
  expands in the transcript rather than creating another output scroll trap.
  Provider-hosted web work uses the same thread nodes, not a globe in the
  gutter. Adjacent actions in an entry share one row; expand it to see each
  search, page open or find in provider order.
- **Ink:** the solid thread and Kit's system UI font 14/22 for answers;
  prompts use the same size with medium weight and a hanging **You** label.
  You/Task, Zevria and System labels use their semantic role inks; the Plan
  label uses the Plan workflow ink. Lavender belongs to the Zevria answer label,
  not thoughts; answer headings use `content.heading`.

Build mint and Plan blue mark live work, mode controls and Send. A green ✓ means
success, ✗ means failure, ⊘ means denied and ◼ means stopped or interrupted.
Shapes and words always accompany state colors. Tool labels remain secondary
ink; shell syntax, not the label, carries color. Background tags use info ink
and appear only for commands started in or moved to the background; a live
foreground command snapshot alone never adds the tag.

The serif and mono families are Charter and Menlo on macOS, Georgia and
Consolas on Windows. Transcript typography is centralized as `Type { size, leading }` in
`ui/style.rs` (all values below are size/line height in pixels):

| Role | Size/leading | Use |
| --- | --- | --- |
| `READING` | 14/22 | Answers, prompts, empty-state and screen body text |
| `DOCUMENT` | 15/24 | Serif Plan documents, in the thread and review sheet |
| `PENCIL` | 13/21 | Serif thinking previews and expanded notes, upright by default |
| `CODE` | 12/18 | Monospace command headers, wells, code, web URLs, completions and errors |
| `SECONDARY` | 13/20 | In-thread notices, live tail, web queries and patterns, ensemble text, subtask titles |
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

The sidebar pins **This session** using its first prompt. That row returns to the
root conversation; subtasks are listed in the Subtasks panel instead. Earlier
sessions are single-line titles with aligned relative times. Arrow keys navigate
the rows; Enter opens one. Below a 900 px window the sidebar auto-hides; **⌃⌘S** on
macOS or **Ctrl+Shift+S** on Windows overrides it. Below a 760 px main column the
gutter labels move above content. A Plan sheet overlays instead of squeezing the
main column below 560 px. The minimum window is 720×520.

Questions **dock at the end of the thread in place of the composer**, without a
scrim. The transcript stays scrollable, and the dock scrolls internally above
55% of the window height (70% when the batch includes option details). Markdown
previews have their own scroll area, syntax highlighting, and a **Copy code**
action on code blocks. The preview sits beside the choices on wide layouts and
below them in compact layouts. Choose a row to follow it in the preview, or use
its **Preview** button to read without changing the answer. Drafts survive
answering/dismissing. Only the app-level **Stop the current turn?** replacement
confirmation remains modal.

### Thinking is supplied text, not an invented explanation

Only readable provider text creates a **Thinking** section. Empty,
encrypted-only, signature-only, redacted-only and unknown typed parts create
neither a heading nor placeholder thoughts. Anthropic `thinking_display =
"omitted"`, Responses `send_reasoning = false` and models without readable
summaries can therefore show no section. The UI does not infer missing thoughts.

- New and restored thinking starts **expanded**. A fold is explicit and shows
  the first non-empty supplied line (without Markdown styling) and a word count.
  In-place revisions to folded history retain their folds. A separate streaming
  preview is always open and has no disclosure until it becomes history.
- For message-only streams such as Anthropic, a correlated Stop or failure keeps
  the readable partial **in this window only**, labeled **Stopped, not saved to
  this session**. It is not engine history, is not persisted and is absent after
  restore. Responses interrupted attempts retain their existing persisted data.
- Stop is a neutral **◼ Stopped** node, not a red error. Incomplete attempts say
  **Stopped before the response finished** or **The response failed before it
  finished**. Actual failures remain red. Copy payloads are unchanged.

Thinking Markdown stays pencil-toned and upright by default, preserving authored
italics and bold. Headings are semibold at the pencil size, inline code and
unhighlighted code blocks use pencil on the panel, and only links retain
`content.link` for affordance. No extra vertical rule is added beside the
existing dashed thread.

In the TUI, reasoning Markdown inherits its
muted italic tone throughout, including bold titles, headings, inline/fenced
code and underlined links; ordinary answer Markdown keeps its accents.

Thinking disclosures are at least 28 px tall, focusable native Button
roles with an expanded state, the accessible name **Thinking**, and the preview
as their description. Enter and Space activate them. Copy and text selection
remain available; answer code fences also have a language label and Copy action.

The thread's live tip and the status bar share one vocabulary: **Waiting for the
model**, **Thinking**, **Writing**, **Running command** (or another tool name),
**Searching the web**, **Opening a page**, **Finding in page**, **Reconnecting**
with attempt/countdown, **Compacting context**, and **Stopping**. Opaque-only
progress cannot be called thinking, so it remains waiting.

### Web activity and Sources

A single action shows its label and summary; a group shows action counts and,
when terminal outcomes differ, colored outcome counts. Each expanded item has
its own status glyph. A confirmed interruption says **interrupted**; a closed
response without provider confirmation says **unconfirmed**. Running nodes use
the same Reduce Motion-aware clock as other work.

Expanded rows wrap queries, find patterns and page addresses. A folded row
keeps one line; a search with several queries shows the first and **+N more**.
Page addresses use link ink for the scheme-free host and muted mono for the
path, query and fragment. Safe HTTP(S) pages open by clicking the address or
using the focusable **Open page in browser** control. Hover shows the full
destination. Other addresses are plain text. **Copy web activity** preserves
the exact copy text of each member, one per line.

An expanded assistant answer with citations has a **Sources** footer, in
first-cited domain order. A single-page tag opens the page; a tag with a count
opens a menu of page-title links. **Show all N sources** reveals domains beyond
the first eight. The footer hides when the answer or message is folded; inline
links and answer copy are unchanged.

Grouping is desktop-only; the TUI keeps its existing rows. Groups keep their
first member's identity, but split where a bulk summary meets an expanded row
so that new activity remains visible. Sources come only from Responses
`url_citation` annotations (including indexed ACP display when present); no
provider result lists are requested. Host labels use lowercase ASCII/punycode
and omit a leading `www.`.

### Transcript folding and turn navigation

Folds are explicit and pane-local. The title strip's **Fold earlier content**
button is a one-shot action, not a reading mode. New and restored messages,
thoughts, and tool blocks start expanded. Finishing a turn, scrolling, or
switching panes never adds folds. A replacement transcript starts without
folds; inspecting another pane leaves the original pane's folds in place.
Folds do not edit the draft or affect what's saved or sent to the model.

| TUI | Desktop View action | Effect |
| --- | --- | --- |
| `za` | **Toggle fold** (or click/Enter/Space on the disclosure) | Toggle the focused message, block/item, or folded range. |
| `zc` / `zo` | **Fold** / **Unfold** (⌥⌘← / ⌥⌘→) | Fold the target or open the whole summary, preserving nested folds. |
| `zm` | **Fold earlier content** (⇧⌘R) | Preview prompts and summarize activity; keep each final output expanded. |
| `zM` | **Fold older turns** (⌥⇧⌘R) | Preview older prompts and answers with activity counts; keep only the last final output expanded. |
| `zR` | **Unfold all** (⌥⌘R) | Clear every fold in the current pane. |

Both bulk actions reopen the final outputs they protect, including a manually
folded answer or submitted Plan. **Fold earlier content** after **Fold older
turns** restores older answers without unfolding their activity. The composer
also accepts these shortcuts. **Expand all thinking** and **Collapse all
thinking** change just the individual Thinking folds, including in folded
messages.

Desktop and the TUI group folding by numbered prompt headers, including Plan
handoffs, job continuations, and ensemble prompts. Headerless entries join the
current turn, or form a leading turn if there's no prompt yet. A trailing
notice does not displace a final answer. If the last turn has no final output,
**Fold older turns** leaves no answer expanded. Compaction dividers split
ranges and stay visible. Hidden diagnostics aren't fold targets. Bulk folds
hide the current provisional bodies without hiding live progress or stop
controls; new snapshots and newly committed answers are not folded by them.

The chevron in the speaker gutter controls the whole message. A folded prompt
shows its first line, just like a local fold. Chevrons in the content column
control blocks, activity rows, and ranges. Adjacent folded activity uses one row,
such as **2 thoughts · 3 searches · 1 page** with a status glyph, even across the
boundary between earlier calls and the answer. An older turn's range previews
the answer's first line alongside muted counts, or just counts if it has no
final output. Web queries, page details, and cards stay hidden until opened.

Opening a summary restores all the content it represents without clearing
nested manual folds. If a fold covers the focused target or the reader's anchor,
that location moves to the visible representative. An expanded answer retains
its Sources chips. Copy uses the original content, not the summary or preview.

**Previous turn** (⌥⌘↑) goes to the nearest start strictly above the live viewport
top, or the current start if the top is mid-turn/mid-prompt. **Next turn** (⌥⌘↓)
goes to the nearest start strictly below. Exact-boundary presses skip that start;
there is no wraparound. These actions align the row top, detach tail following,
preserve all folds and leave composer draft, cursor and focus alone. Turn stops
covered by a folded range land on its summary, without duplicate stops.
**Jump to beginning** (⌥⇧⌘↑) targets row 0;
**Jump to latest** (⌥⇧⌘↓, formerly ⌥⌘↓) resumes following. The Kit Jump to latest
button is unchanged. The stops match TUI **[/]**, at Desktop entry-row granularity.

The live list anchor, rather than a saved destination, handles remeasurement,
streaming, width changes, folds and truncation splices; projection replacement
resets it. A destination within the final screenful cannot align to the top
without trailing blank extent: the list clamps at the end and may resume
following. Multiple ACP user groups within one entry share a stop. Input-context
bindings override Kit's ⌥⌘↑/↓ add-cursor shortcuts, so Desktop text fields give
up those multi-cursor bindings. Navigation is discoverable in menus and keys
only: no floating previous/next controls or sidebar turn list.

### Commands: display without routing, copy exactly

Both frontends share the renderer-free `zevria_presentation::shell` display rule.
At command positions (including after shell separators and inside substitutions),
`rtk`, its recognized global options and an optional `proxy`/`run` are hidden only
when another command word follows. Standalone `rtk`, `rtk --version`, argument
text such as `echo rtk`, `./rtk` and uppercase `RTK` remain unchanged. Quoted
strings, comments and heredoc bodies are not stripped. The lexer is tolerant of
incomplete/streaming input; it is a presentation heuristic, not a shell parser.

Desktop command headers, every TUI command row/fold header, and both frontends'
Jobs surfaces and notices use the stripped display. The TUI
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

### Jobs: inspect and control shell commands

The root status bar shows a **Jobs** chip when the session has jobs. It counts
and marks running jobs, and stays available in compact windows. Click it, use
**View → Toggle jobs panel**, or press `Shift+Cmd+J` on macOS (`Ctrl+Shift+Y` on
Windows) to toggle the panel. `/jobs` opens it even during a turn and consumes
only that slash-command draft. The panel starts closed and stays hidden during
Plan review. It docks on the right when there is room and overlays the thread
in narrower windows.

The panel selects the first running job on open. Click a row or use the arrow
keys to inspect another. Rows show the command, job ID, `fg`/`bg`, elapsed time,
and status. The detail shows bounded stdout/stderr tails with byte counts. It
follows new output while scrolled to the end; scroll up to read without jumping.
**Copy output** copies the retained tails.

On a running command's transcript row, **Stop command** stops just the job;
**Move command to background** releases a foreground call so the turn can
continue. Those controls are also available on the corresponding Jobs row on
hover or keyboard focus. Background command rows show elapsed time while the
job is running, and **Show in Jobs** opens a retained job in the inspector. A
restored command shows **Background** without implying a process is running.

**Background job** and **Background jobs** cards show finished command outcomes
and the output captured in the notice. New cards start expanded; restored cards
start collapsed. Click the header or use **Expand details** / **Collapse details**
to read the breakdown, and **Show all lines** to expand a long output preview.
**Copy notice** copies the exact text delivered to the model. A **Resumed** tag
means Zevria continued a turn after a background job finished.

In an expanded card, **Still running** means at least one listed job is active
now. On hover or keyboard focus, use **Show in Jobs** to inspect a retained row
or **Stop command** to stop an active job without cancelling the turn. When none
are active, **Also running at the time** shows how long the jobs had run when
the notice arrived, plus their final status if still retained. Background
processes do not persist across sessions, so restored notices never show live
controls or a ticking duration.

### Subtasks: switch between conversation panes

When the session has subtasks, the status bar shows a **Subtasks** chip in both
the conversation and child panes. It counts active subtasks; when all have
finished, the chip still opens the list. Click it, choose **View → Toggle subtasks
panel**, or press `Shift+Cmd+T` on macOS (`Ctrl+Shift+T` on Windows). `/subtasks`
opens the panel from the root composer even during a turn.

The panel docks on the right or overlays the thread in a compact window. It
replaces the Jobs panel and stays hidden during Plan review. Each row shows its
kind, title, status, and workspace when available. Click a row or focus it and
press Enter or Space to switch panes. Up/Down also switches panes. The panel stays
open while you read the child's transcript; select **Conversation** to return to
the root. Close the panel with Escape, the close button, or the same toggle.

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
thinking, tool and web headers expose their expanded state. GPUI Kit has no Increase Contrast signal,
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
Thinking-live, web-search-live, question-dock and Plan-overlay scenes also
have dark/light 720×520 minimum-window captures.
The model picker and conversion-warning scenes also have dark/light minimum-size
captures. To capture just model controls at 720×520, 1240×840 and 1920×1080 under
Graphite, Graphite Light and a generated configured theme, set
`ZEVRIA_DESKTOP_GALLERY_MODELS_ONLY=1` on the same gallery command.

The gallery covers configuration errors, no workspace, starting, stopping,
failed/idle/empty sessions, working prose and tool loops with an expanded native
diff, Plan review, questions, replacement confirmation and completions. Working
scenes, thinking-live, thinking-restored, thinking-interrupted, thinking-unsaved,
code-heavy answers, docked questions and Plan review also render under Graphite
Light. The `jobs` scene shows foreground and background commands, a finished
notice, and live stdout/stderr in the open panel. It renders docked at 1240 px
and as an overlay at 720 px, in both Graphite and Graphite Light. The
`web-search` and `web-search-live` scenes show grouped actions,
inline citations and Sources chips, and the running **Searching the web** tip.
They render in Graphite at both widths and in Graphite Light at 1240×840.
The `math` scene restores
the shared Hölder proof fixture at 1240×3200 and 720×3200, plus a light-palette
1240×3200 capture. The `transcript-folds` scene restores three turns, then
invokes **Fold earlier content**. Its 1240×1600 and 720×1600 Graphite captures
and 1240×1600 Graphite Light capture show the message and range summaries,
including two collapsed thoughts with the most recent answer still readable.
The math scene contains all 15 inline and 11 display formulas, including
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
`e629e7e4370cac5cc6dac39971c77feba3e3aaf9` by the root `[replace]` entry for
`markdown:1.0.0`.
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

- Open workspace, new session, Sessions sidebar and `--continue`. If there is no latest session, continue starts a new one with a
  notice. Replacing a busy session from the sidebar offers **Keep working** or
  **Stop and switch**.
- Live/restored transcripts, selectable user text, retained Markdown, collapsible
  reasoning, grouped web activity with Open and Copy, Sources tags and menus,
  native tool rows, structured task/reconciliation lists, command fences,
  file-change counts and expandable diffs, Plan artifacts
  and handoffs, validated prompt-image thumbnails, errors and compaction dividers.
- Multiline composer: **Enter** sends, **Shift-Enter** inserts a newline. Shared
  slash/skill completions support click, arrow selection, Tab and Enter. Built-in
  completion Enter executes the selected built-in outside transcript editing.
- Hover or tab to **Edit and resend** beside Copy on an earlier user message in
  the main transcript. The composer recalls the prompt and marks the message
  **Editing**. **Resend** replaces that message and all later turns when the
  engine accepts it; a rejection keeps the old history and your edited draft.
  **Cancel** or **Esc** restores your unsent draft and Orchestrate setting. While
  editing, mode and model changes and built-in commands are unavailable. In
  Build mode, use `/orchestrate <prompt>` in the recalled text to orchestrate.
  Any images on the recalled message stay attached unless you remove them.
- Build/Plan selection, next-Build-prompt Orchestrate subtasks switch, and Stop.
  Esc first belongs to an open model picker, question, review panel or completion. Cancellation
  targets the pending operation or the learned active turn, never a guessed ID.
- Plan review: Implement, Implement in fresh session, Revise / recovery Close,
  Copy plan as Markdown and reopen Review plan. Every decision includes the authoritative version.
  After Escape dismisses the sheet, Send submits ordinary feedback or `$skill` as
  a Plan revision of that reviewed version, never as approval or implementation.
  Rejection restores the draft and keeps review hidden. The decision shortcut
  can implement only while the review sheet is visible.
- Text, single-select, Other, multi-select and optional-skip questions. Text and
  single-select answers use strings; native and ACP multi-select answers use
  arrays. Dismiss is available only
  when the request permits it. Terminal/close correlation cannot dismiss a newer
  unrelated question.
- Read-only subtask inspection and a breadcrumb back to the root. Child activity
  never takes over root busy state or root question ownership.
- Session-only model/effort selection from the status control, the shortcut
  below or the Session menu. The model control stays visible and truncates long names in compact
  windows; its tooltip gives the full profile/effort, scope, shortcut and any blocker.
- Shared profile/reasoning/context/response/network status and persistence/Plan
  warnings; 250 ms clock updates run only while root work is pending/active.
- Exact primary/output copy values remain available even when display is bounded.

### Keyboard shortcuts

Windows shortcuts do not use the Fn row, dedicated paging keys, the Windows key,
or Ctrl+Alt. Most actions are also in the File, Session and View menus. Quit is
in the Zevria menu on both platforms.

| Action | macOS | Windows |
| --- | --- | --- |
| Open workspace / new session / quit | ⌘O / ⌘N / ⌘Q | `Ctrl+O` / `Ctrl+N` / `Ctrl+Q` |
| Sidebar / switch mode / select model | ⌃⌘S / ⇧⌘M / ⌃⌘M | `Ctrl+Shift+S` / `Ctrl+Shift+M` / `Ctrl+Shift+P` |
| Toggle Jobs / Subtasks panel | ⇧⌘J / ⇧⌘T | `Ctrl+Shift+Y` / `Ctrl+Shift+T` |
| Stop turn / confirm visible decision / copy focused disclosure | ⌘. / ⌘↩ / ⌘C | `Ctrl+.` / `Ctrl+Enter` / `Ctrl+C` |
| Previous / next turn | ⌥⌘↑ / ⌥⌘↓ | `Ctrl+K` / `Ctrl+J` |
| Jump to beginning / latest | ⌥⇧⌘↑ / ⌥⇧⌘↓ | `Ctrl+Shift+K` / `Ctrl+Shift+J` |
| Fold / unfold focused item | ⌥⌘← / ⌥⌘→ | `Ctrl+Shift+F` / `Ctrl+Shift+U` |
| Collapse / expand thinking | ⌥⇧⌘← / ⌥⇧⌘→ | `Ctrl+Shift+H` / `Ctrl+Shift+E` |
| Fold earlier content / fold older turns / unfold all | ⇧⌘R / ⌥⇧⌘R / ⌥⌘R | `Ctrl+Shift+R` / `Ctrl+Shift+B` / `Ctrl+Shift+A` |

Enter sends a draft, Shift+Enter adds a line, and Shift+Tab remains reverse focus
traversal. Confirm sends the open question answer or implements the open Plan;
it does not send an ordinary composer draft. Standard text editing still works
in inputs. Esc belongs first to an open model picker, replacement confirmation,
dismissible question, completion, Plan sheet or Subtasks panel. Otherwise Esc
cancels a recall or stops an edit that is waiting for acceptance. Stop still
cancels the active root turn, including while a non-dismissible question is open.

Supported shared grammar: ordinary text, `/orchestrate <prompt>`, `$skill [args]`,
`/compact`, `/implement`, `/implement-fresh`, `/plan`, `/build`, `/new`, `/resume`,
`/model`, `/model-session`, `/jobs`, `/subtasks`. Skill-management, worker-review and `/ensemble-*` commands retain
the draft and show an explicit not-yet-available notice. Input while busy is not
queued as another turn. A submitted draft stays until acceptance; failures and
cancellation restore it without replacing a newer draft (Recover draft keeps
both when necessary).

### Model and thinking-effort sheet

The status control, shortcut and Session-menu item open the same **session-only**
native sheet. `/model-session` has that scope too; `/model` instead saves the
selected mode's global default. The mode is captured on open: selecting while in
Plan changes Plan, not Build, Review or child assignments. Both session-owned
selections survive `/new`, fresh implementation and resume. Independent sessions
still start from configuration defaults.

The sheet uses the existing theme-aware sheet/scrim, system UI type and focus
styles. Search filters configured profiles, the current model/effort is explicit,
and only supported effort choices appear. Arrow keys and Enter select a model,
then an effort; Apply confirms it. Tab/Shift-Tab traverse native controls. A real
history conversion has its own explanation and Convert/Cancel confirmation.
Loading, applying, cancelling, empty and error states stay in the same sheet.
Escape cancels the picker before completion or turn cancellation and focus returns
to the previous valid control on authoritative settlement. Cancellation racing an
already-committed update still installs that acknowledged selection.

Model selection is idle-only, never queued behind an active turn. Only a live,
writable root with no pending operation, Plan approval (including a hidden Ready
Plan), question or capturing dialog can open it. Conflicting submission, mode and
replacement controls are blocked while it is open. A shortcut/click leaves the
draft untouched; an accepted slash open consumes only its command, while rejected
opens retain it. Management never becomes submitted-turn bookkeeping or a
"waiting for model" conversation row.

Changing only effort with an unchanged exact profile/context policy changes
metadata/request properties, not conversation content. Thinking disclosures,
block IDs, projection epoch, scroll and context/response/cache telemetry remain
intact, as do native reasoning/signatures/encrypted payloads, tool records and
opaque checkpoints. There is no count, conversion, summary, compaction or socket
reset during this operation—even if history is already over limit. Ordinary
next-turn capacity admission remains unchanged. The first changed-effort request
may need full native replay instead of `previous_response_id`; later continuation
recovers normally, using the same instructions, tools and cache key.

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
| Desktop `WorkspaceModel` | GPUI-free root/child/question routing, shared correlated model-picker lifecycle and command metadata; each pane is the shared `SessionModel`. |
| Desktop `SessionController` | Starting/live/stopping/failed ownership, replacement, event/exit tasks, commands, desktop drafts, session catalog and graceful quit. |
| Desktop `ui/math/` | Source-faithful math plugins, bounded in-process em layout, direct KaTeX rasterization and evictable images; no provider/persistence dependencies. |
| Desktop `ui/` | Native retained Markdown/plain text, gutter-led tool/diff/image rows, virtualized list invalidation, composer, sidebar, Plan/question surfaces and status. |

`zevria-host` was previously the workspace's `zevria-app` package; it is not the
renamed desktop. The desktop is a leaf workspace frontend: no package may depend
on it, it cannot reach any TUI crate, and it has no direct Ratatui/Crossterm
production dependency, including target-specific dependencies. The manifest-boundary
tests discover all `crates/*` automatically and enforce these rules. Neither the
host nor desktop has a Ratatui/Crossterm dependency path.

The CLI's normal dependency graph and the Linux desktop stub contain no GPUI or
RaTeX. Desktop's native GPUI Kit, math and logging dependencies are gated to
macOS and Windows. The desktop's test-only Markdown dependency allows portable
parser regressions without adding a renderer to other targets. Native
`--workspace` builds can unify additive features such as extra image codecs.
Cross-platform CI runs a desktop release-profile check and tests; CLI release
check/test jobs still exclude `zevria-desktop` to retain the shipping graph. Workspace CI disables incremental
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
- Steps keys are `{epoch}-steps-{start_row}` and belong to a separate pane-local
  `DisclosureStore`, never the thinking/tool store. Truncations retaining a run's
  start retain its choice; projection replacement and either mode toggle retire
  choices. Mode survives replacement without a new config/persistence schema.
  Runs/counts are rebuilt for history, busy/call or mode changes, never tail-only
  deltas. Every block consumer uses the same presented-block accessor.
- Hidden entries remain logical list items with zero height. Inter-row spacing
  is row-local: Kit's wrapper `pb_8` is disabled, and non-final, non-hidden rows
  use `pb(rems(2.))`, preserving the ordinary gap without hidden-row spacing.
  Role changes remeasure affected rows; summary choices remeasure their run.
  Navigation reads `logical_scroll_top()` and calls `scroll_to_item`, never
  reconciles stored turn targets or invents within-row ACP stops.
- Desktop styling never enters request construction, tool registration, prompts
  or transcript envelopes. The provider's cacheable prompt prefix is unchanged.

### Shutdown

The caption close button, OS close request, Quit menu item and Quit shortcut
(Cmd+Q or Ctrl+Q) do **not** quit immediately. The controller stops the pump
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

Run from the workspace root on native Windows and macOS:

```sh
cargo fmt --all -- --check
cargo check --locked --release -p zevria-desktop --all-targets --features fixtures
cargo test --locked --release -p zevria-desktop
cargo tree --locked -p zevria --edges normal --prefix none
cargo tree --locked -p zevria-desktop --target x86_64-unknown-linux-gnu --edges normal --prefix none
cargo tree --locked -p zevria-desktop -e features -i rustls
```

The CLI and Linux Desktop stub normal trees must have no `gpui*` or `ratex*`
packages. On Linux, the desktop binary tests also check that the stub recommends
the CLI. Native rustls must enable only `aws-lc-rs`, not `ring`; recheck
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
synthetic events and a command channel. Windows and macOS run the ordinary GPUI
suites on GPUI's simulated platform. The opt-in [screenshot gallery](#screenshot-gallery)
is macOS-only because it needs the Metal headless renderer. Linux runs the
GPUI-free routing, parser, engine fixture and stub tests. Broad Cargo dependency
fetches can still download other targets' archives without compiling them.

### Focused regression coverage

Native control tests cover live/restored/interrupted thinking, retained row and
pane identity, nested explicit folds, keyboard controls and question ownership.
Layout/focus regressions check the centered column and composer axis, sidebar
and width changes, blank-click blur/refocus, and Plan Copy in overlay and docked
panels. Compact-window tests cover completion selection, radios/Other/skip,
Reduce Motion and row remeasurement.

Parser coverage includes disabled-mode goldens, the complete inline/display
math proof and incremental parsing boundaries. Native math tests cover source
copying, raster allocation, baseline rounding, Unicode fallback, resource limits,
LRU eviction and typeset memoization. The active parser pin, provenance and
maintenance obligations are recorded in [Desktop Markdown parser fork](markdown-parser-patch.md).

Headless controls, local-SSE fixtures and Metal gallery captures do not prove
native window launch, human visual acceptance or a configured GUI session.

## Interactive acceptance checklists

### Windows x64 development ZIP

- [ ] Extract the ZIP, then double-click the executable. Check that no console
  opens and that the extraction directory isn't selected as a workspace.
- [ ] With a disposable home, verify **Show config in File Explorer**, then
  **Try again**. Open a folder containing spaces and non-ASCII characters with
  the picker and from a terminal.
- [ ] Inspect system UI, monospace and serif text; minimize, maximize, restore
  and close using the caption controls. Open the File/Session/View/Zevria menus.
- [ ] On a real keyboard layout, try the shortcut table (including `Ctrl+.`),
  the focused question editor and ordinary copy/paste/undo/select all. Verify
  that turn navigation leaves the draft, caret and composer focus alone.
- [ ] On a disposable configured workspace, submit and restore a session via
  the sidebar and `--continue`. Close it with the caption button and with
  `Ctrl+Q` (no Fn), and check that both shut down cleanly. The credential-free
  `engine_e2e` test covers submit/resume independently of GUI acceptance.

### macOS

Check these on a scratch workspace:

- [ ] First-run config error, Show config in Finder and Try again.
- [ ] Drag the title strip; double-click to zoom; check traffic-light placement.
- [ ] Maximize and full screen keep one centered axis.
- [ ] Click a chevron, then empty space: the ring clears and ⇧⌘M still works.
- [ ] Toggle the sidebar with ⌃⌘S; resize to 720×520; inspect Graphite Light and
  a generated named theme. Change macOS appearance while streaming; the default
  pair must switch live and a named theme must stay pinned.
- [ ] Keyboard-only pass: sidebar arrows/Enter; Tab and reverse Shift-Tab;
  Enter/Space on message, block, and range disclosures; ⌥⌘→/←; ⇧⌘R; ⌥⇧⌘R; ⌥⌘R;
  ⌥⌘↑/↓ from the focused composer; ⌥⇧⌘↑/↓; ⇧⌘M; ⌘↩ decisions.
  Check every visible focus ring, including virtualized and narrow-window rows.
- [ ] Open model selection from the status control, ⌃⌘M with composer/search focus,
  and Session menu. At 720×520 and wide sizes inspect long model names, current
  effort, search/effort navigation, wrapped conversion warnings, partial-save
  errors, loading/cancelling states, Tab containment and Escape/focus restoration
  in light, dark and a configured theme. Compare nonempty thinking disclosures,
  scroll, draft and telemetry before/after effort-only Apply; verify no model turn.
- [ ] VoiceOver spot check: thinking, message, block, and range disclosures
  announce meaningful Button names and expanded state. Thinking includes the
  supplied preview description. The title button announces Fold earlier content.
- [ ] Test Reduce Motion on and off: the live tip stays at ◐ and Plan opening is
  instant when enabled.
- [ ] Open workspace (including ⌘O on the no-workspace screen), new session,
  streamed Markdown and native command/edit rows.
- [ ] Real Anthropic session: the render-only tail stays expanded through
  deltas and commit. Stop mid-thinking keeps the marked **unsaved** partial
  before a neutral Stopped node. Restore and confirm it is absent.
- [ ] Real Responses session: live summary thinking is visible, not Waiting.
  Fold a projected history thought, then verify it stays folded on revision.
  Stop mid-thinking and restore the saved interrupted output: it starts open.
  Test a model without readable thoughts; expect no empty Thinking header.
- [ ] Real Responses session with web search enabled: watch the live tip as web
  work runs, fold a growing group and verify it stays folded. Expand it and open
  an address with both the text link and **Open page in browser**. Verify Sources
  tags open single pages and a multi-page dropdown opens the chosen citation.
  Escape closes the dropdown without stopping the turn. Check the running node
  with Reduce Motion on and off, and verify keyboard focus and VoiceOver names
  for **Web activity details**, **Copy web activity**, Open and the source tags.
- [ ] Complete a turn while scrolled up. Nothing folds or jumps; Jump to latest
  and another turn don't create new folds. Check that both bulk actions keep the
  newest answer readable and fold only its Thinking blocks.
- [ ] Fold with focus or the top anchor inside an earlier range. Verify the
  summary takes focus and the anchor, then reopen it without losing inner
  folds or the reader's place. Check turn jumps and full copy payloads.
- [ ] Inspect a child, use Unfold all, and return to a folded root. Replace or
  resume the root: the new transcript opens expanded and the draft is unchanged
  by fold actions.
- [ ] Plan → Ready → Implement; repeat with Implement in fresh session.
- [ ] Text/select/Other/multi-select/optional questions in the dock: transcript
  scroll stays available, ⌘↩ sends, Esc only dismisses when allowed, and the
  composer's draft returns unchanged. At 720×520 inspect the Plan overlay and
  question dock without obscured controls.
- [ ] Launch subtasks and check the live status-bar count. Open the panel via
  the chip, shortcut and `/subtasks`; switch panes with the arrows or a click,
  then return to **Conversation** while the child streams. Check that Escape
  closes the panel and that no child rows appear in the sidebar.
- [ ] Resume from the sidebar and `--continue` (including no prior root).
- [ ] Close/Cmd-Q while a turn is active, then resume cleanly.
- [ ] Scroll away during streaming, expand diffs/reasoning, copy input/output.
- [ ] Open the Hölder-proof session from the sidebar; scroll, select/Cmd-A/copy,
  drag across prose and atomic formulas, resize to compact, and check the Plan
  panel. Inspect math baselines, SF Pro/Charter sizing, display centering,
  aligned 2 mm row gaps, boxes, large delimiters and narrow horizontal scrolling.

No external provider credentials were exercised for this acceptance checklist.

## Troubleshooting

- **Config skeleton/error:** use Show config in File Explorer or Finder, then
  Try again; provider/model errors are not desktop-specific.
- **Engine stopped:** inspect `zevria-desktop.log`, then Start new session. Startup
  and cleanup errors remain visible; session replacements do not overlap engines.
- **Build/test-support resolution:** keep the root `Cargo.lock` and use `--locked`.
  When intentionally changing Kit features, resolve from the workspace root rather
  than hand-patching transitive versions. The validated graph retains taffy 0.15.0
  and unicode-properties 0.1.3 alongside Kit's test-support dependencies.
- **Disk pressure on native builds:** workspace builds add GPUI artifacts. Disable
  dev/test debuginfo and incremental compilation as above; use `-p zevria` when
  only working on the CLI. Rust-analyzer also indexes/builds the native dependency.
- **Key-command environment:** launch via the terminal and check the command's
  PATH; credentials are still resolved exclusively by the host/provider layers.
- **Unsupported platform:** Linux has only the GPUI-free desktop stub. Use
  `zevria`. Native Windows ARM64 and Linux GUI are outside this scope.

## Explicit limitations

- Final outputs mode is window-local, not persisted across launches. The
  final-response heuristic is based on visible assistant roles; unusual mixed-role
  ACP entries group coarsely. Multiple user groups in one entry share one stop.
  Navigation aligns rows, not positions inside a row. The final screenful clamps
  rather than adding trailing blank extent and may resume tail following.
- Kit's ⌥⌘↑/↓ multi-cursor shortcuts are overridden in Desktop inputs. Jump to
  latest moved to ⌥⇧⌘↓. Turn-end automatic fold flushing has no Window access,
  so it cannot rescue focus from a live step folded at that point; explicit
  collapses and mode changes do rescue it. Real-provider, VoiceOver and native
  keyboard-only checks above remain manual acceptance, not gallery claims.
- Math is KaTeX-compatible, not a TeX engine: unsupported commands show source,
  not an approximation. Only the screenshot gallery requires macOS and Metal. Raster
  overhang, device dimensions and caches have the budgets listed above; Unicode
  fallback metrics and grayscale antialiasing can differ from surrounding text.
- No live ensemble-worker panes or worker confirmation/review/abandon controls.
  `/ensemble-plan` and `/ensemble-review` are disabled. Restored unfinished Plan
  ensembles can be observed through root rows and cancelled, but confirmation
  still requires the TUI; see [ensemble workflows](ensemble.md).
- No model/reasoning picker or skill-management UI (identity/status and startup
  skill completions only). Unsupported built-ins are explicitly rejected locally.
- No new image attachment input or `@` file mentions. Restored prompt images
  display in the transcript and can be kept or removed when editing a message.
- One window and one live session; no tabs or concurrent-session UI.
- No `.app` packaging, signing or notarization. Windows distribution is an
  unsigned development ZIP only; neither a Desktop installer nor a release asset.
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
