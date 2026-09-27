# Anchored prompt admission: replay reuse

This is the historical synchronous-admission report. The subsequent staged
preparation/measurement/acceptance refactor and extended benchmark boundaries
are documented in `staged-prompt-admission-performance.md`.

## Measurement boundary

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

## Timings

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

## Allocations

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

## Structural assertions and unchanged boundaries

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

## Reproduction

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
