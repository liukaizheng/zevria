# Transcript performance refactor

## Current ownership after the domain-crate split

The historical transcript-refactor measurements below and their CSV files are
unchanged. The subsequent structural crate split is measured separately in the
[crate-split report](crate-split/README.md); those before/after runs must not be
substituted for the older format/ownership stages recorded here.

The later synchronous prompt-admission replay optimization is measured in
[prompt-admission-performance.md](prompt-admission-performance.md), including
its own before/after `admit_prompt` timings and allocation counts. It does not
replace the historical format/ownership measurements below.

The end-to-end `transcript` benchmark remains in **zevria-core**, alongside its
feature-gated synthetic fixture builders and opaque engine-admission helpers.
`zevria-transcript` now owns `Conversation`, records, persistence, reconstruction,
shared replay validation and storage-only benchmark operations. `zevria-model`
owns validated replay/message/request values, estimates and the opt-in canonical
derivation counter. Benchmark code imports these owners directly; no engine
umbrella or cross-package implementation includes are used.

The commands below still use `-p zevria-core --bench transcript`. Core's explicit
`test-support` feature forwards the required lower-owner fixture support.
`allocation-probe` remains opt-in and instruments only its separately compiled
benchmark executable, never production or ordinary timing runs. Default
package-local builds do not gain either facility through production dependencies.

## Permanent provider-prefix release gates

These ordinary tests compare captured wire values and committed model input
without fingerprint helpers. Run them in addition to the default workspace suite:

```sh
rtk cargo test -p zevria-provider native_output_items_survive_capture_and_full_replay_after_reconnect
rtk cargo test -p zevria-provider second_prompt
rtk cargo test -p zevria-provider engine_skill_lifecycle_is_incremental_with_both_explicit_wire_roles
rtk cargo test -p zevria-core second_prompt_after_metadata_bearing_results_keeps_the_committed_prefix
rtk cargo test -p zevria-core resumed_session_reuses_the_pre_shutdown_prefix_after_tool_activation
```

The reconnect fixtures require exact old-input/native-output prefixes, stable
cache keys and request properties (including instructions/tools), correct native
call/result correlation, and no duplicate tool execution. They cover both
`developer_messages` compatibility modes. The admission fixture covers metadata
with and without skill applications: newly admitted skill directives append rather
than rewriting earlier model input, and activation leaves top-level instructions
unchanged. Workflow/catalog changes intentionally take the full-replay path.
Ordered skill directives persist at their exact historical positions. Disk/live
record equality and the pre-shutdown request prefix are checked across resume,
including body activation and revocation without reconciliation duplicates.

Provider log assertions run in isolated single-test processes because tracing
callsite interest is process-wide; parallel fixtures without subscribers must
not make log capture nondeterministic. These fixtures do not change provider
behavior or the direct wire assertions above.

## Status and scope

Baseline captured on **September 18, 2026**, before production refactoring, from
base commit `7c478da`. This report is staged evidence, **not a claim that the
approved transcript refactor is complete**. Later sections record only changes
actually implemented and measured. No private transcripts were used.

The baseline commit adds the Criterion harness, feature-gated synthetic builders
and narrow admission helpers, model-projection fingerprints, an independent pin
of the fixed instruction bytes, and this report. It does not change the root
format. Existing serialized histories must not become rejection fixtures until
the writer/loader/recovery format switch lands together.

## Methodology

- Apple M4, 16 GiB RAM; `aarch64-apple-darwin`, Darwin 27.0.0.
- `rustc 1.100.0-nightly (fb6531d55 2026-08-23)`, Cargo
  `1.100.0-nightly (e8cb624d5 2026-08-22)`; default optimized `bench` profile,
  no custom LTO or target CPU flags. The local Cargo configuration uses `kache`.
- Criterion 0.8.2, 20 samples, 100 ms warmup and a 300 ms target measurement
  interval per benchmark. Criterion increases time when necessary. One initial
  full pass plus a separate pass for the added `owned_encode` boundary yielded
  **252 baseline measurements**. This is a local short-run baseline, not a
  cross-machine or long-term stability claim.
- `transcript-performance-baseline.csv` retains all point estimates, 95%
  confidence intervals, median, standard deviation, actual samples and total
  iterations. Estimates are slope estimates where available, otherwise means;
  units in the CSV are nanoseconds. Generated Criterion sample data stays under
  `target/criterion`; do not confuse a sample distribution with independent
  process repetitions.
- Fixture construction and filesystem setup are excluded from timed operations.
  Decode includes constructing and dropping decoded records. Snapshot and
  reconstruction include output destruction. Transactions use
  `iter_batched_ref(..., PerIteration)` to keep setup/teardown outside the timed
  operation. One-record batches deliberately retain staged-rewrite semantics.
- `load` measures reading, decoding and complete loader validation together;
  `decode` and `validate` expose their separate in-memory boundaries. Ordinary
  and replay-heavy histories have little domain reducer state, while mixed
  histories exercise pin, Plan, ensemble, directive and checkpoint reducers.
- Admission benchmarks call the actual private required/completed paths through
  an opaque feature-gated wrapper. Prompt preparation and completed accounting
  are separate. Provider exact token counting and complete prospective
  prompt/checkpoint preparation still need dedicated benchmarks; no provider
  timing is inferred from the local preparation benchmark.
- Filesystem benchmarks separate serialized writes, staged file sync, rename,
  directory sync, and complete production rewrites. Isolated directory sync on
  an already-synced directory is not the same operation as a dirty directory
  after a real rename. Do not add these medians and claim an end-to-end cost.
- Allocation instrumentation is compiled **only into a separate bench
  executable** with `allocation-probe`. Production and normal timing builds
  use their ordinary allocator. Counts include allocation and reallocation
  calls and the requested size of each; they are neither live bytes nor peak
  heap usage and count reallocations cumulatively. Fixture construction is
  outside the counted region. There are no concurrent tests in that process.
- RSS probes are single executions of a prebuilt release test binary. RSS
  includes allocator retention, executable/runtime pages and OS accounting;
  differences are observations, not exact native-ledger copy sizes.

## Synthetic dimensions

All identities and content are synthetic. Ordinary histories alternate user and
assistant text. Replay histories add opaque reasoning, native IDs and unknown
native JSON values. Mixed groups include live directives, ordinary/native
messages, correlated successful/failed skill tools, metadata and file changes,
direct skill activation/reapplication with historical pins, Plan start/abandon,
versioned ensemble start/cancellation, local and opaque checkpoints, attempts
and errors. Groups finish whole lifecycle spans rather than truncating them to
an invalid requested count. Images use valid 8×8 RGBA-derived PNGs, and image
histories are capped at 100 records. Payload text is 128 or 8,192 bytes, bounded
independently of the image size.

| Workload / requested length | Actual live / durable | JSONL bytes (128-byte payload) | JSONL bytes (8,192-byte payload) |
| --- | ---: | ---: | ---: |
| plain / 10 | 10 / 10 | 1,736 | 66,248 |
| plain / 100 | 100 / 100 | 18,791 | 809,063 |
| plain / 1,000 | 1,000 / 1,000 | 189,341 | 8,237,213 |
| replay / 10 | 10 / 10 | 3,916 | 100,684 |
| replay / 100 | 100 / 100 | 45,586 | 1,230,994 |
| replay / 1,000 | 1,000 / 1,000 | 463,186 | 12,534,994 |
| mixed / 10 | 20 / 19 | 5,849 | 94,553 |
| mixed / 100 | 110 / 104 | 34,099 | 566,323 |
| mixed / 1,000 | 1,010 / 954 | 317,649 | 5,285,073 |
| images / 10 | 10 / 10 | 4,988 | 101,756 |
| images / 100 | 100 / 100 | 58,718 | 1,244,126 |

Mixed checkpoints intentionally replace active model history. Their snapshot
cost therefore does **not** represent cloning every mixed durable record;
replay-heavy histories without checkpoints isolate that scaling cost.

## Baseline layout

Sizes are inline `size_of`, not reachable heap totals.

| Type | Bytes |
| --- | ---: |
| TranscriptItem | 440 |
| Message | 48 |
| ProviderReplay | 104 |
| ReplayBackedMessage (including display binding) | 176 |
| OwnedModelRequestItem | 152 |
| SkillInvocation | 440 |
| ToolResultMetadata | 184 |
| PlanRecord | 152 |
| EnsembleRecord | 240 |
| CompactionCheckpoint | 56 |
| WebSearchAttemptRecord | 184 |

`SkillInvocation`, not the replay ledger, determines this enum's inline size.
Boxing has **not** been justified by a clone/drop/indirection workload experiment.
No layout change should be inferred from these sizes alone.

## Baseline timing highlights

Microseconds per complete operation; brackets are Criterion's 95% interval.
Complete distributions and the other dimensions are in the companion CSV.

| Boundary (1,000 requested, 8 KiB payload) | Plain | Replay | Mixed |
| --- | ---: | ---: | ---: |
| encode | 2,365 [2,361–2,370] | 4,467 [4,456–4,478] | 2,403 [2,384–2,432] |
| decode | 1,383 [1,380–1,385] | 3,588 [3,579–3,598] | 2,277 [2,273–2,281] |
| full load | 4,242 [4,053–4,533] | 8,788 [8,750–8,831] | 5,535 [5,498–5,592] |
| validate | 2.828 [2.812–2.843] | 3.482 [3.478–3.485] | 849.9 [847.3–854.1] |
| model projection | 2.009 [1.975–2.050] | 2.007 [1.955–2.106] | 0.997 [0.996–0.997] |
| owned snapshot | 188.6 [187.0–190.1] | 1,849 [1,825–1,876] | 1.406 [1.395–1.415] |
| display reconstruction | 197.0 [194.8–199.3] | 1,171 [1,151–1,186] | 289.9 [286.6–292.6] |

Replay/1,000/128 transaction measurements: required append 2.819 ms,
required single-record batch 11.485 ms, completed batch 11.407 ms,
tail replacement 11.349 ms, mode replacement 12.054 ms, model replacement
12.161 ms. Session-level required admission was 3.845 ms and completed
admission 3.719 ms. The preparation-only measurement was 0.123 µs, and
completed accounting was 2.107 µs. These are distinct operations, not additive
components of a transaction.

For replay/100/128: full rewrite 9.374 ms [8.877–9.949], write-only
14.745 µs, staged sync 3.515 ms, rename 478.190 µs, clean directory sync
8.179 µs. Filesystem scheduling/cache effects dominate several distributions.

## Baseline allocations and RSS

Isolated allocation pass, 8 KiB text, 1,000 requested records:

| Boundary | Plain calls / requested bytes | Replay calls / requested bytes | Mixed calls / requested bytes |
| --- | ---: | ---: | ---: |
| snapshot | 2,006 / 8,528,064 | 50,908 / 27,991,347 | 75 / 10,921 |
| encode | 3,019 / 24,828,213 | 29,466 / 52,533,086 | 27,017 / 25,495,254 |
| decode | 17,988 / 10,210,035 | 68,387 / 31,886,882 | 37,768 / 11,983,207 |
| validate | 0 / 0 | 0 / 0 | 3,856 / 2,834,874 |
| restoration | 2,001 / 8,767,366 | 25,454 / 23,280,070 | 8,013 / 6,995,642 |

The pre-existing 512-item release RSS probe reported 6,399,780 serialized
native/message bytes. With zero extra snapshots: max RSS 16,515,072 bytes;
with one snapshot: 28,360,704 bytes, snapshot construction 1.630 ms. These
single observations supplement, and do not replace, Criterion measurements.

## Correctness baseline and release gates

The synthetic model-input fingerprints canonicalize **JSON object key order
only** (Cargo feature unification can enable `serde_json/preserve_order`). They
retain every native value, array order, string, tool correlation and profile.
The benchmark policy's rendered instruction set is independently pinned byte-for-byte.

The existing provider suite already checks native unknown values, tool replay,
ordered directives with both wire roles, exact cache keys, profile switches,
compaction, reconnect, HTTP/WebSocket behavior and stale-continuation fallback.
Especially important fixtures include:

- `directive_wire::engine_skill_lifecycle_is_incremental_with_both_explicit_wire_roles`
- `directive_wire::directive_only_preflight_count_and_http_use_identical_input_without_rig_fallback`
- `native_output_items_survive_capture_and_full_replay_after_reconnect`
- `chained_continuations_resend_byte_identical_instructions`
- `models::lifecycle::explicit_a_b_a_switches_send_full_websocket_input_and_clear_all_continuations`
- `run_turn_retries_missing_response_id_with_full_history_once`
- `reconnecting_a_continuation_does_not_execute_its_tool_twice`
- `configured_remote_compaction_posts_ordered_input_and_preserves_opaque_output`

These are existing deterministic transport fixtures, not a newly added combined
wire golden. Both-role coverage of the entire reconnect/compaction matrix and
an explicit cache-key equality assertion across socket replacement remain gaps.
No cache identity or tool correlation is normalized away by the new tests.

### Baseline validation actually run

- Before harness changes: core library 502 passed / 1 intentionally ignored;
  provider library 159 passed.
- `rtk cargo test --offline --workspace --all-targets`: **1,711 passed**, one
  intentionally ignored probe, 12 suites.
- `rtk cargo test --offline -p zevria-core --features test-support`: **505
  passed**, one intentionally ignored probe.
- Workspace Clippy with `-D warnings`, core all-targets Clippy with
  `test-support`, and `cargo fmt --all -- --check`: passed.
- Criterion baseline (252 measurements) and isolated allocation/RSS runs: ran.
- The configured USTC Cargo mirror failed TLS during the first dependency
  resolution attempt. All dependencies were cached; subsequent commands used
  `--offline`. The initial all-targets test caught a fingerprint depending on
  JSON map order; the corrected key-order-only canonicalization passed both
  standalone core and unified workspace builds.
- One pre-existing Clippy `useless_conversion` in the inline-web test was
  removed (an `anyhow::Error.into()` returning the same type). No production
  behavior changed for that correction.

## Reproduction

All commands go through RTK. `proxy` preserves Criterion's bench arguments and
machine-readable output; `--offline` is specific to the cached local environment.

```sh
rtk cargo fmt --all -- --check
rtk cargo test --offline --workspace --all-targets
rtk cargo test --offline -p zevria-core --features test-support
rtk cargo clippy --offline --workspace --all-targets -- -D warnings
rtk proxy cargo bench --offline -p zevria-core --bench transcript --features test-support -- --save-baseline before
rtk proxy python3 crates/core/benches/support/summarize.py before docs/transcript-performance-baseline.csv
rtk proxy cargo bench --offline -p zevria-core --bench transcript --features allocation-probe
```

The isolated RSS runs use the executable reported by this build command (its
hash/path depends on the compiler and features):

```sh
rtk proxy cargo test --offline --release -p zevria-core --lib owned_request_snapshot_memory_probe --no-run
rtk proxy env ZEVRIA_SNAPSHOT_COPIES=0 /usr/bin/time -l target/release/build/zevria-core/9c2f06eaf628d380/out/zevria_core-9c2f06eaf628d380 owned_request_snapshot_memory_probe --ignored --nocapture
rtk proxy env ZEVRIA_SNAPSHOT_COPIES=1 /usr/bin/time -l target/release/build/zevria-core/9c2f06eaf628d380/out/zevria_core-9c2f06eaf628d380 owned_request_snapshot_memory_probe --ignored --nocapture
```

For a new stage, use `--save-baseline <stage>` and export a separate CSV. Never
replace the original baseline or compare changed logical fixtures silently.

## Staged result: trusted replay ownership (pre-format-switch)

This is a compiling **partial type/ownership stage**, not the completed root
format refactor. The root codec, root discovery/recovery, live storage and its
old ordinary-message variants are still unchanged. `MessageRecord` now owns
completed responses, with a consuming conversion into the current transcript
variants; unifying those durable variants is still required at the coupled
live/record/codec stage. No dual-format reader or migration machinery was added.

Implemented:

- `provider_replay::ReplayMessage` is the one private-field canonical/native
  pair. Construction consumes replay and derives the message once. Borrowed
  requests reference the pair; owned requests clone it without decoding again.
  The duplicate `ReplayBackedModelRequestItem` was removed.
- `ModelResponse` no longer accepts or exposes independently supplied
  message/replay halves. It privately owns a validated `MessageRecord`.
  Native completion validation and display binding precede WebSocket
  continuation installation. Core admission moves the trusted content directly.
- Plain `MessageRecord` construction rejects system messages and validates
  images. Display binding is assistant-only and moves content, rather than
  cloning a message or native ledger. Owned extraction can drop unused replay.
- HTTP completion transfers the captured native output rather than cloning it.
  WebSocket completion still needs independently owned continuation output.
- Owned checkpoint request items and skill invocations serialize through
  borrowed fields, not clone-to-serialize owned mirrors. Their schemas and the
  current root schema are unchanged.
- Scoped thread-local test instrumentation proves **one** canonical derivation
  through completion → binding → owned snapshot → admission, **one** when
  decoding the resulting root record, and **zero** during trusted clones and
  serialization. It is not compiled into production or benchmark libraries.
  Pointer-identity tests verify display binding moves backing allocations.

### Staged timings

Same logical fixtures, same build profile, and the same 252-boundary matrix.
`transcript-performance-trusted.csv` retains the entire initial rerun. A
50-sample, 1-second warmup / 3-second measurement targeted repeat is retained
separately in `transcript-performance-trusted-repeat.csv` (three measurements).
Units below are microseconds per operation.

| Boundary | Baseline | Trusted stage | Longer targeted repeat |
| --- | ---: | ---: | ---: |
| replay/1,000/8,192 snapshot | 1,849 [1,825–1,876] | 1,204 [1,188–1,221] | 1,127 [1,120–1,134] |
| replay/1,000/8,192 owned serialization | 4,845 | 4,193 [3,996–4,567] | 4,025 [4,011–4,041] |
| plain/1,000/8,192 snapshot | 188.6 | 191.1 [187.8–195.7] | not repeated |
| mixed/1,000/8,192 root serialization | 2,403 | 2,256 [2,243–2,282] | not repeated |
| mixed/1,000/8,192 restoration | 289.9 [286.6–292.6] | 323.7 [297.2–358.0] | 291.9 [290.3–293.8] |

The initial mixed-restoration increase was not reproduced in the longer run;
restoration implementation/allocation counts are unchanged in this stage. This
is why the initial result was retained rather than silently replaced. Small
changes to unmodified decode/load/validation paths should not be attributed to
this optimization. There is no aggregate end-to-end speedup claim, and final
representative regression acceptance remains open until the whole refactor is
implemented and remeasured.

### Staged allocations, layout and retained memory

Separate allocation-probe runs (1,000 requested records, 8 KiB text):

| Boundary | Baseline calls / requested bytes | Trusted stage calls / requested bytes |
| --- | ---: | ---: |
| replay snapshot | 50,908 / 27,991,347 | 25,459 / 23,024,416 |
| plain snapshot | 2,006 / 8,528,064 | 2,006 / 8,511,712 |
| mixed root serialization | 27,017 / 25,495,254 | 25,785 / 24,039,450 |
| replay restoration | 25,454 / 23,280,070 | 25,454 / 23,280,070 |
| mixed validation | 3,856 / 2,834,874 | 3,856 / 2,834,874 |

`ReplayMessage` is 152 bytes and `MessageRecord` is 176 bytes. `TranscriptItem`
remains 440 bytes, its existing replay/display wrapper remains 176 bytes, and
`OwnedModelRequestItem` remains 152 bytes. **No payload was boxed.** The later
layout experiment is still pending; these measurements do not justify boxing
by themselves.

Rebuilt release RSS probe: zero extra snapshots used 16,482,304 bytes max RSS;
one used 28,180,480 bytes with 1.136 ms snapshot construction. Thus the native
snapshot's substantial retained memory is still present. Eliminated temporary
decoding and allocation calls must not be presented as eliminating the required
owned snapshot itself. The same RSS limitations and exact executable commands
from the baseline section apply.

### Staged verification and reproduction

- Workspace all-targets tests: **1,714 passed**, one intentionally ignored probe.
- Core `test-support`: **508 passed**, one intentionally ignored probe.
- Provider library: **159 passed**, including existing directive, profile,
  compaction, reconnect, stale-continuation, search and cache-prefix tests.
- Workspace all-targets Clippy `-D warnings`, allocation-probe bench Clippy and
  formatting: passed. The opt-in allocator build exposed an unnecessary
  `return` in the new harness; it was removed and Clippy rerun successfully.
- All four deterministic model-input fingerprints and fixed instruction bytes
  match the baseline. The asynchronous display-checkpoint and persistence
  failure tests are included in the passing core suite.

```sh
rtk proxy cargo bench --offline -p zevria-core --bench transcript --features test-support -- --save-baseline trusted
rtk proxy python3 crates/core/benches/support/summarize.py trusted docs/transcript-performance-trusted.csv
rtk proxy cargo bench --offline -p zevria-core --bench transcript --features allocation-probe
rtk proxy cargo bench --offline -p zevria-core --bench transcript --features test-support -- 'mixed/1000/8192/restore|replay/1000/8192/(snapshot|owned_encode)' --sample-size 50 --warm-up-time 1 --measurement-time 3 --save-baseline trusted-repeat
rtk proxy python3 crates/core/benches/support/summarize.py trusted-repeat docs/transcript-performance-trusted-repeat.csv
rtk cargo clippy --offline -p zevria-core --bench transcript --features allocation-probe -- -D warnings
```

## Outstanding approved implementation (not release-complete)

1. Finish the transcript module split, durable-only `TranscriptItem`, ordered
   internal `ConversationItem`, unified durable `MessageRecord`, and validated
   `ToolResultBatch`. The consuming record conversion is an intermediate
   staging boundary, not the final durable representation.
2. Switch the root codec, strict duplicate-aware decoding, conservative torn-tail
   classification, discovery/latest-session handling, bounded previews and
   format documentation together. Retain the existing format until that
   revision can pass the complete recovery/durability matrix.
3. Replace retained-history proposal/estimate clones with repeatable borrowed
   views; preserve append versus staged-batch and post-rename commit contracts.
4. Share borrowed reconstruction analysis between ACP and consuming TUI
   restoration; remove redundant runtime handoff copies, retaining the explicit
   independent engine/frontend copy where required.
5. Complete outstanding baseline boundaries/wire-role matrix gaps, staged codec,
   copy-removal and restoration measurements, layout experiments and final
   full-plan release validation. No unrun gate is marked as passing here.

## Remaining costs and deliberately deferred work

Owned asynchronous request snapshots remain necessary while display checkpoints
mutate the live conversation. Owned frontend events and independent engine/UI
restoration state also require real ownership. Full reducer-state construction,
provider-native/portable preparation, canonical message construction on initial
replay admission, full-file atomic batch rewrites and filesystem synchronization
remain real costs even after redundant clones are removed.

Blanket `Arc` conversion, whole-request caches, incremental lifecycle reducers,
adaptive-compaction changes, transactional append-log redesign and a new
frontend event protocol are explicitly out of scope.
