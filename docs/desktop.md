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
cargo run -p zevria-desktop --locked -- --help
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

The desktop's built-in default is **Zevria Graphite**: a warm graphite reading
plane, recessed wells for the sidebar, input and literal output, and raised
sheets for decisions. Build mint and Plan blue retain the TUI's mode families;
success is a quiet ✓, while live work, failures and denials carry color. A named
`[theme]` selection overrides the default in **both** frontends. The TUI default
remains Zevria Dark. One immutable palette is used per process, with Kit's light
or dark polarity following the palette, not the system appearance. There is no
automatic light/dark switching. See [themes](themes.md).

The signature is an **editor gutter**, not a stack of chat cards. Hanging speaker
labels appear once per turn; ○ ◐ ✓ ✗ ⊘ ◼ mark tool lifecycle at the same edge.
Tool inputs, output and diffs expand into recessed wells. The composer is the
next line on that same axis. Conversation prose stays within 600 px and tool
expansions within 720 px. At a main-column width below 760 px, speaker labels
move above the text and tool glyphs move inline. The sidebar auto-hides below a
900 px window; **⌃⌘S** or **View → Toggle Sidebar** overrides that choice.

For prose and controls, only macOS-bundled fonts are addressed: **SF Pro** (`.SystemUIFont`) for controls
and conversation, **Charter** for Plan documents and handoffs, and **Menlo** for
commands, paths, code and diffs. GPUI falls back when a font is unavailable. The
explicit type scale is 11, 12, 14, 16, 18, 21, 24 and 36 px, with Menlo's optical
13 px exception. Status numbers are tabular. Transparent native chrome joins the
40 px draggable title strip to the window's three depth planes.

The only ambient motion is the shared ◐◓◑◒ work glyph on the existing 250 ms
redraw clock. **Reduce Motion** freezes it at ◐ and makes Plan-sheet opening
instant. Math uses embedded KaTeX fonts with native system fallback for missing
glyphs; see [Math](#math). Icon controls have tooltips and accessible names; native controls retain
visible focus rings. Question and replacement sheets use a translucent scrim so
the conversation remains visible behind the decision.

### Screenshot gallery

The opt-in gallery renders the **real DesktopWindow** without a provider or
credentials, using macOS's text system and the Metal headless renderer:

```sh
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
cargo test --locked -p zevria-desktop --features fixtures --test gallery
# Optional output-directory override (absolute paths are easiest):
ZEVRIA_DESKTOP_GALLERY_DIR=/tmp/zevria-gallery \
cargo test --locked -p zevria-desktop --features fixtures --test gallery
```

PNGs are written to the repository's `target/desktop-gallery/`. The 1240×840 and
720×840 logical viewports are captured at the native test renderer's 2× scale.
The gallery covers configuration errors, no workspace, starting, stopping,
failed/idle/empty sessions, working prose and tool loops with an expanded native
diff, Plan review, questions, replacement confirmation and completions. Working
scenes also render under a generated light palette. The `math` scene restores
the shared Hölder proof fixture at 1240×3200 and 720×3200, plus a light-palette
1240×3200 capture. It contains all 15 inline and 11 display formulas, including
large delimiters, boxes and an aligned derivation with a 2 mm row gap.

For an opt-in, **read-only** capture of an existing session:

```sh
rtk proxy env CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  ZEVRIA_DESKTOP_GALLERY_SESSION=/absolute/path/to/session.jsonl \
  cargo test --locked -p zevria-desktop --features fixtures --test gallery
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
- Text, single-select, Other, multi-select and optional-skip questions. Native
  answers use strings; ACP multi-select uses arrays. Dismiss is available only
  when the request permits it. Terminal/close correlation cannot dismiss a newer
  unrelated question.
- Read-only subtask inspection and a breadcrumb back to the root. Child activity
  never takes over root busy state or root question ownership.
- Shared profile/reasoning/context/response/network status and persistence/Plan
  warnings; 250 ms clock updates run only while root work is pending/active.
- Exact primary/output copy values remain available even when display is bounded.

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
  Turn-continuation flags participate in fingerprints; switching width class
  remeasures every row so hanging labels and virtualized heights stay in sync.
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
rtk cargo check --locked --workspace --all-targets
rtk cargo build --locked -p zevria
rtk cargo test --locked --workspace
rtk cargo test --locked -p zevria-desktop --test markdown_math
rtk cargo test --locked -p zevria-desktop
rtk cargo test --locked -p zevria-desktop --features fixtures --test gallery
rtk cargo test --locked -p zevria --test crate_boundaries
rtk cargo test --locked -p zevria-provider -p zevria-host --features cache-diagnostics
rtk cargo clippy --locked -p zevria-desktop --all-targets --no-deps -- -D warnings
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

### Historical pre-merge validation ledger

The following records describe the original split and separate desktop, **not**
the merged workspace. Environment: macOS 27.0.1 arm64; rustc 1.100.0-nightly
(`fb6531d55`, 2026-08-23). Commands were run in their respective checkouts.

#### Baseline (before the original frontend changes)

- `cargo fmt --all -- --check`: passed.
- `cargo test --workspace`: **2,422 passed, 5 ignored**, 51 suites.
- Main working tree was clean; sibling desktop was an uncommitted Cargo scaffold.

#### Automated implementation checks

- After host rename: host + CLI tests **197 passed, 1 ignored**.
- Ratatui-free palette/adapters: theme/widgets/input/TUI/CLI tests
  **1,058 passed, 1 ignored**; no terminal dependency path from host.
- Module moves: presentation/input/TUI/CLI tests **957 passed, 1 ignored**.
- Shared reducer: **754 TUI tests passed**, without changing characterization
  assertions. New shared tests cover lifecycle, stale IDs, early rejection,
  Ready/mode behavior, accepted-edit ordering, mode correlation, transport fences,
  cancellation targets and tool-block revisions.
- Desktop debug and release builds succeeded. Desktop tests include pure routing,
  question rules, grammar, fingerprints, headless native input/buttons/Plan/questions,
  rejection recovery, close veto, palette/diff rendering, mixed Tokio/GPUI startup
  and active replacement/lease/resume, plus local-SSE host end-to-end coverage.
- Desktop `cargo clippy --all-targets -- -D warnings`: passed.
- Touched workspace crates `cargo clippy --no-deps ... --all-targets -- -D warnings`:
  passed. An attempted dependency-inclusive warnings-denied run also linted
  untouched responses/session-api/transcript code and stopped at existing
  large-enum/result-size/style lints; those unrelated APIs were not rewritten.

#### Final aggregate results before the merge

| Zevria workspace command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --locked --workspace --all-targets` | Passed |
| `cargo build -p zevria` | Passed |
| `cargo test --locked --workspace` | **2,434 passed, 5 ignored**, 53 suites (baseline +12 passes) |
| `cargo clippy --no-deps` for host, presentation, theme, tui-widgets, tui-input, tui, foundation and zevria, all targets, `-- -D warnings` | Passed, no warnings |
| `cargo test -p zevria-host --features cache-diagnostics` | **113 passed** |
| `cargo check -p zevria --features cache-diagnostics` | Passed |
| Host inverse dependency trees for ratatui/crossterm | Both packages absent, as intended |
| `git diff --check HEAD` | Passed |

The full workspace run included the unchanged core/provider prefix/request
regressions and the manifest-boundary tests. The additional host payload test
verified a one-time move while retaining shutdown/cleanup identity.

| Separate desktop package command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --locked --all-targets` | Passed |
| `cargo build --locked` | Passed |
| `cargo build --locked --release` | Passed |
| `cargo clippy --all-targets -- -D warnings` | Passed, no warnings |
| `cargo test --locked` | **23 passed**, 0 ignored, 5 suites |
| Inverse dependency trees for ratatui/crossterm | Both packages absent, as intended |
| Release `--help` | Printed documented usage and exited successfully |

The original desktop total comprised three CLI/row tests, eleven headless native
UI tests, six GPUI-free workspace/question/grammar tests, two mixed-runtime
lifecycle tests, and one local-SSE end-to-end test. Completion tests verified caret
placement as well as insertion/admission; draft tests verified that editing and
undoing did not revive an old replacement token. Ordinary transcript text was not
truncated by tool-preview display bounds.

At that historical validation point no commits had been created and changes
remained in the two working trees. The unmerged app-server branch was not checked
out or changed.

### Merged-workspace validation ledger

Validated on **2026-11-08 (local CST)**, macOS 27.0.1 arm64, rustc
1.100.0-nightly (`fb6531d55`, 2026-08-23). Cargo checks/builds/tests used `--locked`
and `--offline`, with `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`. The historical counts above are not evidence for
this merged build.

| Merged workspace check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --locked --workspace --all-targets` | Passed, no warnings |
| `cargo build --locked -p zevria` | Passed, no warnings |
| `cargo test --locked --workspace --no-fail-fast` | **2,492 passed, 0 failed, 5 ignored**, 59 suites |
| `cargo test --locked -p zevria-provider -p zevria-host --features cache-diagnostics` | Provider **269 passed, 2 ignored**; host **114 passed** |
| Five default-feature provider/core prefix gates from `release.yml`, individually listed and run | All passed; selected counts **1 / 2 / 1 / 1 / 1**, none empty |
| `cargo clippy --locked -p zevria-desktop --all-targets --no-deps -- -D warnings` | Passed, no warnings |
| `cargo check --locked --workspace --exclude zevria-desktop --all-targets` | Passed, no warnings |
| `cargo test --locked --workspace --exclude zevria-desktop --no-fail-fast` | **2,467 passed, 0 failed, 5 ignored**, 53 suites |
| `cargo build --locked --release -p zevria-desktop` | Passed, no warnings |
| `target/release/zevria-desktop --help` | Printed the documented `zevria-desktop` usage and exited successfully |
| Both workflow YAML syntax checks; `python3 tests/install/test_release_contract.py` | Passed; **2 contract tests passed** |
| `git diff --check` and `git diff --cached --check` | Passed |

The desktop contributes **25 passed tests** on macOS: the original 23 plus two
binary help/argument tests. The additional unsupported-platform binary test is
compiled/run only on non-macOS CI. All 11 headless native UI tests and both
real-I/O controller tests passed locally.

Dependency checks confirmed both desktop inverse terminal trees absent (expected
Cargo exit 101), **0 GPUI entries** in the CLI and desktop-excluded release
workspace trees, and only the **aws-lc-rs** rustls provider in desktop, CLI and
full-workspace native feature trees. None of the newly introduced package names
enters the CLI graph. The provider/core default-feature prefix gates passed
independently of the desktop's additive workspace features.

Offline lock review: **550 → 1,026 package entries**, **31 existing-version
replacements**, no package-name removals. The desktop subgraph adds 475 external
package identities plus the desktop itself, including parallel versions under
existing names. Newly introduced native desktop versions/checksums match the
source lock; taffy remains **0.15.0**, unicode-properties **0.1.3**, and the CLI's
tokio/serde_json/reqwest/rustls versions are retained. Seven offline-selected
versions differ from the source only outside the native macOS desktop graph:
font-types 0.12.6, open 5.4.4, yazi 0.2.1, zbus_names 4.3.4, zcheapstr 1.1.0,
zvariant 5.15.0 and zvariant_derive 5.15.0. No broad dependency update was run.

Native Linux/Windows execution, hosted-runner GPUI behavior, and stable-toolchain
GPUI compilation await shared CI. Local cross-target inspection was blocked by
uncached platform archives during Cargo's host-inclusive acquisition; no new
targets/toolchains were installed. No manual GUI or real-provider acceptance run
was performed, and the sibling checkout was left untouched.

### Gutter-led redesign validation

The redesign was validated with the low-disk Cargo environment documented above.
The historical ledgers are retained as history; the current results are:

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo test --locked -p zevria-theme` | 16 passed, including both built-ins and the color-literal guard |
| `cargo test --locked -p zevria-desktop` | 42 passed; all original flows and IDs retained |
| Desktop Clippy, all targets, `-D warnings` | Passed, both default features and `--features fixtures` |
| `cargo test --locked -p zevria --test crate_boundaries` | 3 passed |
| `cargo check --locked --workspace --all-targets` | Passed |
| `cargo test --locked --workspace` | 2,558 passed, 0 failed, 5 ignored, 59 suites; includes prompt/cache-prefix regression coverage |
| `cargo build --locked -p zevria` and CLI dependency tree | Passed; 0 GPUI entries |
| Release desktop build and `target/release/zevria-desktop --help` | Passed |
| Opt-in Metal gallery | 26 PNGs at both widths, including generated-light scenes |

Native control tests cover the 720×520 composer, compact mode commands,
round-trip wide/compact row remeasurement, rendered completion selection,
sidebar toggling, radios/Other/skip, and Reduce Motion. Gallery text/layout
analysis checked every render's dimensions and decision text, and caught and
corrected a Plan-body overflow into its footer. The critique pass also removed
a redundant file summary once the native diff header is visible. PNG generation
and automated text/layout checks are **not human visual acceptance**; the native
window interactions and real-provider checklist below remain manual.

### Native math validation before Git migration (2026-11-08, local CST)

The original vendored implementation was validated on native macOS arm64 with
the low-disk Cargo settings above. These historical results predate replacing
the workspace parser member with the pinned Git dependency; the parser tests
now live in the portable desktop integration target:

| Check | Result |
| --- | --- |
| Workspace fmt / locked all-target check | Passed |
| `cargo test --locked --workspace` | **2,724 passed, 0 failed, 16 ignored**, 62 suites; 11 ignored examples are from the newly vendored upstream parser |
| `cargo test --locked -p markdown` | **83 passed, 11 ignored** (52 upstream unit tests, 9 math contract/streaming tests, 22 doctests) |
| Published/patched parser comparison | All 14 off-switch golden cases and 45 tracked Markdown documents match upstream mdast in both math modes |
| `cargo test --locked -p zevria-desktop` | **63 passed**; includes 19 math tests, native select-all/copy, streaming, measurement without rasterization, and restored/Plan views at both widths |
| Desktop Clippy, all targets, `-D warnings` | Passed with default features and with `fixtures` |
| Release desktop build / `--help` | Passed |
| `crate_boundaries` | **3 passed**; first-party package inventory unchanged |
| Five default-feature provider/core prefix gates | All passed; independently discovered nonempty counts **1 / 2 / 1 / 1 / 1** |
| Cache-diagnostics provider / host tests | **271 passed, 2 ignored** / **122 passed** |
| CLI / Linux-desktop / Windows-desktop production dependency trees | **0 GPUI or RaTeX nodes** in each |
| Real Metal gallery, including opt-in source session | **31 PNGs**; all three math and both session captures have the full proof through its final paragraph |

The gallery's automated OCR/geometry sanity check caught zero-depth inline math
falling back to source when GPUI rounded its box below the explicit baseline.
Rounding the box height outward fixed it; the native fixture test now requires
all **26** formulas to allocate painted raster entries, not merely some of them.
Corrected captures have no raw TeX artifacts. Explicit black/black colorboxes,
straight BGRA tinting, representative ink signatures, inclusive resource limits,
Unicode fallback, LRU eviction and the typeset memo are covered by unit tests.

The source JSONL was read-only throughout, and its SHA-256 remains
`de8a2c0ebc0463d5ec59464db39c9f6de5400e1c6df0679de63baf88d71fe289`.
The checked-in fixture is byte-identical to the specified assistant output text;
no replay records or reasoning were copied. Parser compilers are byte-identical
to the published archive; the source patch is eight files (+497 / −6 lines).

**Not performed:** human visual approval of the PNGs, interactive real-provider
GUI scrolling/drag selection/resizing/sidebar/Plan acceptance, and native
Linux/Windows compilation. Dependency trees were checked for those platforms;
the existing CI remains responsible for cross-platform builds. Automated native
copy/selection and headless rendering tests are not a substitute for that manual
acceptance. No prompts, tool declarations/order, replay/persistence code or TUI
implementation changed.

### Pinned Git parser validation (2026-11-08, local CST)

The parser patch is published on `liukaizheng/markdown-rs` **main**, commit
`e629e7e4370cac5cc6dac39971c77feba3e3aaf9`, and root Cargo metadata/lockfile select
that exact Git source. The vendored tree is removed. No dependency versions
changed: only the Markdown source record and desktop's direct test dependency
changed in the lockfile. Zevria still uses Serde 1.0.228, with no SWC/test-tool
leakage from the fork. Its complete upstream library manifest and both parser
compilers remain byte-identical to the 1.0.0 upstream commit.

| Check against the published Git source | Result |
| --- | --- |
| Workspace fmt / locked all-target check | Passed |
| Workspace tests | **2,650 passed, 0 failed, 5 ignored**, 60 suites |
| Portable `markdown_math` integration target | **9 passed**, including the complete 15-inline / 11-display proof and incremental boundaries |
| Complete desktop package | **72 passed** |
| Desktop Clippy, all targets, `-D warnings` | Passed with default features and with `fixtures` |
| Release desktop build / `--help` | Passed |
| `crate_boundaries` | **3 passed** |
| Five default-feature provider/core prefix gates | All passed; discovered counts **1 / 2 / 1 / 1 / 1**, none empty |
| Cache-diagnostics provider / host | **271 passed, 2 ignored** / **122 passed** |
| CLI / Linux-desktop / Windows-desktop production trees | **0 GPUI or RaTeX nodes** in each |
| Metal gallery, including read-only opt-in session | **31 PNGs regenerated** |
| Fork's own local workspace validation | **207 passed, 11 ignored**; fmt and Clippy passed (existing upstream style warnings remain) |

The lower workspace test/ignore count reflects removing the third-party parser
as a workspace member; its upstream suite now runs in the fork. All nine
application-specific parser regressions remain in Zevria's portable desktop
test target. Validation used `--offline`/`--locked` and the low-disk settings;
an isolated Cargo home under `target/` avoided editor-owned shared-cache locks,
without changing global configuration or interrupting those processes.

The source session acquired newer appended records during this work. Its
original **six-record, 56,254-byte** snapshot is still an exact prefix with
SHA-256 `de8a2c0ebc0463d5ec59464db39c9f6de5400e1c6df0679de63baf88d71fe289`;
the specified proof output and checked-in fixture remain byte-identical. The
newer records were preserved, not truncated or rewritten to restore the old
whole-file hash. No application session data or local paths were published to
the fork, and Zevria's unrelated working-tree implementation changes remain
uncommitted. Human visual/manual GUI acceptance and native Linux/Windows builds
remain outside this local validation, as noted above.

## Manual acceptance (not performed)

Automated headless tests and the local SSE fixture **do not constitute a manual
real-provider GUI acceptance run**. Check these on a scratch workspace before a
release:

- [ ] First-run config error, Show config in Finder and Try again.
- [ ] Drag the title strip; double-click to zoom; check traffic-light placement.
- [ ] Toggle the sidebar with ⌃⌘S; resize to 720×520; inspect a generated light theme.
- [ ] Tab through controls with visible focus; test Reduce Motion on and off.
- [ ] Open workspace, new session, streamed Markdown and native command/edit rows.
- [ ] Cancel during a provider stream; enter another turn afterwards.
- [ ] Plan → Ready → Implement; repeat with Implement in fresh session.
- [ ] Text/select/Other/multi-select/optional questions; Answer and permitted Dismiss.
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
- No automatic light/dark switching, theme hot reload or full accessibility/
  keyboard-navigation parity audit. Compact layout is covered down to a 720 px
  window by native tests and the gallery; OS title-strip interactions, 720×520
  operation and real-provider acceptance still need the manual checks above.
  Large-session projection is off-thread; no real-provider GUI latency benchmark
  or human visual acceptance is claimed.
- Tool input/output previews are capped at 24,000 characters, parsed diffs at
  128 KiB, and add/delete previews at 1,000 source lines. Raw copy remains exact.
  Diff viewports follow the displayed line count, clamped to 120–420 px. Worker journals are host-loaded
  but individual worker panes are deliberately absent.
- Hosted macOS runner behavior and stable-toolchain GPUI builds remain subject
  to the shared CI run; local validation uses the toolchain recorded in the ledger.
