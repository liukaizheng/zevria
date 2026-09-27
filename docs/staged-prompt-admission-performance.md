# Staged prompt admission and acceptance

Follow-up to `prompt-admission-performance.md`, implementing plan
`689bf902-427e-4ef4-a84f-6ec813272ccd@1` against baseline `9119791`.

## Boundaries and reproducibility

The existing `admit_prompt` matrix still exercises append/edit ×
message/activation/reapplication at 10/1,000 records and 128/8,192-byte native
replay payloads. It now calls the production preflight and preparation stages.
Preparation includes the immutable counting context: validated/rendered
instructions, fixed overhead, filtered tool serialization, and request-shape
fingerprinting. These costs must not be mistaken for projection or persistence.

Two additional matrices make the broader boundaries explicit:

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

## Synchronous preparation: before → after

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
to modestly slower. The new boundary eagerly prepares counting material that
was formerly recomputed later; reducing total pipeline work does not imply
speeding up this isolated stage. No end-to-end before/after latency improvement
is claimed.

## Extended pipeline samples

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

## Structural regression coverage

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
