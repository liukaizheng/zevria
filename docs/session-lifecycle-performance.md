# Session lifecycle performance (2026-11-11)

The original lifecycle optimization at `6f870e4` implemented its phases 0–5 and
deferred its optional background reconnect. The follow-up **Session Lifecycle
Performance and Structure Plan** is separate: its Phase 0 measurements,
unconditional Phase 1 steps, and completed Phases 2–6 are recorded below.
Conditional step 1.5 still requires new evidence before activation.
The older transcript and prompt-admission reports describe different historical
boundaries; their numbers are not updated or reinterpreted here.

**Implementation checkpoint — approved plan
`cfd10bef-34a9-4105-8a3a-c28848079133@1`:** Phase 0 (0.1–0.5) and Phase 1's
unconditional steps (1.1–1.4, 1.6–1.7) are implemented. Conditional step 1.5 is
not activated without new HTTP idle/start evidence. **All requested Phases 2–6
are implemented and release-validated**, one numbered step per commit.
`Conversation` is the sole replay owner; core shares request plans/accounting,
uses explicit workflows, and publishes completed usage after persistence.
Provider, transcript and host boundaries are split, with borrowed Responses
serialization, shared Anthropic projection and validated resume reuse.
Real-session Phase 1 latency/cache/continuation acceptance remains pending.

## Implemented boundaries

- **Instrumentation and batching guidance:** lifecycle timings log `operation`,
  `records`, and `elapsed_us` under `zevria_core::lifecycle`. A single application
  guidance sentence asks for independent inspection calls in the same response.
  Timing, identities, retries, and cache bookkeeping never enter model input.
- **Append persistence:** multiple records share one versioned
  `{"zevria_batch":{"version":1,"records":[...]}}` physical JSONL line and one
  successful `sync_data`. Loading flattens the batch while retaining each item's
  original physical source line. Old individual records remain readable.
  Unknown versions, nested batches, invalid completed children, and batched
  session headers are rejected rather than repaired away.
- **Failure boundaries:** required append failures discard prospective memory;
  completed external work remains in memory and degrades persistence until
  repair. Failed writes/syncs attempt a truncation rollback. If storage also
  rejects rollback, the error explicitly reports **indeterminate durability**;
  it is not possible to promise that rejected bytes did not survive that double
  storage failure. A recognized torn final record restores no batch children.
  Tail repair truncates to its verified byte offset, preserving all earlier
  physical envelopes verbatim. Edits, header changes, initial publication, and
  degraded-persistence repair retain staged full rewrites.
- **Suffix replay:** validated instruction, Plan, and Ensemble state is retained
  across appends. Rejected/invalid evidence takes the independent full-validation
  fallback, preserving Skills/Plan/Both error precedence. Ensemble snapshots use
  copy-on-write run state; ordinary appends do not copy old review journals.
- **Dispatch:** cached policy preparation owns stable instructions, tools,
  serialized tool bytes, and request shape. Its key includes policy, application,
  and model context. Existing guidance/catalog/tool-window/history invalidations
  clear it. The provider borrows conversation input while a disjoint writer
  persists display-only failed-attempt checkpoints; core installs those records
  after the request borrow ends.
- **Responses projection and capacity:** a running local hash chain identifies
  input boundaries. Projection and capacity preflight reuse validated prefix
  correlation state and process only the suffix. A future native tool ID that
  collides with an already minted portable ID forces a rebuild and full replay,
  never an incorrect `previous_response_id` continuation. Compatibility, profile,
  edit, and compaction boundaries invalidate incompatible state. Input identities
  are local only, and synthetic callers can omit them.
- **Streaming:** snapshot construction is gated at 30 ms with dirty, quiet-tail,
  terminal, and error flushing. The pending SSE read remains pinned across timer
  ticks. Frames are parsed into JSON once and shared with attempt reduction,
  native-output capture, diagnostics, and typed-event decoding. A drop-time
  attempt collector retains the latest received evidence on cancellation without
  per-delta snapshot clones or retaining frontend event senders.
- **Resume:** root and worker startup load/validate and prepare the writer on
  blocking workers. The immutable load report carries its validated replay into
  writer/engine construction without reopening or replaying the history again.
  A regression probe asserts one load and one authoritative validation through
  report → writer → engine. The frontend moves its restoration payload instead
  of making another whole-history clone.

## Cache-prefix release gates

Except for the deliberately added application-guidance sentence, this change
adds no instruction bytes or model-visible lifecycle metadata. The ordinary
prepared-request fixture is semantically unchanged; its incidental JSON-key-order
regeneration was removed. The host application-prompt fixture is deliberately
updated. Independent instruction-byte, continuation, reconnect, resume,
compatibility-role, directive, incremental-tool, retry, and cache-diagnostics
fixtures exercise the real wire boundaries.

A local lineage is a trusted engine optimization, not an alternate source of
conversation truth. `ModelRequest.input_identity` must describe that exact input;
callers without a maintained lineage use `None`.

## Structure plan implementation (2026-11-11)

The follow-up Session Lifecycle Performance and Structure Plan was implemented
one numbered step per commit. Each section records its boundary and verification;
the final checkpoint distinguishes offline completion from live acceptance.

### 0.1 — Provider request telemetry

OpenAI terminal response logs now include `request_ms`, `first_event_ms` and
`function_calls`; Anthropic terminal logs include the same timing fields and
`tool_uses`. Timings start at each transport attempt, excluding connection
establishment and preparation, and include request send/headers and response
streaming. `first_event_ms=None` means no nonempty response event was observed
(for example, an HTTP status rejection); it is not a zero-latency response.
Calls are counted from terminal native output, falling back to deduplicated
completed output indices when a gateway omits the terminal array. Anthropic
counts replayable calls after any fallback boundary. WebSocket connection logs
include `connect_ms`, including credential resolution and handshake time.
These are local log fields only: instructions, tools and request bodies are
unchanged.

### 0.2 — One model-round summary

Each dispatched model call emits one INFO `model round summary` event under
`zevria_core::round`, with `turn`, `call`, `items`, `prepare_us`, `model_ms`,
`commit_response_us`, `tool_calls`, `tools_ms` and `commit_results_us`.
Preparation includes admission assessment/counting and any pre-dispatch
compaction; model time includes the provider call and checkpoint pump. Response
commit includes settling attempts, durable response admission and its existing
publications; result commit includes assembling/admitting results and their
publications. Thus these are elapsed phase measurements, not CPU-only timings
or additive measurements of fsync alone. Final-only rounds have zero tool and
result-commit time. Cancellation/failure accounts for the last active phase;
a preparation rejected before dispatch does not invent a model-call summary.
Detailed `zevria_core::lifecycle` operation timings remain available at DEBUG.
No writes, cancellation checks or event publications were reordered.

### 0.3 — Reproducible offline analysis

`tools/lifecycle-stats.py` uses only standard-library Python and reads inputs
without repairing or rewriting them. It emits JSON aggregates, never prompt,
argument, image or response content. Both files are read to a size snapshot;
active logs/transcripts can therefore grow without producing an unbounded scan.

```sh
rtk proxy python3 tools/lifecycle-stats.py ~/.zevria/logs/zevria.log .zevria > /tmp/lifecycle-stats.json
rtk proxy python3 tools/lifecycle-stats.py ~/.zevria/logs/zevria.log .zevria/sessions --end 2026-11-11T00:00:00Z --max-gap-seconds 60
rtk proxy python3 -m unittest discover -s tools -p 'test_lifecycle_stats.py'
```

- Log parsing recognizes exact tracing messages and scans quoted/escaped
  `key=value`, `Some(...)` and `None` fields. The most recent outstanding request
  is paired with its terminal by non-null `prompt_cache_key_fingerprint`.
  Superseded/unmatched attempts and missing fingerprints are counted explicitly.
- Outputs include duration percentiles by output size, short-output duration by
  input size, full/incremental duration and uncached input, first-event/connect
  timings when present, global invalidation reasons, idle before replay,
  token-weighted cached-input fractions, tool-call histograms and round phases.
  Anthropic profiles use `input_tokens + cache_creation + cache_read` as the
  input denominator; old logs cannot supply its missing request timings.
- A gap is a completed response to the next request with the same fingerprint.
  It includes tools, storage, scheduling **and user idle**. Every gap is reported;
  the aggregate time and history-size table use a labeled cutoff of 300 seconds
  by default (`--max-gap-seconds 0` includes all). History bands use the next
  request's `history_message_count`, not persisted record count. Percentiles use
  linear interpolation at `(n - 1) * p`. Summed time across sessions is not
  elapsed wall time. `--start` is inclusive and `--end` exclusive, affecting logs
  only. The original one-off probe's unavailable filtering cannot be recreated
  by claiming a new snapshot reproduces its exact totals.
- Historical invalidation rows lack fingerprints, so reasons are **global**;
  the tool never attributes them to a nearby session. Anthropic terminal logs
  identify profiles, not sessions. Absent timings/call telemetry remain unknown.
- Transcript discovery accepts `.zevria`, `sessions/` (also finding sibling
  `subsessions/*/`), or a supplied transcript directory. Version-1 batches are
  flattened once; nested batches, batched headers and unsupported versions are
  rejected. A malformed/torn batch contributes no children. Only top-level native
  replay items count as responses, and only immediate `function_call`/`tool_use`
  items count as calls. Compaction replacements, argument lookalikes, hosted
  tools and generic synthetic assistant messages are not recursively counted.
  This is a statistics reader, not a substitute for Rust replay validation.

The synthetic Python tests cover interleaved lineages, cutoffs, token weighting,
old/new fields, exact message matching, date boundaries, batches, torn tails,
header rejection, final records without newlines and read-only inputs.

### 0.4 — Complete engine-round benchmark

The `engine_round/{100,1000,5000}` Criterion group submits one ordinary Build
prompt through `SessionEngine::handle_command`, dispatches a scripted local
provider with one function call, executes a trivial in-process echo tool, then
dispatches and commits the final answer. Initial counts are exact live record
counts, including headers and reconciled policy scaffolding. Histories contain
128-byte replay-heavy synthetic records and use one matching profile with ample
capacity, avoiding compaction, shell work and external requests.

Construction, initial durable publication, replay, policy priming and fixture
destruction are outside timing. Prompt/response/result durability, ordinary
engine assessment and event publication are **inside** timing. A capacity-one
lifecycle receiver drains concurrently in the same task; assertions require one
accepted turn, two calls, one successful correlated result and one final event.
Correctness tests additionally capture both requests and verify stable
instructions/tools, exact prefix extension, and equality of reloaded/live
history at all three sizes. Full request capture is compiled out of the timed
benchmark to avoid measuring test-double history clones.

```sh
rtk cargo bench -p zevria-core --features test-support --bench transcript -- engine_round --save-baseline structure-before
# After each structural step 2.1–2.4 and 3.1:
rtk cargo bench -p zevria-core --features test-support --bench transcript -- engine_round --baseline structure-before
```

This measures two complete local calls and real storage, not provider latency,
FIFO supervisor startup/shutdown, or provider-adapter wire serialization. It is
not an isolated engine CPU benchmark. Preserve the saved workload and baseline
when interpreting later structural changes.

### 0.5 — Baselines before behavioral and ownership changes

#### Approved plan's historical session evidence

These are the approved plan's read-only observations on **November 11, 2026**:
November 7–11 application logs contained 8,741 Responses requests across 73
session/profile cache lineages and 1,855 Anthropic responses; 45 root/child
transcripts contained 9,815 records / 40.5 MB. The logged binary **predated
`6f870e4`**. These numbers are retained as historical acceptance baselines, not
recomputed from today's growing files and not measurements of this new build.

| Aggregate bucket | Share of 24.3 h | Per round |
| --- | ---: | --- |
| Model requests | 22.9 h / 94.5% | median 4.75 s; p90 19.5 s |
| Response → next request (tools, persistence, engine) | 1.34 h / 5.5% | p10 49 ms; median 81 ms; p90 131 ms; p99 15.6 s |

| Latency observation | Historical result |
| --- | --- |
| Requests below 50 output tokens | median 2.90 s |
| Below 150 output tokens, below 50K input | median 2.88 s |
| Below 150 output tokens, 100–200K input | median 4.31 s |
| Below 150 output tokens, above 200K input | medians 5.17–5.34 s |
| Local gap below 200 / above 1,200 input items | medians 79 / 121 ms |
| Full / incremental request duration | medians 7.87 / 4.69 s, **not output-controlled** |
| Full / incremental uncached input | means 17.2K / 1.1K tokens |
| Unix `zsh -lc true` / `zsh -c true` | 19.8 / 2.8 ms on the Apple M4 probe |
| Each durable append | 3.5–4 ms; two per ordinary tool round |

| Continuation, tool and cache observation | Historical result |
| --- | --- |
| Responses native assistant records | 4,062 total; 4,009 with exactly one function call; zero multi-call |
| Calls per tool-calling Responses response | exactly 1.00 |
| Anthropic responses with 2–4 calls | 212 of 681 / 31%; 1.35 calls per response |
| All tool calls / `command` calls | 4,926 / 4,295 (87%) |
| Multi-call responses containing non-command tools | one |
| Mid-session full replays | 147; 105 after at least 60 s idle |
| `reason="reconnect"` invalidations | 85; 9.7 per 1,000 requests |
| Longest idle before incremental dispatch | 122 s |
| Incremental requests after more than 300 s idle | 0 of 8,548 |
| Reported/projected cached-input fraction | 98.8% overall; 99.1% incremental, token-weighted |

The original sub-millisecond engine-CPU figure was an **estimate**, not a
measurement. The complete-turn benchmark below includes storage and the
scripted provider's ordinary full preflight, so it does not validate or refute
that CPU-only estimate for the real incremental provider. Phase 1 effects still
require real sessions on a newly built binary; no external sessions were started
for this baseline and no improved cache ratio, batching or continuation rate is
claimed.

#### `structure-before` complete-turn baseline

Captured at `f0f5b5f`, before Phase 1 behavior or Phase 2 ownership changes, using
Apple M4 / `aarch64-apple-darwin`, `rustc 1.100.0-nightly (fb6531d55 2026-08-23)`
and the default optimized bench profile. Each size used 20 samples / 20 complete
two-call turns, 100 ms warmup and a 300 ms requested collection target; Criterion
extended collection because these durable turns exceed that target. Setup and
teardown were excluded as described in 0.4. These are one local run's mean
estimates and bootstrap 95% confidence intervals, not independent repetitions.

| Exact initial live records | Complete two-call turn | 95% confidence interval |
| ---: | ---: | ---: |
| 100 | 18.518 ms | 18.109–18.918 ms |
| 1,000 | 24.059 ms | 23.837–24.301 ms |
| 5,000 | 43.528 ms | 43.137–43.922 ms |

Saved Criterion samples/estimates are under
`target/criterion/engine_round/<records>/structure-before/` (local, untracked).
Retain that directory for structural comparisons; this table preserves the
measurement if build artifacts are removed. The growth here must not be called
pure engine CPU: the synthetic adapter's default preflight scans history, unlike
the real Responses adapter's incremental preflight cache. No structural speedup
has been measured yet.

### 1.1 — Explicit native call batching

Responses completions now send `parallel_tool_calls: true` by default.
`compatibility.parallel_tool_calls=false` disables it; provider
`additional_params` still wins. Exact input-token requests strip the option so
counting gateways need not implement a completion-only parameter. The prepared
request fixture deliberately adds only that property; no instruction/tool bytes
change, and ordinary tool execution remains sequential. HTTP/WebSocket loopback
fixtures cover default/false/true overrides and stable instructions, tools and
continuation prefixes. Live acceptance remains pending: compare about 500
Responses tool-calling responses against the historical 1.00 calls / zero
multi-call baseline and verify cached-input fractions do not fall. If the model
still never batches, keep the switch and treat new guidance or a model-visible
batch tool as a separate decision.

### 1.2 — Idle WebSocket keepalive

The socket pump now sends a Ping after one interval without any frame sent or
received. `network.websocket_keepalive_seconds` defaults to 30; `0` disables it.
The value follows the network policy through initial establishment and reconnect.
Ping-send failure ends the pump; missing pongs alone do not. Pongs stay inside the
pump and cannot reset response-progress deadlines. No early connection, detached
reconnect task, instruction or request-body changes are introduced.

Short-interval loopback tests cover parked pings, invisible pongs, missing-pong
survival, disabled keepalive and response watchdog expiry despite active ping/
pong traffic. Gateway acceptance remains pending: look for incremental requests
after more than 300 seconds idle and fewer reconnect invalidations, without a
cached-input fraction drop. If neither improves, record that the gateway ignores
control frames rather than adding increasingly aggressive keepalive policy.

### 1.3 — Opt-in cancellable idle prewarm

`network.prewarm` defaults to false. The engine polls `ModelProvider::prewarm`
for the selected mode's role in its idle select, once per idle period. Queued or
arriving commands take priority and drop the future before command execution;
no detached task is created. The router initializes/delegates only an opted-in
WebSocket profile; HTTP-only and Anthropic profiles do nothing. A successful
handshake is installed atomically, HTTP 426 selects sticky HTTP, and other
failures leave the first completion's fresh recovery budget intact. A healthy
socket is never replaced merely to prewarm it.

Loopback tests cover opt-out/role locality, no early completion content, healthy
socket reuse, failed and dropped handshakes, a fresh one-attempt completion after
failure, and HTTP 426. Engine tests fence mode/turn/shutdown against pending idle
work and ensure completed prewarm does not spin. `PRIVACY.md` documents that
opt-in prewarm contacts the provider and resolves a configured API-key helper at
session start. Live acceptance remains pending: the handshake should precede
the first round, reducing that round's `model_ms - request_ms` by `connect_ms`.

### 1.4 — Exact counting only for unmeasured images

Context usage now tracks unmeasured model-visible images per profile. Appends
mark new images, provider-reported usage clears the mark, and full reestimation
initializes it from the active conversation without retaining a stale baseline.
Admission and dispatch request an image-driven exact count only without a
compatible usage baseline or when new images were added. Capacity bands,
measurement disagreement and forced rechecks are unchanged.

Regression fixtures exercise three image tool rounds: four model calls need one
count with provider usage, versus four when usage is absent. A new image batch
counts once more, text-only continuation does not, and changing policy shape
counts again. Measured images near the automatic trigger still get an exact
count; profile-local flags and history reestimation are checked independently.
No instructions, tools, completion bodies or persisted records change. Real
image-session request telemetry remains the post-build acceptance check.

### 1.5 — HTTP pooling change not activated

This step is conditional on post-0.1 evidence of a first-request-after-idle
request-start penalty. No real HTTP/SSE sessions on the instrumented build have
been collected, so that condition is **not established**. Completion clients'
`pool_idle_timeout` and `tcp_keepalive` remain unchanged for both Responses and
Anthropic. Revisit only after new request/first-event timings isolate an idle
connection penalty; ordinary full-versus-incremental response durations are not
controlled evidence for this HTTP-specific change.

### 1.6 — Capture the Unix login environment once

`command.shell_environment = "captured"` is the default. Session and worker
composition prepare one process-wide login-shell export before tools freeze the
shell. The shell invokes the current binary's private `--__print-env` entry point
with its path and a random marker as positional arguments (`$0`/`$1`, or fish's
`$argv`). The exporter writes native NUL-delimited key/value bytes. Capture uses
`process::bounded_output` with eight seconds and a 1 MiB per-stream cap, discards
profile noise before the marker, and excludes `PWD`, `OLDPWD`, `SHLVL` and `_`.
Commands then use `-c`, a cleared inherited environment and the captured exports.
Windows retains its existing Git Bash behavior.

`command.shell_environment = "login"` restores per-call login startup. Any
capture failure warns once and falls back to that old behavior. Login-profile
edits and non-exported functions are not picked up by capture; restart the
process or choose `login`. Shell-specific non-login startup files may still run.
Captured values are not logged or persisted, including through `ShellSpec` Debug.
Both binaries dispatch the exporter before configuration, launcher selection or
GUI setup; `PRIVACY.md` describes the early local profile execution.

Tests use a noisy fake shell and an exporter path containing spaces and a quote,
verify a profile counter runs once despite repeated preparation/commands, check
cleared command environments and native non-UTF-8 values, and cover opt-out and
non-retried capture failure. Binary fixtures check both private entry points have
no configuration/GUI side effects. Real single-command `tools_ms` comparison is
still required; no local fake-shell result is claimed as a 17 ms live speedup.

### 1.7 — One append for recoverable record groups

Core now records each nonempty group of completed recovery items through one
`record_completed_items` call rather than one completed append per item. This
uses the existing version-1 batch envelope and one durable append; empty groups
still do nothing. Replay failure retains fatal precedence, and persistence
failure still retains completed work in memory under degraded persistence.

Tests verify three recovery records produce one physical append, preserve the
exact old disk prefix and stay outside model input. The pipeline probe observes
one replay installation for the whole group. A failed pending publication leaves
a competing file untouched, retains every completed record and repairs the full
live history after the obstruction is removed. No new persisted format or
request/instruction bytes are introduced.

## Measurements from the original lifecycle optimization

These are short local optimized Criterion runs (20 samples, 100 ms warmup,
300 ms target measurement). Fixture creation, initial disk publication, cache
priming, and identity construction are excluded from the timed operations.
Criterion increases collection time where necessary. Numbers are point
estimates, not independent process repetitions or end-to-end latency claims.

### Durable admission

The saved `lifecycle-before` baseline named `tool_round` actually admits **one
completed assistant record**, not an assistant/tool-result pair. That workload
and name remain unchanged for a valid before/after comparison. The new
`tool_batch` workload measures an actual assistant call plus correlated result,
with no invented pre-change measurement.

| Existing replay records | Single record before | Single record after | Actual two-record tool batch after |
| --- | ---: | ---: | ---: |
| 100 | 3.923 ms | 3.497 ms | 3.510 ms |
| 1,000 | 3.698 ms | 4.092 ms | 4.202 ms |

The disk-dominated results are mixed: the 100-record sample is lower, while the
1,000-record sample is higher and its confidence interval overlaps the baseline.
They **do not establish a general durable-admission speedup**. The new batch
format removes whole-prefix serialization/write amplification, but durable sync
cost and run-to-run variation still matter.

Reproduce:

```sh
rtk cargo bench -p zevria-core --features test-support --bench transcript -- 'transactions/(100|1000)/tool_(round|batch)' --save-baseline lifecycle-after
```

`cargo bench` already selects the optimized bench profile and does not accept
`--release`.

### Responses capacity preflight

The `zevria-responses` preflight benchmark compares the full path with a primed
cache for the same synthetic histories. `append_pair` adds one user message and
one native assistant record; `cached_unchanged` checks an unchanged boundary.
These isolate projection/validation/estimation CPU work, not network or durable
I/O.

| Input items | Full assessment | Assess appended pair | Unchanged cached boundary |
| --- | ---: | ---: | ---: |
| 100 | 206.03 µs | 4.643 µs | 38.08 ns |
| 1,000 | 2.050 ms | 5.435 µs | 38.30 ns |

The append-pair assessment is about 44× / 377× faster than full assessment in
these isolated cases. This is not a corresponding speedup for an entire model
tool round. Full and incremental results are also compared in regression tests across
all suffix splits, compatibility modes, prefix edits, native-ID collisions,
images, opaque material, and tool payloads.

```sh
rtk cargo bench -p zevria-responses --bench preflight
```

### Existing sessions and logs

A one-off read-only probe loaded all four existing workspace session files
(1,149 logical items at the time of the probe). Individual load/validation times
were 0.267–20.906 ms. The largest file was approximately 3.15 MB. The probe checked
exact bytes before/after every load: all files were unchanged, and no repairs or
writable session handles were opened. These are single warm/local observations,
not a resume baseline or a full frontend startup measurement.

The existing application log, analyzed with `/tmp/zevria-analysis/gaps.py` and
`cache.py`, contained 875 incremental usage rows with **98.0%** weighted cached
input, and 29 full-request usage rows with 69.6%. Its recorded local gaps had
66 ms median / 127 ms p90 overall and 69 ms median / 126 ms p90 with at least 100
input items. These observations come from the already-running application/log,
**not a fresh run of the newly built binary**. A later snapshot of the largest
existing session had one function call in each of 511 native assistant records.
It establishes no post-change increase in batching.

No new external provider session was started. Consequently, neither a post-change
≥97% cache-hit target nor increased model-selected tool batching is claimed.
The guidance change is not a deterministic guarantee about model behavior.

## Validation and remaining costs

The release-mode package suites passed for core, transcript, provider, Responses,
Anthropic, host, session API, and TUI. The workspace release suite also passed,
including real root/worker ACP processes and opt-in cache diagnostics. In addition,
the ignored feature-on/off capture fixture was run separately in both builds:
its reconnect/resume WebSocket payloads and HTTP retry bodies were byte-identical
under `cmp`. Formatting and whitespace checks passed. Workspace/all-target Clippy
completed with warnings; `-D warnings` is not clean (large enum/error types and
style/test warnings), so no warning-free lint claim is made.

Loopback tests must bypass the environment's HTTP proxy:

```sh
rtk proxy env NO_PROXY=127.0.0.1,localhost,::1 no_proxy=127.0.0.1,localhost,::1 cargo test --release --quiet --workspace --features zevria-core/test-support,zevria-transcript/test-support,zevria-provider/cache-diagnostics
rtk cargo clippy --release --workspace --all-targets --features zevria-core/test-support,zevria-transcript/test-support,zevria-provider/cache-diagnostics
rtk cargo fmt --all -- --check
rtk git diff --check
```

The optimization does not make every dispatch O(suffix): constructing the borrowed
input reference vector still scans effective history, full HTTP/reconnect replay
must serialize the full payload, and the Anthropic capacity path remains a full
projection. Explicit history edits and degraded-persistence repair still require
full work. There is still a necessary engine-to-frontend ownership copy at resume.

Background WebSocket reconnect needs a transport supervisor with bounded retries,
generation fencing, and cancellation/reset/shutdown race coverage. Existing
on-demand reconnect and full replay remain unchanged; a detached background socket
task is deliberately not introduced.

### 2.1 — One validated replay owner

`Conversation::replay()` borrows its always-present validated result; core no
longer retains an instruction/Plan replay copy. Completed appends move the prior
replay through suffix reduction. Required appends retain exactly one prospective
copy until persistence accepts the proposal, and commits no longer return an
owned replay. Legacy replacement helpers eagerly recompute their replay. Invalid
committed evidence remains terminal and cancels work before policy refresh.

Release workspace tests and all six separately run prefix gates passed. Clippy
retains only the 26 baseline warnings; formatting and whitespace checks passed.
Probes assert one prospective copy for required append and none for completed
append. Instruction/request fixtures are unchanged. Benchmark recheck against
`structure-before`: 100 / 1,000 / 5,000 records: **18.606 / 24.472 / 44.481 ms**;
Criterion reports no change / within noise / within noise. The first run flagged
a 3.4% 5,000-record increase; the isolated recheck was 2.2%, within Criterion's
noise threshold. These include durable storage and are not engine-only CPU.
Logs: `/tmp/zevria-lifecycle-2.1-{checks,recheck}.log`.

### 2.2 — Two admission paths and ordered effects

Core uses `commit(change)` for required proposals and `record_completed(items)`
for completed work. `Committed` identifies the installed range and history
replacement; no detached replay or item copies are returned. `RecordError` keeps
fatal replay, degraded persistence and rejected proposals distinct, including
replay-over-storage precedence. Error conversion no longer uses anyhow downcasts.
One `after_commit` observes fatal replay first, refreshes skills/query state,
resets replaced history, accounts the borrowed committed range, invalidates
preparation and arms compaction in the established order. The completed-input
wrapper preserves single-record versus explicit-batch canonical-file checks.
Empty groups do not create a transaction.

Scoped probes now count actual `TranscriptItem::clone` calls; required and
completed append accounting assert zero item clones, with one/zero prospective
replay copies respectively. Range, rejection, cancellation, degradation, recovery
and prefix tests pass. Release workspace tests, Clippy (26 existing warnings),
formatting, whitespace and all six isolated prefix gates passed. Fixtures did
not change. Log: `/tmp/zevria-lifecycle-2.2-final-checks.log`.

The retained historical Criterion comparison reports **18.942 / 25.341 /
44.775 ms** (100 / 1,000 / 5,000), flagging +5.3% / +2.9% at the larger sizes.
Repeats flagged the 1,000-record case consistently. A detached, unmodified
`f0f5b5f` control run on the same machine measured **21.994 / 27.193 / 45.932 ms**
and itself regressed versus its saved baseline (+18.8% / +13.0% / +5.5%). Thus
the saved-baseline gate is sensitive to environmental/storage drift; current
code is not slower than that contemporary control, but the historical
comparison is **not a clean no-regression or speedup claim**. The control did
not replace `structure-before`; its temporary worktree was removed. Reusing its
target directory exposed a stale no-feature session-api artifact, so that
package's release artifacts were cleaned and the full checks rerun successfully.

### 2.3 — Shared request plans and explicit history views

`RequestPlan` replaces both policy and instruction preparation payloads. It
shares rendered instructions, tools and serialized tool bytes through `Arc`,
and retains shape, fixed/irreducible counts and skill-availability state.
Admission, dispatch, local previews and tool scopes reuse the plan rather than
rendering or copying instruction strings. `HistoryView::{Committed, Prospective}`
selects history explicitly; production no longer uses pointer equality to choose
preparation. Model-preview invalidation is independent of request-plan
invalidation. Tool-window refresh shares existing instruction allocations; stale
application/context keys cannot be relabeled as the current prefix.

A scoped test exercises reconciliation, local assessment and tool-scope capture,
asserting one instruction render and shared instruction/tool allocations, also
across prospective views and preview invalidation. Test-only registry mutation
explicitly invalidates plans; production registries remain frozen.
Release workspace tests, all six isolated prefix gates, Clippy (26 baseline
warnings only), formatting and whitespace passed; fixtures are unchanged.
Final engine-round comparison: **18.167 / 24.607 / 44.302 ms**; Criterion reports
no change / within noise / no change. Log:
`/tmp/zevria-lifecycle-2.3-final-checks.log`.

### 2.4 — Context accounting owner

`ContextAccounting` owns usage trackers (including unmeasured-image state),
prepared/failed count state, capacity candidates/assessments, snapshots and
automatic-compaction arming. `on_committed`, `on_history_replaced`,
`on_provider_usage`, `assess` and cancellable counting centralize their reducers.
Core retains provider/preflight and publication orchestration but delegates
accounting; committed estimates still borrow records and profile/shape baselines
never cross incompatible requests. The default/test count helper now lives with
its authoritative state rather than the engine's dispatch logic.

Release workspace tests, all six isolated prefix gates, Clippy (26 baseline
warnings only), formatting and whitespace passed; fixtures are unchanged. The
existing image, count-failure/cancellation, capacity-band and clone/render probes
pass through the delegated paths. First engine-round results were
18.180 / 24.820 / 43.700 ms, flagging +3.2% at 1,000 records. Isolated recheck:
**17.754 / 24.074 / 41.084 ms**, within noise / no change / improved relative to
the retained baseline. Storage/environment variability discussed in 2.2 still
applies; these are not isolated CPU measurements. Logs:
`/tmp/zevria-lifecycle-2.4-{checks,bench-recheck}.log`.

### 3.1 — Explicit round steps and post-commit usage

The model loop delegates dispatch/checkpoint pumping, attempt settlement,
response admission/publication, and tool-batch execution to named steps. Request
borrows end before checkpoint installation and attempt settlement; cancellation
still wins a just-returned, uncommitted response. A single response commit path
orders persistence → attempt updates → usage → intermediate/final outcome for
ordinary finals, premature orchestration finals, and tool responses.
OpenAI WebSocket/HTTP and Anthropic return completed usage without awaiting the
frontend. Deferred ensemble finals retain their existing atomic workflow commit
and terminal-publication point; usage is emitted there rather than before their
response exists durably. Their pre-existing deferred attempt-publication point
is unchanged. Frontends already consume usage metadata independently of provider
stream events and require no presentation/protocol change.

Tests assert one usage event, attempt-before-usage-before-terminal order, deferred
usage absence until finalization, native/HTTP returned usage without premature
provider publication, and response durability while a capacity-one receiver is
blocked. Release workspace tests, all six isolated prefix gates, Clippy (26
baseline warnings only), formatting and whitespace passed. Instruction and
request fixtures are unchanged. Engine-round result **18.311 / 24.542 /
42.602 ms**: no change / no change / improved against the retained baseline.
Logs: `/tmp/zevria-lifecycle-3.1-{checks,final-checks}.log`.

### 3.2 — Typed calls, effects and batch policy

Each live batch resolves call names once into `EngineCall`/`EngineTool` and
computes one `BatchPlan`: skill-only, mixed-rejected, serialized Plan/ensemble,
or default. Dispatch, skill admission, Plan draft/submission checks, projection
metadata and orchestration correlation reuse those identities. Recovery resolves
its persisted call/result identities independently.
`ToolCallSlot` now owns one `ToolEffect` instead of five independent optional
payloads. Typed question-disposition sidecars remain explicitly preservable with
Plan effects, including existing arbitrary-tool result-detail behavior. Wire
names, outputs, metadata and correlation bytes are unchanged. Ordinary calls
remain sequential; only the pre-existing launch-subtasks futures overlap.

Release workspace tests, six isolated prefix gates, Clippy (26 baseline warnings
only after moving a test-only constant import), formatting and whitespace passed.
The batch-plan policy/order probe and existing skills, Plan, question, result
detail, orchestration, cancellation and tool-order tests pass. Fixtures are
unchanged. Logs: `/tmp/zevria-lifecycle-3.2-{checks,final-checks}.log`.

### 3.3 — One control-plane router

Idle runtime dispatch, active-work supervision, and the public direct test API
now use one `CommandRouter`. It applies command/worker/question controls,
cancellation and shutdown synchronously, returns owed replies, and distinguishes
idle dispatch from FIFO queuing and active busy/query handling. Schedulers still
own supervision and reply publication; no control effect moves behind an await.
The direct API's historical explicitly registered worker-route behavior is an
explicit policy input, while production idle dispatch still rejects worker
controls. Maintenance cancellation remains request-ID scoped; bounded replies
cannot starve active work or delay fatal replay propagation.

Release workspace tests and six isolated prefix gates passed, including FIFO,
starvation, busy mutation/query, cancellation, shutdown, worker-control and model
maintenance tests. A new router probe verifies idle controls apply immediately
while management work returns to its scheduler without side effects. Clippy
retains 26 baseline warnings only; formatting/whitespace pass. Fixtures unchanged.
Log: `/tmp/zevria-lifecycle-3.3-checks.log`.

### 3.4 — Smaller worker-review and model-selection workflows

Worker review separates recovery/preparation, the serialized control loop,
correlated-control draining on every exit, and sealed-worker acknowledgement.
The same route registration remains alive for exactly the control-loop scope;
it is dropped before the lossless drain, including failed durable acceptance.
Frozen recovery, abandonment, actor mirrors, confirmation receipts and late
telemetry keep their existing order and barriers.
Model selection carries one validated intent through explicit validation,
conversion-requirement assessment, confirmation preview, conversion and the
short synchronous commit. No lock crosses counting/inference. Checkpoint-first,
config-before-header, same-profile continuation, stale revision, cancellation
and partial-save semantics are unchanged. The temporary preflight history
snapshot ends with its assessment stage instead of spanning later inference.

Release workspace tests, six isolated prefix gates, Clippy (26 baseline warnings
only), formatting and whitespace passed. Existing worker recovery/control/
confirmation/abandonment and model preview/conversion/save-failure suites cover
the extracted paths. Instruction/request fixtures unchanged.
Log: `/tmp/zevria-lifecycle-3.4-checks.log`.

### 3.5 — Responsibility groups and explicit production dependencies

The session root now composes `kernel/` (replay/commits, instructions/plans,
accounting/capacity/state/timing), `turn/` (execution, rounds, tools, routing,
orchestration and jobs), `workflow/` (prompts, Plan/ensemble review, compaction
and summaries), and `manage/` (modes, models and skills). Production files import
their dependencies explicitly from their defining modules/crates instead of
`use super::*`. A cfg-test-only prelude retains existing unit-test conveniences;
bench support also has explicit imports. Internal visibility keeps the former
session scope and test paths follow the new physical layout.

This is a pure layout/import move. Automated comparison found all **22 moved
production module bodies equivalent** after normalizing imports, qualified paths,
visibility scope, whitespace and formatter trailing commas. No behavior changes
are bundled. Release workspace tests, six isolated prefix gates, Clippy (26
baseline warnings only), formatting and whitespace passed. Instruction/request
fixtures unchanged. Log: `/tmp/zevria-lifecycle-3.5-checks.log`.

### 4.1 — Responses provider modules (pure move)

The former 2,495-line `turn.rs` is split into `responses/{prepare, continuation,
websocket, http, driver, recovery_loop, maintenance, errors}.rs` plus its facade.
Modules use transport-specific explicit imports; test helpers and the trait
implementation remain in the facade. Crate-private callers retain the `turn`
alias, and test include paths are adjusted without changing fixtures. Internal
fields/helpers retain access across the former file's boundary.

Normalized body comparison preserves all extracted bodies; the facade differs
only by imports/declarations and its two relocated test paths. No event-driver
or continuation behavior change is bundled. Release workspace tests, all six
isolated prefix gates, Clippy (26 baseline warnings only), formatting and
whitespace passed. Instruction/request fixtures unchanged. Captured diagnostic
on/off wire output for the next step compares byte-identically at
`/tmp/zevria-wire-4.2-before-{off,on}.json`.
Log: `/tmp/zevria-lifecycle-4.1-final-checks.log`.

### 4.2 — One Responses event driver

Both frame loops now feed decoded JSON to one `ResponseDriver`: diagnostics,
request telemetry, response-progress observation, meaningful-progress clock,
native-output capture, attempt reduction, typed parsing, and terminal handling.
Typed transport policy retains WebSocket prior-ID exclusion/observed IDs, stale
response repair, connection limits, continuation installation and late-done
markers versus HTTP request-ID/category mapping and retry metadata.
Malformed framing/raw JSON remains with its transport. Explicit hooks retain
parse/item-error cleanup, failed-chunk versus failed-done cleanup, and HTTP's
extra missing-ID terminal log; those differences are not silently unified.
Attempt wrappers and their error/quiet-tail flushes remain outside the driver.

Release workspace tests, six isolated prefix gates, Clippy (26 baseline warnings
only), formatting and whitespace passed, including late terminals, recovery,
watchdogs, HTTP status/error classification and streaming fixtures. Instruction
and request fixtures are unchanged. `cmp` passes for both WebSocket/HTTP capture
files before/after and diagnostics on/off:
`/tmp/zevria-wire-4.2-{before,after}-{off,on}.json`.
Log: `/tmp/zevria-lifecycle-4.2-checks.log`.

### 4.3 — Mandatory continuation lineage

`ContinuationState` always carries its validated logical lineage; the legacy
projected request/output vectors and fallback selector are removed. One
`after_response` constructor serves production and all four former legacy
fixtures. It extends the logical digest by one completed response and measures
the projected prefix using native item count, not logical message count.
Selection still requires socket generation, property compatibility, strict input
extension and the preparation-time projection-extension guard; native-ID remints
cannot continue solely because a logical digest matches. Native output moves into
its response record instead of being copied for obsolete bookkeeping.

Count/cancel fixtures now seed a real prepared request and native response; the
instruction and benchmark fixtures use the same constructor outside measurement.
Release workspace tests, six isolated prefix gates, Clippy (26 baseline warnings),
formatting and whitespace pass. Fixtures unchanged. Before/after capture `cmp`
passes for both feature configurations and diagnostics on/off equivalence:
`/tmp/zevria-wire-4.3-{before,after}-{off,on}.json`.
Log: `/tmp/zevria-lifecycle-4.3-checks.log`.

### 4.4 — One observational diagnostics facade

Runtime call sites now use `Diagnostics` and opaque request/transmission/observer
metadata in both feature configurations. Enabled implementations delegate to the
existing observer; disabled owners and socket/router sidecars are zero-sized and
perform no diagnostic hashing, history inspection, allocations or request clones.
The diagnostic HTTP builder clone is confined to the enabled facade. Public
opt-in/context exports retain their feature gates. Transport and continuation
choices remain outside the observer.

Release default-provider tests/Clippy, enabled workspace tests/Clippy (26 baseline
warnings), all six isolated prefix gates, formatting and whitespace passed.
Before/after and on/off captures compare byte-identically:
`/tmp/zevria-wire-4.4-{before,after}-{off,on}.json`. Fixtures unchanged.
The removal inventory/script now understands the Responses split and facade,
retains the permanent recovery-jitter `uuid` dependency, and preserves historical
reports. In an independently marked source copy with its own target directory,
observer deletion passed release all-target workspace checking, the complete
provider suite (including its four permanent prefix gates), and both core prefix
gates. No Git worktree or live session was changed. Logs:
`/tmp/zevria-lifecycle-4.4-{checks,final-checks,removal-checks}.log`.

### 4.5 — Changed-only attempt reduction and in-place ACP text

Attempt ingestion no longer clones/deep-compares the accumulated attempt. Visible
part, response-ID, hosted-action and answer-set/order mutations mark revisions;
authority-only bookkeeping, duplicate terminals and sanitized-empty deltas do not.
Final reasoning retains identical addressed parts rather than clearing and
reinserting them. Reasoning deltas append to their existing allocation while
preserving readability/whitespace, authority fences and item-ID corrections.
ACP preview coalescing moves the active matching segment out of the watch value,
appends in place and replaces its metadata under the same sequencing lock. A new
thought/message/ID segment keeps the unread previous row.

Pre-reserved-allocation tests assert pointer retention across 256 chunks in both
paths; revision/equality checks cover duplicate terminals, authority changes and
removed reasoning. Existing annotation, control-sanitization, ordering, preview
fencing and relay tests pass. Release workspace tests, six isolated prefix gates,
Clippy (26 baseline warnings), formatting and whitespace pass. Fixtures unchanged;
before/after and diagnostic on/off capture `cmp` passes:
`/tmp/zevria-wire-4.5-{before,after}-{off,on}.json`.
Log: `/tmp/zevria-lifecycle-4.5-checks.log`.

### 4.6 — Borrowed Responses serialization

Full and incremental transmissions borrow slices from their immutable prepared
request; their lifetime does not borrow the mutable provider. WebSocket and HTTP
bodies serialize borrowed nested properties/input through a small property index,
without cloning projected trees. Insertions replace values in place, and HTTP
removals deliberately emulate `serde_json::Map`'s preserve-order **swap-removal**.
This preserves incidental key order, including caller-supplied transport fields.
HTTP authentication/retry clones of its already serialized builder are unchanged;
the extra diagnostic clone remains enabled-only behind the facade.

An exhaustive five-property permutation/prefix test compares bytes with the
former map builders for both WebSocket continuation modes and HTTP. Continuation
tests assert full/suffix pointer identity with prepared input. Release workspace
tests, six isolated prefix gates, Clippy (26 baseline warnings), formatting and
whitespace pass. Fixtures unchanged. Before/after and on/off captured WebSocket
and HTTP bytes compare identically:
`/tmp/zevria-wire-4.6-{before,after}-{off,on}.json`.
Log: `/tmp/zevria-lifecycle-4.6-checks.log`.

### 4.7 — Shared validated Anthropic history

Each router slot caches one immutable, destination/compatibility/input-digest
keyed `ValidatedHistory` and its management assessment. Capacity preflight,
router validation, completion and counting share that artifact rather than
repeating native/portable projection and tool correlation. Changed input,
profile or compatibility rebuilds it; synthetic callers use a content-derived
identity. The artifact is deliberately **unbound**: management still assesses an
empty prefix and idle history, while completion/count bind to their actual
system/tools and validate completion shape. Reusing the management-bound output
would incorrectly discard signed thinking. Request assembly still owns copies
of projected JSON; this is not a copy-free Anthropic serializer.

Tests verify artifact reuse/key boundaries, cached/uncached body and count bytes
under inline/folded tools, unchanged native signatures, actual system/tool
binding changes and mismatched-input rejection. Existing ConversionRequired,
foreign replay, count, fallback and one-time binding-repair suites pass. Release
workspace tests, six isolated prefix gates, Clippy (26 baseline warnings),
formatting and whitespace pass. Fixtures unchanged; Responses before/after and
on/off capture `cmp` passes:
`/tmp/zevria-wire-4.7-{before,after}-{off,on}.json`.

The Phase 4 Responses preflight benchmark (unchanged benchmark/preflight source)
measured **205.19 µs / 2.0311 ms** full, **4.7435 / 5.5687 µs** appended pair,
and **37.855 / 38.215 ns** unchanged cache at 100 / 1,000 records on recheck.
The historical report is 206.03 µs / 2.050 ms, 4.643 / 5.435 µs and 38.08 /
38.30 ns respectively. Criterion flagged a 2% 100-record full-path difference
between two runs of the identical current build; the larger full path/cache
were within noise and the appended-pair path improved on that immediate recheck.
Thus these microbenchmarks show environmental variability, not a clean uniform
speedup/no-regression claim against the historical samples. Logs:
`/tmp/zevria-lifecycle-4.7-{checks,bench-recheck}.log`.

### 5.1 — Transcript responsibility modules (pure move)

The transcript facade retains all crate-root and `transcript::*` exports while
its implementation moves into `item`, `sessions`, `writer`, `header`,
`conversation`, `load` and `projection`. Production dependencies are explicit;
only tests inherit a compatibility prelude. Tests are owned by their subjects:
eight existing test files move unchanged, and the large inline test module is
split into 52 tests with ten shared helpers. Sibling storage/decoder/test access
uses the former transcript module's privacy boundary, not public fields.

Automated comparison verifies all seven production bodies after normalizing
imports/visibility/formatting, all eight external test files byte-for-byte, and
every inline test/helper body. No logic or API narrowing is bundled. Release
workspace tests, six isolated prefix gates, Clippy (26 baseline warnings),
formatting and whitespace pass. Instruction/request/persisted fixtures unchanged.
Log: `/tmp/zevria-lifecycle-5.1-checks.log`.

### 5.2 — Production mutation surface

Legacy required append/batch, truncate/tail replacement, persisted-item injection
and latest-checkpoint helpers are now hidden behind unit-test/`test-support`
configuration. Normal builds expose validated anchored admission and completed
work admission, not raw fixture mutation. The existing synthetic benchmark and
intentional invalid-evidence tests retain their explicit feature-only hooks.
Ordinary tool-window lifecycle tests now exercise production anchored commits;
header-only/historical storage regressions keep their specialized fixture paths.

Default-feature release workspace checking and transcript tests pass, proving
normal callers need none of the removed APIs. The feature-enabled release
workspace tests, six isolated prefix gates, Clippy (26 baseline warnings),
formatting and whitespace also pass. No persistence, projection, instruction or
request bytes changed. Log: `/tmp/zevria-lifecycle-5.2-checks.log`.

### 5.3 — One full worker-log scan on reopen

`AgentRunTranscriptWriter::reopen` validates and projects the same streaming
reader, checks the expected root/worker header **before** tail repair, and moves
the exhausted reader's authoritative append validator into the writer. Ensemble
logging no longer projects a log and then reopens it through a second validation
pass. The display collector is not treated as proof of valid journal order;
existing bounded final-tail repair and writer synchronization are unchanged.

Feature-gated probes assert one reader open/four validated records for a complete
reopen. Regressions cover retained terminal append state, missing final newline,
wrong expected identity leaving crash bytes untouched, and invalid complete
journal evidence rejecting before repair. Release workspace tests, six isolated
prefix gates, Clippy (26 baseline warnings), formatting and whitespace pass.
Fixtures and persistence/request bytes unchanged.
Log: `/tmp/zevria-lifecycle-5.3-checks.log`.

### 6.1 — Shared staged host composition

Interactive roots and restricted native workers now use one profile-driven
startup coordinator: resolve intent/config/workspace → acquire lease and load/
validate storage → build services → prepare writer/build engine → spawn. Typed
stage values carry ownership instead of parallel opening functions. Root-only
skills/model management, subtask supervision, jobs, ensemble launch and Plan
projection remain explicit; workers do not construct those services. Workspace
namespace guards, selection/dependency errors before writable repair, ensemble
construction timing, opening Plan enqueue, lease retention and the dependent
supervisor's keepalive/publication lifetime retain their prior boundaries.

Release host and workspace tests, all six isolated prefix gates, Clippy (26
baseline warnings after removing one newly introduced style warning), formatting
and whitespace pass. The diagnostics removal inventory follows the relocated
shared startup hook; deletion in another marked source copy passed release
all-target workspace checking and host tests with its own target directory.
Instruction/request/persisted fixtures unchanged. Logs:
`/tmp/zevria-lifecycle-6.1-{checks,final-checks,removal-checks}.log`.

### 6.2 — One configured engine build

`SessionEngineBuilder` retains an unexposed opening engine until history,
guidance, compaction, skill policy and optional capabilities are configured.
`build` performs one initial context-accounting pass. Root/native-worker startup
and Explore/Builder child composition use it, including validated load-report
construction without reopening/revalidating history. Existing ready-engine
constructors and live setters retain their invalidation semantics; history-seed
versus loaded-history compaction arming remains explicit.

A scoped pipeline probe asserts one accounting pass after complete configuration,
byte-equal rendered instructions/tools and equal restored contexts versus the
legacy chain. A loaded-builder probe asserts zero new loads/full replay/
validations, and live guidance/compaction setters still perform their two
expected invalidations. Release workspace tests, six isolated prefix gates,
Clippy (26 baseline warnings), formatting and whitespace pass. Fixtures unchanged.
Log: `/tmp/zevria-lifecycle-6.2-checks.log`.

### 6.3 — Reuse validated restoration

Resume no longer validates the authoritative root or each child a second time.
Child transcripts and worker logs load concurrently on the blocking pool. All
read-only jobs finish before the first error is selected in original discovery/
record/descriptor order; writer repair and service/engine spawning remain behind
the all-dependencies-valid barrier. Worker records and expected-header checks
share one strict reader, replacing a projection preflight plus a second capture
scan. Missing optional worker logs remain supported.

The already validated `ActiveSkills` snapshot used for startup pin diagnostics
moves through `SessionRestoration`/`RestoredSession` to terminal and desktop;
neither frontend replays pins again. Startup input is projected once and replay
preflight runs once per distinct profile, while role-specific notices keep their
original order and wording. No request or persisted bytes change.

Tests cover overlapping dependency jobs with ordered results, an all-finished
error barrier, recoverable bytes remaining untouched on a dependency failure,
zero extra root validation, one child validation, one worker-reader scan,
allocation-preserving pin handoff and one preflight per distinct profile.
Release workspace tests, six isolated prefix gates, Clippy (26 baseline warnings),
formatting and whitespace pass. Log: `/tmp/zevria-lifecycle-6.3-checks.log`.

## Final Phases 2–6 verification

- Full release workspace tests with core/transcript test-support and provider
  diagnostics pass; all six permanent prefix gates also pass independently.
  Default-feature release workspace checking passes. Clippy completes with the
  same **26 existing warnings**, not a warning-free result.
- Final diagnostics off/on wire captures are byte-identical to one another and
  to the pre-4.2 captures, covering WebSocket continuation/reconnect/resume and
  HTTP retries. Every `**/fixtures/**` file is unchanged since the Phase 0–1
  checkpoint `af011913604ef1fc3c76c8d8db62a47b2e507c81`; the earlier deliberate
  Phase 1.1 request property is not silently reverted.
- All **11** Python lifecycle-analysis tests, formatting and whitespace checks
  pass. Observer removal on the final source in an independently marked copy
  passes release all-target workspace checking and host tests with its own
  target directory.
- Final engine-round benchmark against retained `structure-before`, at 100 /
  1,000 / 5,000 records: **16.231 / 23.733 / 39.681 ms**. Criterion reports
  improved / within noise / improved in this run. These include durable storage;
  the environmental/storage drift documented in 2.2 still limits attribution.
- Responses preflight's unchanged executable showed small run-to-run shifts:
  initial final-run cache/full cases flagged roughly 2–3% versus the prior run.
  Its immediate standalone recheck (no source change or rebuild) measured
  **198.68 µs / 2.0570 ms** full, **4.6662 / 5.5981 µs** appended pair and
  **38.703 / 38.209 ns** unchanged cache at 100 / 1,000 records. Criterion then
  reported improved/within-noise cases throughout. This is recorded variation,
  not a claim that every microbenchmark uniformly improved.

Evidence: `/tmp/zevria-lifecycle-final-{checks,static,preflight-recheck,removal-checks}.log`
and `/tmp/zevria-wire-final-{off,on}.json`. No paid/live provider acceptance
session was started. Conditional 1.5 and real-session latency, batching,
continuation and cached-token acceptance are **not** claimed complete. No push.
