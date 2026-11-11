# Session lifecycle performance (2026-11-10)

This change implements lifecycle phases 0–5. Optional phase 6 (background
WebSocket reconnect) is deferred. The older transcript and prompt-admission
reports describe different historical boundaries; their numbers are not updated
or reinterpreted here.

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

## Measurements

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
