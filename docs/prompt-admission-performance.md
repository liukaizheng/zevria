# Prompt admission performance

This report preserves two historical measurement stages:

- [Stage 1: synchronous admission replay reuse](#stage-1-synchronous-admission-replay-reuse),
  measured on September 19, 2026, against baseline `f570674`.
- [Stage 2: staged preparation, measurement and acceptance](#stage-2-staged-preparation-measurement-and-acceptance),
  a follow-up against baseline `9119791`.

The stages have different measurement boundaries and local baselines; their
figures are not a continuous before/after comparison or a measurement of current
latency. Historical commands, timings, allocation counts and caveats are retained
below. Transcript-format results remain separate in
[transcript-performance.md](transcript-performance.md).

## Current benchmark boundaries

The current [`AdmissionFixture`](../crates/core/src/session/benchmark.rs)
wrappers exercise production paths in
[`session/prompt.rs`](../crates/core/src/session/prompt.rs):

- `admit_prompt`: preflight and synchronous preparation, including rendered
  instructions, context limits and an irreducible-input estimate. No
  checkpoint-aware projection or durable mutation is included.
- `admit_prompt_pipeline/measure`: adds projection, tool selection/serialization,
  fixed-token and request-shape computation, and deterministic exact counting.
- `admit_prompt_pipeline/accept`: the async acceptance pipeline, including normal
  measurement/compaction decisions, authoritative replay validation, persistence,
  replay installation, accounting and acceptance events. No completion or tool
  execution is included.

Tool serialization and request-shape fingerprinting now happen during
measurement, not synchronous preparation. Stage 2 below records the earlier
boundary, which included those costs in preparation; its figures have not been
remeasured for the current implementation.

## Stage 1: synchronous admission replay reuse

### Measurement boundary

Local measurements on September 19, 2026, based on `f570674`. The baseline was
captured **before changing production behavior**, after adding the synthetic
`admit_prompt` harness and making the private method visible to its sibling
benchmark wrapper. This is separate from the older transcript-format benchmark
results in `transcript-performance.md`.

- `aarch64-apple-darwin`, `rustc 1.100.0-nightly (fb6531d55 2026-08-23)`;
  optimized Cargo bench profile, offline dependencies.
- Criterion: 20 samples, 100 ms warmup, 300 ms target measurement interval.
  The table records the final comparison run's reported point estimates, not
  an end-to-end turn latency guarantee. Short local measurements are noisy.
- Both versions call the actual synchronous `SessionEngine::admit_prompt`.
  The older `prepare_prompt` benchmark only exercises bare turn preparation.
- Fixtures are constructed outside timing/counting: a reconciled instruction
  prefix, zero or one pinned skill, and replay-heavy historical messages with
  native provider payloads. History dimensions are 10/1,000 requested records
  and 128/8,192-byte payloads. The common fixture's two metadata records are
  omitted when adding history to the engine's existing prefix.
- Append retains all history. Edit replaces the final two historical records,
  retaining nearly the same history. Activation begins unpinned; reapplication
  retains its first activation before the synthetic message history.
- No provider or tool is executed. Persistence setup, token estimation/counting,
  compaction, acceptance, and authoritative commit replay are **not timed**.
  Admission's output destruction is included.
- Allocation probes use the separate `allocation-probe` executable. Counts
  include allocations/reallocations and their cumulative requested bytes,
  **not** live heap size, retained memory, or peak RSS.

### Timings

Microseconds per synchronous admission. Baseline → refactored:

| History / payload bytes | Input | Append (µs) | Edit (µs) |
| --- | --- | ---: | ---: |
| 10 / 128 | Message | 6.272 → 5.004 | 6.170 → 7.077 |
| 10 / 128 | Activation | 31.454 → 22.511 | 31.326 → 24.210 |
| 10 / 128 | Reapplication | 36.072 → 15.836 | 35.913 → 20.531 |
| 10 / 8192 | Message | 6.246 → 5.022 | 6.130 → 7.028 |
| 10 / 8192 | Activation | 35.215 → 22.419 | 34.492 → 24.675 |
| 10 / 8192 | Reapplication | 39.461 → 15.555 | 38.327 → 20.779 |
| 1000 / 128 | Message | 9.048 → 4.964 | 9.672 → 10.094 |
| 1000 / 128 | Activation | 506.740 → 22.399 | 501.360 → 27.471 |
| 1000 / 128 | Reapplication | 504.330 → 15.439 | 518.190 → 23.453 |
| 1000 / 8192 | Message | 8.973 → 4.964 | 9.905 → 10.064 |
| 1000 / 8192 | Activation | 1221.500 → 22.210 | 1202.600 → 27.657 |
| 1000 / 8192 | Reapplication | 1245.900 → 15.599 | 1216.700 → 23.567 |

The large-payload typed-skill cases improve by roughly 98%, eliminating their
history-sized prospective transcript copy and repeated instruction reductions.
Append-message admission improves by about 20% for short histories and 45% for
long histories.

Not every case improves: **short edit-message admission regresses about 15%
(~0.9 µs)**; the long edit-message cases are ~1.6–4.4% slower by point estimate.
The 8 KiB long edit-message comparison was classified within Criterion's noise
threshold. The new path also carries structural reducer state and validates the
prospective suffix before commit; reduced historical pass counts alone are not
a guarantee of faster small-message edits. No wall-clock assertions were added.

### Allocations

For 1,000 requested historical records and 8,192-byte payloads:

| Operation | Calls before → after | Requested bytes before → after |
| --- | ---: | ---: |
| Append message | 212 → 185 | 60,078 → 49,254 |
| Edit message | 212 → 242 | 60,078 → 70,815 |
| Append activation | 26,280 → 618 | 24,397,065 → 179,225 |
| Edit activation | 26,229 → 675 | 24,348,653 → 200,786 |
| Append reapplication | 26,396 → 532 | 24,459,862 → 149,915 |
| Edit reapplication | 26,351 → 631 | 24,412,218 → 187,859 |

Refactored counts and requested bytes are identical across all four tested
history-size/payload-size combinations for each operation. This demonstrates
removal of historical **message/payload** cloning in admission, not constant
cost for arbitrary instruction state. More pins, larger pinned bodies/catalogs,
more pending calls, or larger Plan artifacts can still increase reducer-state
copying and reconciliation work. Edits still scan their retained history.

### Structural assertions and unchanged boundaries

Feature-gated, admission-scoped replay probes assert for messages, activation,
and reapplication:

- Append: zero full historical instruction replays and zero pin-only replays.
- Edit: one combined retained-prefix replay and zero pin-only replays.
- Both: one suffix application, whose input length is exactly the newly prepared
  record count, never the historical prefix plus tail.
- Plan prefix replay is counted separately: zero for append, one for edit.

The combined reducer preserves pending tool calls, header position, and absolute
lifecycle locations. Split tests include unresolved calls and successful results,
forbidden directives/checkpoints, and complete workflow/revocation batches.
Failure-only pin replay preserves `Skills`/`Plan`/`Both` error priority over an
earlier directive failure. The standalone pin API remains directive-independent.

Core tests compare exact ordered records and owned model input against the
source-based preparation reference, including first activation, reapplication,
edits, and Ready revisions. Existing provider wire fixtures cover fixed bootstrap
bytes/identity, cache keys, retained prefixes, and count/completion parity.
Existing core compaction/resume/persistence suites remain the checks on the
asynchronous and authoritative boundaries. Admission never installs its staged
reducer state after asynchronous work; commit still performs full validation.

### Reproduction

Capture the baseline with the benchmark-only changes, before the production
refactor; then compare using the same fixture and feature flags:

```bash
rtk cargo bench --offline -p zevria-core --features test-support --bench transcript -- admit_prompt --save-baseline anchored-before
rtk cargo bench --offline -p zevria-core --features test-support --bench transcript -- admit_prompt --baseline anchored-before
rtk cargo bench --offline -p zevria-core --features allocation-probe --bench transcript -- admit_prompt
```

Criterion samples and the named local baseline are under `target/criterion`;
allocation results are printed by the separate executable. Neither is a CI
performance threshold. Token measurement, projection, persistence, compaction,
and commit processing retain their own costs and are outside this optimization.

## Stage 2: staged preparation, measurement and acceptance

Follow-up to the [replay-reuse stage](#stage-1-synchronous-admission-replay-reuse),
implementing plan `689bf902-427e-4ef4-a84f-6ec813272ccd@1` against baseline `9119791`.

### Boundaries and reproducibility

At this measured stage, the `admit_prompt` matrix exercised append/edit ×
message/activation/reapplication at 10/1,000 records and 128/8,192-byte native
replay payloads. It called the production preflight and preparation stages.
Preparation then included the immutable counting context: validated/rendered
instructions, fixed overhead, filtered tool serialization, and request-shape
fingerprinting. These costs must not be mistaken for projection or persistence;
the [current benchmark boundaries](#current-benchmark-boundaries) differ.

Two additional matrices made the broader boundaries explicit:

- `admit_prompt_pipeline/measure`: preflight, preparation, one borrowed
  checkpoint-aware projection, local estimation, deterministic exact counting,
  and the first-dispatch identity. No durable mutation. Exact counting is forced
  here so every sample exercises the identity handoff work.
- `admit_prompt_pipeline/accept`: the actual async acceptance pipeline, including
  authoritative cross-domain validation, persistence, replay installation,
  accounting, and acceptance events. Exact counting uses normal admission
  thresholds. Fixture creation and initial transcript writes are outside timing.
  Single-record appends use append/fsync; multi-record activation and edits use
  atomic rewrites. No completion or tool execution is included.

The provider performs no network operations and returns `Exact(1)` whenever
counting is requested. The matrices do not benchmark compaction inference.
Compaction work is covered by deterministic regression tests instead.

Commands used for the local before/after run:

```text
rtk cargo bench -p zevria-core --bench transcript --features test-support -- admit_prompt --quick --save-baseline staged-before
rtk cargo bench -p zevria-core --bench transcript --features test-support -- admit_prompt --quick
```

The before run completed before production edits. Its printed results were
retained, but the generated Criterion baseline directory was unavailable when
attempting `--baseline staged-before`. Consequently the tables below compare
captured point estimates, **not a paired Criterion significance analysis**.
Both runs used `--quick`; local microsecond results and filesystem timings are
noisy and are not CI performance thresholds. All 72 final cases completed.

### Synchronous preparation: before → after

Microseconds, using the middle estimate printed by Criterion:

| Records / payload bytes | Input | Append (µs) | Edit (µs) |
| --- | --- | ---: | ---: |
| 10 / 128 | Message | 2.607 → 4.975 | 2.704 → 4.974 |
| 10 / 128 | Activation | 9.662 → 9.536 | 9.614 → 9.629 |
| 10 / 128 | Reapplication | 8.965 → 9.414 | 10.467 → 10.983 |
| 10 / 8192 | Message | 2.555 → 4.997 | 2.570 → 5.020 |
| 10 / 8192 | Activation | 9.295 → 9.637 | 9.541 → 9.676 |
| 10 / 8192 | Reapplication | 8.869 → 9.449 | 10.554 → 10.971 |
| 1000 / 128 | Message | 2.850 → 5.016 | 8.577 → 10.795 |
| 1000 / 128 | Activation | 9.492 → 9.601 | 15.184 → 15.390 |
| 1000 / 128 | Reapplication | 9.136 → 9.322 | 16.464 → 17.161 |
| 1000 / 8192 | Message | 2.910 → 4.985 | 8.534 → 10.854 |
| 1000 / 8192 | Activation | 9.457 → 9.668 | 15.156 → 15.297 |
| 1000 / 8192 | Reapplication | 8.910 → 9.518 | 16.395 → 16.853 |

**This narrow preparation benchmark does not improve overall.** Message
preparation adds roughly 2–2.5 µs while skill preparation is approximately flat
to modestly slower. At this historical stage, preparation eagerly built counting
material that was formerly recomputed later; reducing total pipeline work did
not imply speeding up this isolated stage. No end-to-end before/after latency
improvement is claimed.

### Extended pipeline samples

Final-run milliseconds for 1,000 records × 8,192-byte payloads:

| Input | Measure append | Measure edit | Accept append | Accept edit |
| --- | ---: | ---: | ---: | ---: |
| Message | 13.921 | 13.875 | 19.768 | 56.455 |
| Activation | 14.241 | 14.243 | 35.713 | 57.187 |
| Reapplication | 14.276 | 13.956 | 18.991 | 55.263 |

These lanes distinguish preparation/projection/count-identity costs from
acceptance with durable I/O; they are not an algebraic decomposition of I/O
alone because acceptance also validates and updates accounting. They have no
pre-refactor timing baseline. They explicitly demonstrate that the complete
workflow is **not constant-time**: projection, hashing, final replay validation,
usage reestimation, and durable rewrites still scale with history/payload size.

### Structural regression coverage

Task-scoped core counters and poll-scoped transcript counters exercise the
actual async `prepare_and_accept_prompt` boundary, including interleaved tasks:

- One instruction snapshot and one prompt preparation per attempt.
- One legitimate pre-prompt reconciliation; skill input additionally has one
  post-invocation update, shared with its irreducible-capacity check.
- Append borrows committed replay; edit reduces the retained instruction and
  Plan prefixes once each. No pin-only replay on successful admission.
- One projection/measurement for an unchanged request, including unavailable
  compaction and unsupported/failed counts. Two only after a checkpoint changes
  the request. A prior exact count forces recount; old usage/exact values do not
  survive rebuilding. The failed-count latch does survive.
- One proposal construction, authoritative replay validation, and replay
  installation per successful commit. Installed append compaction is counted
  as a separate durable mutation; rejected edit checkpoints install nothing.
- No history scan solely to select mode or discover historical request
  boundaries. Orchestration eligibility runs once, before persistence repair.

Tests compare actual count requests with committed model input and first
dispatch, preserve retained prefixes, and check that remeasurement does not
regenerate request identities, invocation records, or body directives. Existing
resume/reconnect/cache-prefix, image, Ready Plan, ensemble, persistence, and
malformed-history suites continue to cover their respective boundaries.
