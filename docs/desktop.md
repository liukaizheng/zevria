# Native desktop implementation and validation

The desktop is the **separate sibling `zevria-app` package**, using path dependencies
on this checkout. Its README contains build/run instructions, supported controls,
troubleshooting and the full limitations list. GPUI is intentionally not a workspace
member or a dependency of the CLI/ACP build.

## Ownership map

| Owner | Responsibility |
| --- | --- |
| `zevria-host` (formerly `crates/app`) | Read-only configuration, production registries/provider/engine composition, skills, leases, supervisors, root-session discovery, transcript loading and ordered shutdown. Only this crate owns core. |
| `zevria-presentation` | Shared semantic blocks and native/ACP projection, lifecycle/telemetry, Plan workflow, command/skill/completion grammar, `SessionModel`, typed admission, restoration and live-tail derivation. No renderer, host, theme or engine dependency. |
| `zevria-theme` | Immutable semantic sRGB tokens and concrete user themes, unchanged OKLCH generation and contrast/CVD validation. Generic token groups map to renderer color types. |
| `zevria-tui-widgets::theme` | Thin, lazy Ratatui color adapter; preserves startup freeze and terminal call sites. |
| TUI `App` / `SessionViews` | Terminal editor/recall drafts, selection, viewport, folds, overlays and existing worker-review routing. `App` applies `Reduction` hooks instead of interpreting lifecycle events again. |
| Desktop `WorkspaceModel` | GPUI-free root/child/question routing and command metadata; each pane is the shared `SessionModel`. |
| Desktop `SessionController` | Starting/live/stopping/failed ownership, replacement, event/exit tasks, commands, desktop drafts, session catalog and graceful quit. |
| Desktop `ui/` | Native retained Markdown/plain text, tool/diff/image cards, virtualized list invalidation, composer, sidebar, Plan/question surfaces and status. |

There is no RPC or app-server layer. The unmerged out-of-process `zevria-app`
branch was not used or modified. No instruction text, tool registration order,
request construction, routing policy or transcript envelopes were changed.

## Important invariants

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
- The GPUI foreground task consumes the receiver directly, at most 64 updates per
  wake. Tokio owns the engine. The current presentation/restoration types are Send,
  so initial projection is off-main-thread. Markdown parser and diff completion
  notifications remeasure the corresponding retained list row.
- Normal close/Cmd-Q drops the pump/receiver and frontend endpoints before awaiting
  host engine/supervisor shutdown, then flushes the named log guard and quits.
  Replacement during startup is serialized too. A runtime exit does not permit
  starting another engine before the old stack has been cleaned up.

## Validation ledger

Environment: macOS 27.0.1 arm64; rustc 1.100.0-nightly
(`fb6531d55`, 2026-08-23). Commands were run in their respective checkouts.

### Baseline (before changes)

- `cargo fmt --all -- --check`: passed.
- `cargo test --workspace`: **2,422 passed, 5 ignored**, 51 suites.
- Main working tree was clean; sibling desktop was an uncommitted Cargo scaffold.

### Automated implementation checks

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

### Final aggregate results

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

The full workspace run includes the unchanged core/provider prefix/request
regressions and the manifest-boundary tests. The additional host payload test
verifies a one-time move while retaining shutdown/cleanup identity.

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

The desktop total comprises three CLI/row tests, eleven headless native UI tests,
six GPUI-free workspace/question/grammar tests, two mixed-runtime lifecycle tests,
and one local-SSE end-to-end test. Completion tests verify caret placement as well
as insertion/admission; draft tests verify that editing and undoing does not
revive an old replacement token. Ordinary transcript text is not truncated by
the tool-preview display bounds.

Inverse-tree absence intentionally produces Cargo's nonzero “package ID did not
match any packages” result. No commits were created; changes remain in the two
working trees. The unmerged app-server branch was not checked out or changed.

## Manual acceptance (not performed)

Automated headless tests and the local SSE fixture **do not constitute a manual
real-provider GUI acceptance run**. Check these on a scratch workspace before a
release:

- [ ] First-run config error, Reveal config and Retry.
- [ ] Open workspace, new session, streamed Markdown and native command/edit cards.
- [ ] Cancel during a provider stream; enter another turn afterwards.
- [ ] Plan → Ready → Implement; repeat with Implement in fresh session.
- [ ] Text/select/Other/multi-select/optional questions; Answer and permitted Dismiss.
- [ ] Launch and inspect a subtask; return to the root while it streams.
- [ ] Resume from the sidebar and `--continue` (including no prior root).
- [ ] Close/Cmd-Q while a turn is active, then resume cleanly.
- [ ] Scroll away during streaming, expand diffs/reasoning, copy input/output.

No external provider credentials were exercised for this acceptance checklist.

## Explicit limitations

The intended deferred scope remains: live ensemble-worker/review controls;
model/reasoning and skill-management UI; transcript recall/edit UI; attachment
input and `@` file mentions; multiple windows/tabs/concurrent sessions; `.app`
packaging/signing/notarization; Windows/Linux validation; theme hot reload; and a
full accessibility/keyboard-navigation audit. Root ensemble rows remain visible,
but restored Plan-ensemble worker confirmation still requires the TUI.

Additional implementation bounds are deliberate and visible: tool input/output previews
are capped at 24,000 characters, parsed diffs at 128 KiB and add/delete previews
at 1,000 source lines; exact copy is preserved. Diff views use a fixed-height
nested viewport. Very small windows and real-provider visual/latency behavior
have not been manually audited. OS-forced termination has only the short
best-effort quit fallback, not the normal ordered shutdown guarantee. Closing
while startup is still constructing the host waits for that construction before
shutdown; slow external credential commands can therefore delay close.
