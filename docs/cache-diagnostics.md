# Temporary cache diagnostics (remove before release)

This is an **opt-in investigation tool**, not a cache-retention fix. No live API
reproduction was performed. Never issue paid requests, warm a cache, or disturb
an active session to test it without separate authorization.

## September 25 partial reuse incident: intact local prefix, upstream cause unknown

Approved investigation baseline: session `0ada7757-bdcb-4fdb-bb29-691ec5da8ba1`,
its local JSONL, and `~/.zevria/logs/zevria.log`, principally lines **19251–19350**,
runtime `5e8442d8-db33-4157-8c60-848b97885014`. These findings were supplied by the
read-only planning investigation; implementation does not copy or replay the
private transcript. All completion times below are **September 25, 2026, UTC**.

| Completed | Transmission | Input | Cached | Cached/input | Hosted-search actions |
| --- | --- | ---: | ---: | ---: | ---: |
| 04:29:26 | Initial full request | 9,572 | 3,328 | 34.8% | 0 |
| 04:32:41 | Incremental, same socket | 41,056 | 8,448 | 20.6% | 7 |
| 04:42:20 | Full replay after reconnect | 71,048 | 10,496 | 14.8% | 8 |
| 04:50:36 | Full replay after reconnect | 32,515 | 13,568 | 41.7% | 2 |
| 04:55:07 | Full replay after reconnect | 43,565 | 20,736 | 47.6% | 1 |
| 05:06:07 | Full replay after reconnect | 132,325 | 37,120 | 28.1% | 15 |
| 05:08:09 | Incremental, same socket | 127,104 | 42,240 | 33.2% | 0 |

Weighted session total: **135,936 / 457,185 = 29.7%**. This calculated aggregate
is not the TUI's last-response statistic. The 33 actions include `search`,
`open_page`, and `find_in_page`; they are not 33 independently priced queries.

Planning independently reconstructed **all seven prepared-input hashes** with
the inspected native projection and fingerprint algorithms; every hash matched.
Each follow-up exactly extended the previous input plus native output. Input item
counts were `1, 5, 24, 52, 68, 79, 133`. Instructions, tools, other request
properties, and transmitted cache-key fingerprints were unchanged, and every
preparation-to-wire check passed. Raw usage matched projected usage throughout;
cached counters were present and cache-write counters separately reported zero.
All 89 reasoning items retained encrypted content; same-profile replay preserved
raw native items, not rebuilt assistant text.

Absolute cached counts increased on every turn. Reconnects occurred on turns
3–6, but turns 2 and 7 also had low fractions on unchanged sockets. The final
turn performed **no new search**. Neither reconnect nor new search alone is an
established explanation, and there is no demonstrated Zevria prefix-preservation
bug. The confirmed defect was observability: the former unexplained-miss flag
required `Present(0)` and said nothing about partial reuse.

OpenAI caches a rendered prefix; matching local input cannot prove matching
provider-internal rendering, cache placement, or gateway forwarding. Retrieved
search content can add input work, but no trustworthy token breakdown exists for
this incident. **Do not subtract an estimated search allowance or label all
uncached input as search overhead.** See the official [prompt caching guide],
[web search guide], and [search pricing]. Internal context treatment and upstream
cache decisions remain unresolved. Search behavior, reasoning, model, cache keys,
and replay policy must not be changed merely to improve the percentage.

`cache_diagnostics/seven_turns.json` and `partial_tests.rs` generate a seven-turn
regression with these item-count, partial-usage, and transport shapes. Reasoning
and search items, IDs, text, and opaque values are synthetic. **All usage is
mocked, not evidence of actual provider caching.** No real searches, operational
output, credentials, or encrypted blobs are committed.

## September 22 resume incident: verified local prefix, unexplained reported zero

The approved investigation examined session
`b1965f1a-8cbe-4d7a-9ca9-9eabd5e644f1` and lines **19316–19337** of
`~/.zevria/logs/zevria.log` (September 22, UTC):

| Boundary | Time | Transport / request | Input tokens | Cached tokens |
| --- | --- | --- | ---: | ---: |
| Final completion before shutdown | 11:44:52 | WebSocket incremental, native replay | 15,820 | 14,848 |
| Resumed `ok` completion | 11:45:05 | New WebSocket, full native replay | 15,979 | 0 |

Both requests used `openai/gpt-5.6-luna`, the same cache-key fingerprint, and
unchanged instruction, tool, and remaining-property hashes. The session restored
its model; a changed global Build default did not change this request. The
investigation independently reconstructed the Responses input from JSONL using
the inspected projection/fingerprint algorithms: **all 11 historical prepared
request hashes matched**. The previous request's 44 items were an unchanged
prefix of the resumed request's 47 items. The only additions were two native
output items from the final response and the user message `ok`. The skill
directive stayed in its original position.

This is **not an observed local prefix-rewriting defect**. Resume necessarily
loses socket-scoped continuation and sends full history without
`previous_response_id`. That transition is proven, but its causal relationship
to the reported zero caching is not. There is no justified cache-policy fix here.

The new synthetic resume regression models this boundary: **46 previous
input-plus-output items match, followed by one `ok` item**, unchanged properties,
a new runtime, a persisted baseline, and no continuation ID. Its 14,848 → 0
usage is mocked, not evidence about real provider caching.

Historical logs cannot supply wire/response metadata that was never recorded.
No snapshot for the original incident is fabricated. Establish a new completed
baseline before a user-operated reproduction.

### Earlier September 19 reconnect evidence (unchanged)

The earlier incident boundary is session
`98e2293a-62b9-4f28-8e46-4c4d106dfa79`, JSONL line 547. The following are
**September 19, 2026, UTC**, from `~/.zevria/logs/zevria.log`:

| Completion | Time | Input tokens | Cached tokens | Log line |
| --- | --- | ---: | ---: | ---: |
| Last completion before the question | 17:14:51 | 121,909 | 121,472 | 52469 |
| First completion after it, full replay | 17:27:30 | 122,407 | 3,968 | 52474 |
| Next incremental continuation | 17:27:33 | 123,307 | 122,112 | 52476 |

Lines 52470–52473 record `read_error`, `idle_ms=741674`, socket generation 3 → 4,
continuation invalidation, and all 439 input items being sent. The model remained
`openai/gpt-6-astra`, and the cache-key fingerprint remained
`sha256:bf984ac5b907f625`. An earlier full replay (52463) retained 120,960 of
121,307 input tokens in cache.

The finding remains **disconnect → reconnect/full replay → one-response cache-hit
drop**, not a demonstrated prefix-rewriting bug. Historical request envelopes,
prefix fingerprints, and upstream placement evidence are absent. Reconnect alone
is not sufficient to explain the drop. `idle_ms` is time since the last completed
request, **not the time of socket death**. Neither a cache TTL nor a routing change
can be inferred from these logs.

## Enable explicitly

`cache-diagnostics` is default-off on all three packages. The CLI forwards to
`zevria-app`, which forwards to `zevria-provider`. The provider's `libc` and
`uuid` dependency edges are optional and enabled only by this feature (both
packages already existed in the workspace).

```sh
rtk cargo build -p zevria --features cache-diagnostics
rtk cargo build --release -p zevria --features cache-diagnostics
# Only in a separately authorized session:
rtk proxy env RUST_LOG=info cargo run -p zevria --features cache-diagnostics
rtk proxy env RUST_LOG=info cargo run --release -p zevria --features cache-diagnostics
```

The existing logging setup honors `RUST_LOG`. Events from
`zevria_provider::cache_diagnostics` use `diagnostic_event` and the message
`cache diagnostics`. Existing returned-identifier log fields are also bounded
in diagnostic builds; their operational ID/header values are not changed.

**Release profile does not disable the feature.** `--all-features` deliberately
includes it. Use an ordinary build without the feature for a diagnostics-free
binary while this temporary code exists:

```sh
rtk cargo build --release -p zevria
rtk proxy cargo tree -p zevria -e features,no-dev -i zevria-provider
```

Without the feature, the new module, provider/request fields, sidecar allocation,
hashing, persistence, UUID generation, locks, clocks, and diagnostic event calls
are not compiled. Gates cover
argument construction too. Existing operational cache-key hashing is unchanged.
The test-only probe is compiled into unit tests, not the application; its two
tests are ignored unless explicitly selected, and its allocation counter is
inactive outside the preparation probe.

## Interpret the evidence

- `prepared`: one snapshot **after final preparation and budget admission** for
  each logical completion. Join **`runtime_id`, `local_request_id`, profile**;
  request numbers and socket generation alone are not unique across restarts.
  A fresh runtime UUID is shared by its lazy profile slots and is never sent
  upstream. `baseline_source=memory|persisted|none` and
  `missing_baseline_reason` distinguish a loaded baseline from unknown/rejected
  evidence. Token counting and remote compaction do not dispatch or promote.
- `transmission`: the same local ID on every send, including a stale-ID full
  replay, reconnect, or HTTP retry. Reports full versus incremental input count.
  `transmission_number` correlates retries. `full_replay_reason` distinguishes
  fresh runtime/no continuation, reconnect, property/history/native-output
  mismatch, stale ID, HTTP retry, and fallback. `wire_bytes` and `wire_hash` cover
  the **exact serialized WebSocket text or built HTTP body**, not a second
  serialization. `transmitted_cache_key_fingerprint` fingerprints the actual
  serialized field (including absence/null), not merely the configured key;
  unavailable inspection yields `None`, not a fingerprint of assumed absence.
  This full, presence-aware canonical-JSON digest differs from the existing
  abbreviated raw-string `prompt_cache_key_fingerprint` encoding. Echo digests
  use the same presence-aware encoding as the corresponding request fields.
  `preparation_to_wire_consistent` checks input, properties, framing and chosen
  continuation against preparation; `None` is unknown, not a match. JSON
  inspection is capped at 16 MiB; larger outgoing bodies are still hashed fully.
  HTTP and WebSocket payloads are not rewritten by the diagnostic canonicalizer.
- `connection`: runtime/profile/socket identity, selected transport, redacted
  endpoint **origin only**, normalized bounded routing-header name and value
  fingerprint, full configured endpoint fingerprint, and handshake `x-request-id`
  or explicit absence. It does not expose a header map, cookies, URL credentials,
  path, query or fragment. Routing fields describe configuration, not proof of
  gateway-internal routing. `routing_metadata_changed` compares these fingerprints.
- `raw_terminal`: an allowlist read before SDK projection or existing duplicate
  suppression. Records event type, response ID, returned model/status/service tier,
  input/cached/cache-write/output/total counters, and presence/fingerprints of
  echoed instructions/tools/cache key. Counters distinguish `Missing`, `Null`,
  `Malformed`, and `Present(0)`. Oversized response inspection emits
  `response_observation_unavailable`; missing evidence is never treated as zero.
  The eight-entry terminal ledger attributes late `response.done` to its own
  response/request ID and records `duplicate_counters_differ`. It does not consume
  extra events, publish usage twice, or promote a second baseline. HTTP events
  after the existing completion return are deliberately not drained.
- `reported_input`: raw input, cached, and cache-write counters retain their
  individual availability. `derived_uncached_input` is **input minus cached**,
  not a provider-returned field, and is absent unless both are valid and cached
  does not exceed input. `cached_input_fraction` is absent for unavailable or
  invalid counters and for zero input. Arithmetic categories are `Unavailable`,
  `ZeroInput`, `CachedExceedsInput`, `ZeroCached`, `Partial`, and `FullyCached`.
  Even `FullyCached` describes only reported input, not a confirmed cache hit.
- `provider_comparison`: the raw terminal's `prompt_cache_diagnostics` object,
  observed on both transports before SDK projection. `outcome` distinguishes
  `Absent`, `Null`, `Malformed`, `CacheHit`, `CacheMiss`,
  `ComparisonResponseNotFound`, `Unavailable`, and `Unknown`. Recognized reason
  codes use a fixed enum; arbitrary/future strings become `Unknown`, never raw
  text (even if identifier-shaped). `comparison_reusable_tokens` and
  `cache_missed_tokens` use the same presence-aware counters but are **provider
  estimates, not usage**. A missing/malformed type is `Malformed`; a recognized
  miss type retains malformed/missing reason and counter states and is not
  conclusive. A reported hit with unexpected known reason/estimate fields also
  remains inconclusive rather than hiding contradictory/malformed evidence.
  Unknown reasons are conservatively inconclusive. This record uses
  the same duplicate/late owner as raw usage and reports
  `duplicate_comparison_differs`; duplicates cannot replace the accepted result.
- `native_context`: content-free `historical_native` counts from the full
  prepared input and `current_native` counts from validated native output.
  Each counts reasoning items and search/open-page/find-in-page actions, with
  unknown/missing search actions counted separately as `other_search`. These are
  item/action counts, **not tokens or billable search queries**. URLs, queries,
  reasoning text, and opaque payloads are never recorded. The explicit operator
  comparison ID is represented by `provider_comparison_requested` presence/hash;
  it is independent of the local previous-completion baseline.
- `terminal`: normalized status/usage and available upstream response/request
  IDs, before native validation. A `completed` terminal alone does **not** prove
  that a new baseline was accepted. `usage_projection_difference` compares raw
  counters with the usage actually delivered to the frontend, without changing
  that delivery. Missing cached usage projected as zero is not a raw cache miss.
  Non-native errors may instead end only in an
  `attempt_finished` or `socket_recovery` event.
- `attempt_finished`: `baseline_promoted=true` only after complete native replay
  validation and successful search finalization. Invalid, failed, incomplete,
  and partial attempts preserve the prior baseline. Cancellation does not replace
  it. A future dropped during cancellation need not emit an attempt-finished event.
- `reset`: clears the baseline, increments the local epoch, and names
  `engine_reset` (or a defensive `profile_changed`). It writes a reset tombstone,
  including for uninitialized lazy slots; a cleared baseline cannot be reloaded
  within that runtime even if persistence fails. Profiles do not inherit each
  other's baselines. Reconnect/fallback invalidate operational continuation as
  before but do **not** clear diagnostic history.
- `comparison_summary` and `comparison_identity`: join previous/current runtime
  and request IDs, baseline age/timestamps, separate previous input/output counts,
  matched items/first difference/kinds, property/routing changes, connection/mode,
  wire consistency, previous/current raw and normalized cache usage, returned
  models and available provider IDs. The identity fields are a separate bounded
  event. Evidence flags include `local_input_changed`,
  `request_properties_changed`, `wire_projection_mismatch`,
  `usage_projection_difference`, and
  `provider_reported_miss_with_unchanged_local_prefix`. The latter now requires an
  actual provider `CacheMiss` outcome; a raw zero is not sufficient. Field validity
  is separately reported by `provider_comparison_conclusive`.
  `partial_reuse_unexplained` remains true for partial usage without a conclusive
  upstream comparison, even when some caching occurred. Only with matching
  routing metadata and **both previous/current wire consistency verified**, an
  unchanged emitted prefix and unexplained zero/partial caching mark
  `unresolved_beyond_client_boundary=true`. This is an investigation flag, not a
  conclusion that prefix tokens were missed: new input may be uncached normally.
  Missing prior wire evidence is not assumed consistent; a known previous wire
  mismatch remains a client discrepancy. A comparison hit does not mean every
  input token was reused, and does not erase local or transport discrepancies.

The baseline is fingerprints of **the previous completed full input plus its
native response output**, in order. Comparison reports baseline count, matched
count, first differing zero-based index, and bounded old/new item-kind enums:

| Prefix status | Meaning |
| --- | --- |
| `unknown` | No completed baseline; inspect the missing-baseline reason. Never a match. |
| `exact_extension` | Every baseline item matches and more input was appended. |
| `exact_equal` | Same complete baseline; no new suffix. Not itself permission to continue. |
| `mismatch` | An earlier item differs; inspect the first index and kinds. |
| `truncated` | Input ends before the baseline ends; first index is the first missing item. |

`instructions_changed`, `tools_changed`, and `remaining_properties_changed` are
separate comparisons (`None` when unknown). The comparison projection excludes
**only** `prompt_cache_options.comparison_response_id`. Empty options are removed
only if removing that leaf made them empty; pre-existing empty/null/malformed
options and every sibling remain significant. Exact-wire verification and hashes
still include the real comparison ID. `full_input_hash` changing after an
append is normal, not a prefix mismatch. Object keys alone are sorted for hashing;
array order, IDs, strings, opaque native values, and exact tool-argument strings
are preserved. The ordered input hash is a versioned digest of compact item hashes,
not a checksum of a rewritten outgoing JSON request. The complete rendered
instruction set is stable across ordinary turns and skill activation/revocation
within one workflow, so those transitions report `instructions_changed=Some(false)`.
Standard → explicitly orchestrated → Standard Build requests keep those fixed
bytes and tool order unchanged, reporting `instructions_changed=Some(false)`.
Activation, the single corrective continuation if needed, and the next Standard
boundary are append-only typed request inputs, not instruction replacements or
skill/catalog mutations. Incremental requests can therefore remain suffix-only;
reconnect sends the same full logical prefix, without relying on the old socket's
continuation ID. A real Build↔Plan policy change still changes instructions and
may require full replay; unchanged tools alone do not prove an unchanged prefix.

A low cached-token count with an exact local prefix means **the local native input
matches**. It does not prove upstream tokenization, cache availability, retention,
routing, placement, or reuse. The high → low → high test uses mocked usage and is
not evidence about a real upstream cache.

### Socket timing and privacy

`socket_recovery` reports the operational terminal category separately from the
sidecar's observed category/cause. `termination_age_ms` uses a monotonic observation
time; `since_last_completion_ms` is the old idle measure. `observation_source` is
`pump`, `local` (for example a locally imposed timeout), or `inferred`. Inference
has no invented termination time. Even a direct observation is not necessarily
the instant a remote peer died.

The optional sidecar records errors **before** moving them into the existing
inbound queue. Queue-overflow/closed-queue precedence, the terminal enum/watch,
and pump shutdown remain unchanged. There is no new background task or polling.
Only a bounded error class and optional `std::io::ErrorKind` are retained; no error
body or close reason is stored. Preflight can observe the cause without dequeueing.

New events contain no prompt, result text, tool argument, opaque/encrypted replay,
credential, raw cache key, headers, or arbitrary server/close text. IDs and profile
labels are bounded/single-line; unrecognized or oversized IDs are redacted. Item
kinds come from a fixed enum. Events have a fixed set of fields regardless of
history size; per-item fingerprints are retained locally, not dumped to logs.
Fingerprint equality still exposes equality information: handle logs as sensitive
operational evidence, not as public telemetry. Existing logging outside this
module has not been audited or changed by this work.

## Snapshot lifecycle and privacy

Snapshots are adjacent to the **actual transcript path**, including nondefault
resume paths and the worker `ensemble-sessions` namespace:

```text
<transcript-parent>/.cache-diagnostics/<session-digest>-<profile-digest>.json
```

Normal/worker application startup passes this optional context through the lazy
router. Alternate composition roots without a context retain memory-only
observations. Loading never restores a conversation, tools, cache key,
`previous_response_id`, or operational continuation. Only the latest validated
provider completion per session/profile is retained. It is **not proof that the
response was durably appended to JSONL**. Crashes, history edits, compaction, or a
changed binary can legitimately yield a mismatch; use provenance and timestamps.

The **v2** document holds session/profile fingerprints, runtime/request IDs,
property and ordered item fingerprints with bounded kind enums, separate input
and output counts, completion time, transport/mode/connection metadata, and the
bounded raw/normalized observations, native counts, and allowlisted provider
comparison outcomes/reasons/estimates. Version is checked before the current
schema is decoded; incompatible v1 baselines are explicitly rejected with
`snapshot_version`, not migrated or treated as a current baseline.
There are no prompts, tool results,
encrypted content, authorization headers, cookies, raw bodies or raw cache keys.
Fingerprints expose equality and IDs can identify sessions: treat **both logs
and snapshots as sensitive operational evidence**.

Limits are **4 MiB and 32,768 input-plus-output items**. Exceeding a cap invalidates
the latest diagnostic baseline instead of presenting a truncated prefix as
complete. Missing/corrupt/version/identity/permission/symlink/cap failures have
bounded reason codes and are nonfatal. Current in-memory completions remain
available after ordinary disk I/O failures; failed, partial, cancelled, counting,
and compaction attempts do not replace a completed baseline.

On Unix, storage uses owner-only directories (`0700`) and files (`0600`),
descriptor-relative no-follow reads/staging/replacement, regular-file checks,
hard-link rejection, bounded reads, and file/directory sync. Existing unsafe
permissions are rejected, not silently relaxed. A private sibling staging file
is atomically renamed; a crash before publication leaves the old snapshot intact.
An orphan `.tmp` file is never considered a baseline. On other platforms the
persistence backend reports `storage_platform_unsupported` and stays memory-only
rather than weakening these safeguards.

Reset/cap tombstones prevent disk resurrection when writable. If the filesystem
rejects **all** invalidation writes, no client can durably record that reset:
`persistence_error` reports the failure, the running router still suppresses
reload, and an old snapshot may remain for a later runtime. Remove it manually
before collecting a fresh baseline in that case. Similarly, an unsuccessful
save can leave the prior on-disk timestamp, never an assumed new completion.

To remove diagnostic state, quit the session and remove its adjacent
`.cache-diagnostics` directory (or the selected digest-named snapshot). This does
not modify JSONL. Do not remove live transcript/lease files. Restart to discard
in-memory state too. There is no automatic age-based eviction or cache warming.

## User-operated bounded reproduction and evidence bundle

**Separate authorization is required before any live call**: agree on the endpoint,
model/profile, exact synthetic content, total request count (including retries and
maintenance), output/search limits, and a cost ceiling. Do not replay either
private incident, prewarm caches, alter gateway routing, or run paid probes during
ordinary implementation/testing. Stop at the budget or an unexpected retry/tool
call; do not silently extend the experiment.

One proposed bounded experiment is **six completion requests**, subject to that
approval. Use an operator-controlled driver when exact repeated logical input or
a deliberate transport comparison cannot be expressed by the interactive UI.
No probe driver, automatic baseline selection, or extra requests are installed by
this implementation. Preserve model, reasoning, tools, instructions, cache key,
cache mode/retention, and routing metadata throughout.

1. Build with `rtk cargo build -p zevria --features cache-diagnostics` and enable
   info logging. Prepare a stable synthetic prefix long enough for the selected
   model's cache eligibility. Advertise the same tools throughout, but direct the
   initial control to answer without searching. **Request 1** establishes a
   recent completed baseline. Verify `baseline_promoted=true` and zero current
   native search actions; unexpected searching makes it unsuitable as a control.
2. Explicitly supply that response ID through
   `additional_params.prompt_cache_options.comparison_response_id` (example in
   `docs/responses-compatible.md`). **Requests 2 and 3** repeat the stable logical
   input/no-search control. Retain all other request properties and record every
   wire body fingerprint, raw usage, and provider comparison. The ID may remain
   fixed for the experiment; Zevria never updates it automatically. If a more
   recent baseline is required, stop and obtain approval for any extra calls.
3. **Request 4** appends a bounded synthetic research task using approved public
   content. **Request 5** follows it on the same socket without requesting new
   searches. Preserve the complete native input/output sequence; record actual
   search and reasoning counts rather than assuming compliance or estimating
   search tokens. Confirm the request really used incremental continuation.
4. **Request 6** sends the **same complete logical input as request 5**, on a new
   socket or the already approved HTTP path, without `previous_response_id`.
   The explicit diagnostic comparison ID and every cache-sensitive setting stay
   fixed. The operator must arrange this controlled replay, not silently change
   application reconnect policy. Comparing unlike histories is not a transport
   control. Transport results remain subject to provider/gateway variability.
5. Collect `prepared`, every `connection`/`transmission`/`socket_recovery`,
   `raw_terminal`, `reported_input`, `provider_comparison`, `native_context`,
   `terminal`, `comparison_summary`, `comparison_identity`, and
   `attempt_finished`. Include UTC timestamps, baseline age/provenance, runtime,
   profile, local request ID, returned model, both response IDs, available HTTP
   or WebSocket-handshake `x-request-id`s (or explicit absence), socket generation,
   request mode, and content-free fingerprints. The provider's explicit baseline
   need not equal the immediately previous local completion; record which was
   supplied. An opaque gateway route cannot be inferred from response-ID syntax.

Interpret the result before proposing policy changes:

- A demonstrated input/settings/wire difference warrants a targeted regression
  and a fix in the responsible client or gateway transformation.
- A conclusive comparison hit with a low overall fraction is compatible with new
  input work. Document that accounting; do not rewrite correct native replay or
  assume a hit means all reported input was cached.
- Missing/rejected diagnostics, expired baselines, malformed/future outcomes, or
  `Unavailable` leave an evidence gap. Ask the operator for forwarding/routing
  evidence; never turn missing support into success or invent a cache-policy fix.
- A provider miss gives an upstream reason/estimate, not a replacement usage
  counter. Correlate it with local evidence before acting. Changed routing,
  keepalives, cache policy, search depth, reasoning, or model need a follow-up
  decision; this implementation authorizes none of them.

The official [comparison diagnostics guide] describes the comparison as
non-history-loading and caching-neutral. It is best effort; diagnostic records
expire, and compatible gateway syntax does not guarantee support or forwarding.
No live provider calls were made for these tests. All automated network tests use
loopback synthetic servers, not caches or search services.

[prompt caching guide]: https://developers.openai.com/api/docs/guides/prompt-caching
[web search guide]: https://developers.openai.com/api/docs/guides/tools-web-search
[search pricing]: https://developers.openai.com/api/docs/pricing
[comparison diagnostics guide]: https://developers.openai.com/api/docs/guides/prompt-caching/diagnostics

## Historical offline probe and measured overhead (before resume extension)

The figures below predate persisted baselines, exact-wire hashing, raw observation
and the expanded metadata. They are **not a current end-to-end overhead estimate**.
Re-run the probe to measure current preparation, and separately measure terminal
observation and synchronous persistence if those costs matter.

```sh
# Run alone. The proxy preserves the bounded CSV measurement output.
rtk proxy cargo test --release -p zevria-provider cache_preparation_probe::cache_preparation_probe -- --ignored --nocapture --test-threads=1
rtk proxy cargo test --release -p zevria-provider --features cache-diagnostics cache_preparation_probe::cache_preparation_probe -- --ignored --nocapture --test-threads=1
```

Measured September 19, 2026 on Apple M4/16 GiB, `aarch64-apple-darwin`,
`rustc 1.100.0-nightly (fb6531d55 2026-08-23)` and
`cargo 1.100.0-nightly (e8cb624d5 2026-08-22)`, matched optimized release test builds.
After building both configurations, three paired off/on **prebuilt executable**
runs were alternated, with no concurrent build. Raw medians, p95s and allocation
counts are in `crates/provider/src/cache_diagnostics/measurements.csv`.
Initial post-compilation runs were noisier (particularly for small inputs); the
CSV is the explicit paired series, not a claim of long-term stability.

Each process uses 20 warmups and 100 timed samples per operation, then one separate
allocation-counted sample. Timings include request/snapshot destruction. Synthetic
history construction is excluded. The probe calls actual preparation and actual
transmission selection/derivation, but **not wire serialization, network I/O,
completion-output hashing/promotion, or event formatting/log I/O**. No subscriber
is installed. Feature-on figures therefore do not price the entire enabled logging
pipeline. Full-input hashing and comparison still run with no subscriber.

The small input has 7 items/1,424 projected JSON bytes. The large input has 439
items/516,344 JSON bytes, with about 488k synthetic text/opaque bytes: roughly 122k
tokens only by a **four-bytes-per-token sizing proxy**, not measured tokenization.
It includes native opaque reasoning and native assistant output. These fixtures
contain no private transcript or API content.

Median of the three per-process medians, microseconds:

| Items | Operation | Off | On |
| ---: | --- | ---: | ---: |
| 7 | Preparation only | 13.500 | 15.125 |
| 7 | Preparation + incremental selection | 13.458 | 16.208 |
| 7 | Preparation + full replay derivation | 13.458 | 16.583 |
| 7 | Full replay retry, existing snapshot | 1.208 | 1.167 |
| 439 | Preparation only | 562.291 | 925.542 |
| 439 | Preparation + incremental selection | 588.125 | 951.667 |
| 439 | Preparation + full replay derivation | 638.833 | 999.375 |
| 439 | Full replay retry, existing snapshot | 84.542 | 86.709 |

Enabled large-input preparation costs about **0.36 ms more (+65%)** in this probe;
it is not negligible. Incremental selection still fingerprints the logical full
history once. Retries reuse that snapshot: measured allocation counts/bytes are
identical off/on for retry derivation (4,179 calls/942,261 requested bytes for the
large history), with small timing variation. No retry hashing is hidden in a no-op.

Preparation-only allocations (allocation/reallocation calls, cumulative requested
bytes, **not live or peak heap**): 486/68,345 → 501/69,552 for small inputs;
22,104/7,160,153 → 22,767/7,217,088 for large inputs. The diagnostic delta is
15 calls/1,207 bytes and 663 calls/56,935 bytes respectively. Hashes stream; there
is no extra full-history or opaque-content clone in the diagnostic implementation.

Retained diagnostic sizes, inline plus owned capacities, excluding allocator
rounding: request snapshot 463/14,719 bytes and completed baseline 435/14,691 bytes
(small/large). The baseline includes only compact item hashes/kinds, property
hashes, local identity, and profile labels. The socket sidecar separately measured
one 88-byte requested heap allocation, plus 8 bytes per Arc handle (session, and
pump while connected). The process-local correlation counter is 8 bytes. These are
not RSS or peak-memory measurements, and promotion briefly holds old/new compact
baselines. Feature-off application builds retain none of this diagnostic state.

### Feature-on/off payload comparison

The ignored `cache_wire_equivalence_capture` fixture records four reconnect-flow
WebSocket sends, four **exact WebSocket text payloads** from the skill/tool/JSONL
resume flow, and an **exact HTTP body** retry pair. It also asserts outcomes and
tool counts. Run each build with a different **absolute,
disposable** output path, then compare:

```sh
rtk proxy env CACHE_DIAGNOSTICS_WIRE_OUTPUT=/absolute/scratch/wire-off.json cargo test --release -p zevria-provider cache_wire_equivalence_capture -- --ignored --test-threads=1
rtk proxy env CACHE_DIAGNOSTICS_WIRE_OUTPUT=/absolute/scratch/wire-on.json cargo test --release -p zevria-provider --features cache-diagnostics cache_wire_equivalence_capture -- --ignored --test-threads=1
rtk proxy cmp /absolute/scratch/wire-off.json /absolute/scratch/wire-on.json
```

This comparison was run successfully: captured WebSocket/HTTP payload files were
byte-identical; both executions passed the fixture's operational assertions.
After the resume extension, the debug off/on captures both had SHA-256
`79ff5a330d23506b7edf229f500d2a7ba24c329b08e644dfcd329a55250a011b`.
The resume fixture specifically checks the 44 + 2 → 47 boundary and directive
position; diagnostic tests independently compare wire hashes with captured bytes.

## Exact removal inventory

Delete these temporary assets as a unit; keep the permanent regressions separate:

- `crates/provider/src/cache_diagnostics/` in its entirety:
  `mod.rs`, `fingerprint.rs`, `persistence.rs`, `persistence_tests.rs`,
  `response.rs`, `response_tests.rs`, `socket.rs`, `tests.rs`, `lifecycle_tests.rs`,
  `partial_tests.rs`, `seven_turns.json`, `probe.rs`, `probe_support.rs`,
  `measurements.csv`, `rehearse_removal.py`.
- `crates/app/src/cache_diagnostics_runtime_tests.rs` and its gated module hook.
- This file: `docs/cache-diagnostics.md`, and the marked temporary observer
  paragraph in `docs/responses-compatible.md`. The rehearsal script removes both.
- `cache-diagnostics` definition/forwarding and their temporary comments in
  `crates/provider/Cargo.toml`, `crates/app/Cargo.toml`, `crates/zevria/Cargo.toml`.
- Every `#[cfg(feature = "cache-diagnostics")]` hook in the following files:
  - `lib.rs`: optional module declaration and `CacheDiagnosticContext` export.
  - `router.rs`: optional context, reset marker, context builder, lazy-slot
    configuration, initialized/uninitialized reset handling.
  - `connection.rs`: `OpenAiProvider.cache_diagnostics`, initializer in `connect`,
    fallback observation, and the diagnostic branch of `log_identifier`.
    Unwrap its feature-off identity branch (or remove the no-op helper/maps).
  - `turn.rs`: `PreparedRequest.cache_diagnostics`, the `None` initializer in
    `prepare_turn_request`, `log_terminal_response` observation,
    `send_prepared_request` transmission observation, `run_model_request` and
    `run_http_model_request` finish hooks, `run_http_model_request_inner`
    transmission observation, `run_model_request_with_recovery` dispatch and
    recovery observations, and `ModelProvider::reset` hook. Also remove
    `Transmission.full_reason` and its initializers, pre-projection raw-terminal
    observations, reconnect observations, and the built-HTTP diagnostic branch.
    **Unwrap** the `#[cfg(not(feature = "cache-diagnostics"))]` original HTTP
    `.send()` branch; do not remove it.
  - `websocket_session.rs`: `OpenAiWebSocketSession.cache_diagnostics`;
    sidecar local, pump argument, and struct initializer in `new_with_send_delay`;
    `disconnected` initializer; `terminate` observation; `pump_socket` optional
    parameter, send/pong/read error observations, close observation, final
    observation before watch publication.
  - `tests.rs`: optional `cache_diagnostic_lifecycle` module plus field
    initializers in **both** `test_session_with_pump` and
    `reconnecting_a_continuation_does_not_execute_its_tool_twice`.
  - `resume_wire_tests.rs`: only context attachment and snapshot assertions;
    keep the ordinary engine/JSONL/native replay regression.
  - `crates/app/src/runtime.rs`: normal and worker context attachment and gated
    diagnostic startup-test module.
- Remove provider optional `libc`/`uuid` dependency edges and their entries in
  the `zevria-provider` dependency list in `Cargo.lock`, not the workspace packages.
- The unconditional-within-tests `cache_preparation_probe` path/module inclusion
  in `tests.rs` and the `#[cfg(test)] cache_preparation_probe_support` inclusion
  (and temporary comment) at the end of `turn.rs`.

**Keep** `wire_request_properties`, `incident_prefix_flow`, the strengthened native
replay regression, second-prompt tests in provider/directive-wire/core admission,
ordinary fixtures, `isolate_log_test` and its existing operational-log test callers,
and the permanent release gates in `docs/transcript-performance.md`.
Also **keep** `prompt_cache.rs`, its `lib.rs` module, the narrowly scoped
`turn.rs` continuation/counting/compaction calls, `prompt_cache_tests.rs` and
its `tests.rs` module, and the maintenance wire assertions. These are explicit
`additional_params` interoperability, required in **both feature configurations**,
not observer infrastructure. They do not depend on the temporary module, add
requests, insert IDs, or change requests lacking the comparison field. Keep the
operator opt-in documentation apart from its marked observer paragraph. If the
comparison request capability itself is later retired, this is its complete
separate removal inventory; restore exact property equality in
`select_transmission` and remove the two maintenance filtering calls with it.
Keep other existing dependencies, including `sha2`, and the `libc`/`uuid` packages
used by other crates. Remove only the two new provider lockfile edges.
No operational decision or public schema depends on any removable asset.

### Rehearse deletion, not just feature disablement

Use an independent disposable source copy, not a Git worktree or a live session
directory. Copy source, manifests/lockfile, and ordinary fixtures; exclude `.git`,
`.zevria`, and `target`. Add an empty `.cache-diagnostics-disposable` marker. From
the original tree:

```sh
rtk proxy python3 crates/provider/src/cache_diagnostics/rehearse_removal.py /absolute/disposable/source
# Commands below execute with the disposable source as working directory:
rtk cargo fmt --all -- --check
rtk cargo test --workspace
rtk cargo build --release -p zevria
# Also run the permanent named gates in docs/transcript-performance.md.
```

The script refuses the original tree, unmarked directories, and Git worktrees. It
removes only the inventory above and checks for remaining module/feature/probe
references throughout source/manifests/docs. Review its output and diff; it is not
a substitute for tests. Give the copy its own target directory.

## Validation and pre-release checklist

### September 25, 2026 partial-reuse implementation

Synthetic data and loopback servers only; **no live caching measurement**:

- `rtk cargo test -p zevria-responses`: **6 passed**.
- `rtk cargo test -p zevria-provider`: **169 passed, 2 ignored**.
- `rtk cargo test -p zevria-provider --features cache-diagnostics`:
  **198 passed, 2 ignored**.
- `rtk cargo test -p zevria-core resumed_session_reuses_the_pre_shutdown_prefix_after_tool_activation`:
  **1 passed**.
- `rtk cargo check -p zevria --all-targets --features cache-diagnostics`: passed.
- `rtk cargo fmt --all -- --check` and `rtk git diff --check`: passed.
- Explicit debug feature-off/on `cache_wire_equivalence_capture` runs passed;
  `cmp` confirmed identical ordinary payload captures, including raw resumed
  WebSocket text and HTTP retry bodies.
- New regressions cover seven synthetic turns, partial/zero/full/unavailable and
  invalid arithmetic, every supported provider outcome/reason, malformed/null/
  missing estimates, unknown/oversized/private fields, late/duplicate comparison
  ownership, provider hit/miss versus partial usage, v2 persistence and v1 rejection,
  content-free native counts, opt-in HTTP/WebSocket bodies, comparison-only
  continuation, real option changes, maintenance filtering/non-promotion, and
  comparison-ID wire mutation detection with an exact payload hash assertion.
- Removal rehearsal in an independent marked OS-temp source copy removed all
  observer references, passed formatting, **169 provider tests**, and
  `cargo check -p zevria --all-targets`, using a separate target directory. It
  preserved explicit request interoperability and its permanent tests. A fresh
  full workspace test/release build of that copy was not run.

The two normally ignored tests are offline preparation-overhead and wire-capture
probes; the latter was invoked explicitly as described above. Existing native
replay, resume, routing headers, and tool-execution-once regressions remain in the
provider/core suites. No cache policy, search depth, reasoning setting, gateway,
`TokenUsage`, TUI arithmetic, or last-response accounting was changed. A causal
upstream explanation still requires the separately authorized operator run.

### Historical resume-extension validation

Resume extension validation (synthetic data and loopback servers only):

- `rtk cargo test -p zevria-provider --features cache-diagnostics`: **175 passed,
  2 ignored**.
- `rtk cargo test -p zevria-provider`: **158 passed, 2 ignored**.
- `rtk cargo test -p zevria-core resumed_session_reuses_the_pre_shutdown_prefix_after_tool_activation`:
  **1 passed**.
- `rtk cargo test -p zevria-app --features cache-diagnostics`: **94 passed**,
  including normal/nondefault-path and worker resume composition.
- `rtk cargo check -p zevria --features cache-diagnostics` and
  `rtk cargo check -p zevria`: passed.
- `rtk cargo fmt --all -- --check` and `rtk git diff --check`: passed.
- Removal inventory rehearsal in an independent source-only OS-temp copy removed
  all temporary references and passed formatting. The expanded deletion tree was
  not given another full workspace/release build (the older run below is historical).
- The explicitly invoked wire-equivalence capture passed in both debug feature
  configurations, with identical raw resume WebSocket and HTTP retry bodies.
- Persistence/observation tests cover profile isolation, reset (including lazy
  slots), missing/corrupt/oversized/version/identity/unsafe metadata, symlinks,
  hard links, unsafe/unwritable storage, interrupted staging, unchanged disk
  baselines on failed/partial/cancelled/maintenance attempts, raw counter states,
  late completed/done ownership, privacy sentinels, routing changes and an
  intentionally mismatched wire projection.

One local validation link failed because the disk was full. Clearing only
provider/app/core Cargo artifacts freed space; the complete matrix above then
passed. No live provider requests, cache-policy changes, or gateway edits were
performed. The old workspace/release/deletion measurements below are **historical**,
not claims that the expanded implementation reran that larger release matrix.

### Historical September 19 validation

Validated on September 19, 2026 (all tests use synthetic data/loopback servers):

- `rtk cargo test --workspace`: **1,785 passed, 5 ignored** (including the two
  intentionally ignored temporary probes).
- `rtk cargo test -p zevria-provider --features cache-diagnostics`: **164 passed,
  2 ignored**. Diagnostic-only log tests and existing operational-log assertions
  run in isolated test processes to avoid tracing callsite-interest races with
  parallel no-subscriber fixtures.
- `rtk cargo check -p zevria --all-targets --features cache-diagnostics`: passed.
- Both requested `rtk cargo build --release -p zevria` configurations passed.
  The ordinary resolved production feature graph contains no `cache-diagnostics`.
  The ordinary binary contains none of the new diagnostic event markers; the
  explicitly enabled binary contains them. No application binary was launched.
- Independent OS-temp source copy `zevria-cache-removal-e8sf6x9i`: all temporary
  files/features/hooks removed, no source/manifests/docs references remaining;
  **1,785 workspace tests passed, 3 ignored**, ordinary release build passed,
  and all four named permanent gate commands passed (five tests in total).
  The copy used its own target directory and contained no Git/session storage.
- Formatting checks passed in both trees. The removal script also normalizes the
  trailing newline left after deleting the test-only module inclusion.

- [x] Default workspace tests; feature-on provider tests; feature-on CLI all-target check.
- [x] Ordinary and explicitly enabled release builds; inspect ordinary resolved features.
- [x] Matched release preparation probes and actual retained-memory/allocation measurements.
- [x] Feature-on/off WebSocket and HTTP captured payloads equal; outcomes/tool counts pass.
- [x] Independent deletion copy: no references, default workspace tests, release build,
      and named permanent prefix regressions pass without diagnostic helpers.
- [x] Formatting and whitespace checks pass.
- [ ] Before shipping: delete the full temporary inventory and rerun permanent gates.
- [ ] Confirm release automation no longer enables or references the removed feature.

No keepalive, warming, retry-policy change, continuation reuse across sockets,
header/routing change, retention parameter, reasoning change, frontend/transcript
schema change, or public configuration change is part of this work.
