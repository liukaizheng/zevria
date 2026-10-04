# Per-Profile Responses WebSocket and HTTP/SSE Transport

`zevria-provider` implements one wire protocol: OpenAI Responses and
Responses-compatible HTTP/SSE or WebSocket. Configured provider keys identify
endpoints, not protocol adapters. Chat Completions and vendor-specific socket
dialects are outside this boundary.

## Router and single-profile runtimes

A session owns a `ResponsesRouter`. Its route table maps `ModelRole` to slots
that are deduplicated by exact `(provider key, model ID)` identity:

```text
SessionEngine<ResponsesRouter>
    │ ModelRequest { model_role, complete projected history, policy }
    ▼
ResponsesRouter
    ├── slot provider-a/model-a -> OpenAiProvider (lazy)
    ├── slot provider-a/model-b -> OpenAiProvider (lazy)
    └── slot provider-b/model-a -> OpenAiProvider (lazy)
```

Each `OpenAiProvider` is a fixed single-profile runtime containing:

- one exact wire model ID and model-specific reasoning settings;
- one endpoint, literal credential, compatibility policy, and additional map;
- an optional provider-specific session-routing header, shared by its HTTP and
  WebSocket transports;
- one 64-character SHA-256 prompt-cache key derived from length-delimited
  session ID + provider key + model ID components;
- one WebSocket generation and continuation chain;
- one profile-local sticky HTTP fallback state; and
- the selected model's physical context window and hard input ceiling, with the
  latter used for remote-compaction trimming.

Constructing a root or child router performs no network I/O. A slot initializes
on its first completion, exact input-token count, or eligible remote-compaction
request. An unavailable Plan profile therefore does not block startup or poison
an unrelated Build profile.

The root router contains Build, Plan, and Review model-role routes. Request-local
orchestration uses the Build route and saved Build selection, not a separate mode,
model role, or `modes.orchestrate` assignment. Ensemble Plan uses Plan;
Ensemble Review uses Review even though its UI workflow mode is Build.
Explore and Builder are not root role routes; their configured profiles may be
selectable by a root mode without installing child-role routes. Each child gets
a fresh single-profile Explore or Builder router with an independent transcript,
cache identity, transport, compaction lifetime, and continuation state. Both child
kinds run nominal Build turns with their dedicated roles and restricted tools.
Only an explicitly orchestrated root Build request permits native Builders;
Standard Build retains optional Explore, and Plan retains configurable Explore only. Native worker
Plan/Review is unchanged. `ResponsesRouterFactory::new(role, profile, preamble)` captures the opening
assignment; `create(session_id, tools)` receives the same child registry handle
as its engine. Root model switches never repoint either child factory.
All configurations must explicitly assign both `modes.explore` and the required
`modes.builder`, in addition to Build, Plan, and Review; there is no fallback.

## Session routing identity

`providers.<key>.session_id_header` optionally names the HTTP header that carries
this conversation's persistent session ID. Omission sends no session header;
`x-opencode-session` is an OpenCode Go configuration example, not a built-in
name. A different provider can use a different valid name, such as a documented
`session-id` header, without changing the wire protocol or request body.

The router passes the raw conversation ID to provider construction separately
from its hashed profile-cache key. The configured header is installed in both
the actual `reqwest` client's default headers and the stored WebSocket handshake
headers. Consequently, initial handshakes, reconnects, HTTP fallback/retry,
completion, exact counting, and configured remote compaction retain the same
name/value. Configuring only Rig's client would not cover the separate HTTP
request builders. Authorization and transport-owned headers cannot be replaced.

The ID does not rotate with socket generations, continuation resets,
cancellation, or model/profile changes. Resume recovers the transcript filename
stem; fresh conversations and both child kinds use their own IDs. Provider
settings choose the name independently for each endpoint, and prompt-cache
transmission can be disabled without disabling this header. No new transcript
record or migration is needed.

Name validation is shared between catalog loading and direct runtime setup.
When configured, blank or HTTP-header-invalid session-ID values fail before
network I/O; omitted configuration adds no value validation. Header values are
marked sensitive for debug formatting. Explicit auxiliary URLs also receive
the correlation ID, potentially at a different origin, and redirect behavior
is unchanged. See [session routing configuration](responses-compatible.md#session-routing-headers)
for reserved names, trust considerations, and setup examples.

## Source-aware request preparation

Core always supplies complete checkpoint-aware history. The shared, side-effect-free
`provider::replay` projection validates it before a destination slot initializes
or a model request is dispatched. Foreign replay-only input returns a structured
conversion-required preflight result and never reaches a counting endpoint.
Before transport selection, the fixed profile prepares one immutable Responses
request snapshot:

1. set `instructions` to the complete rendered engine instruction set (application,
   captured file guidance, workflow policy and pin-independent eligible catalog);
   only skill bodies and revocations remain typed ordered input;
2. select the slot's fixed model and reasoning settings;
3. advertise only the turn policy's allowed tools;
4. apply provider compatibility flags and validated additional parameters;
5. project every replay item for the target profile and every directive at its
   recorded position, using developer role unless the endpoint explicitly opts
   into user-role compatibility.

A replay whose `source_profile` exactly matches the slot contributes its native
Responses items byte-faithfully. A foreign replay contributes a deterministic
portable assistant projection: visible text/refusals and re-correlated function
calls/results survive, while reasoning, encrypted/opaque values, unknown output
types, and native IDs are dropped. A foreign replay-only opaque checkpoint is
rejected because it has no safe provider-neutral representation.

This target-specific preparation occurs before deciding whether the socket can
send only a suffix. Every reconnect, stale-ID fallback, or HTTP retry reuses the
same prepared snapshot; recovery cannot silently change model, instructions,
tools, replay projection, or gateway parameters.

A first skill activation is an append-only input change, not a request-property
change. After a tool activation the full logical input is previous input + exact
native response output + complete tool results + new body directive. The very
next WebSocket request can send only results + directive with the prior
`previous_response_id`. A direct activation similarly appends its invocation and
body; unchanged repeated application adds no body. Revocations remain append-only.
Skill activation never changes the catalog or `instructions`. Standard →
orchestrated → Standard Build requests likewise keep fixed instructions and tools:
activation, correction, and reset are append-only typed request directives, in
developer-role or compatibility-user input as configured. Existing native output
and directive positions are retained, so suffix-only continuation remains possible.
Reconnect transmits the same full logical prefix without a stale response ID.
A real Build↔Plan mode or synthesis policy switch changes `instructions` and can
require full replay. Catalog-management mutations likewise change the prefix. The session/profile
cache key stays unchanged; real tool/property/profile changes still require full replay.

## Continuation identity

A continuation records the exact boundary represented by one server response:

```text
socket_generation
response_id
request_properties
request_input
response_output
```

An incremental request is allowed only when:

- the continuation belongs to the current socket generation;
- immutable request properties are equal;
- complete native response output exists; and
- the newly prepared full input is an exact extension of the previous request
  input plus response output.

If any check fails, that slot invalidates its continuation and sends complete
history. Switching A → B → A does not move continuation state: B starts its own
chain, and the later A request may continue A's chain if the shared transcript
is an exact extension after target-specific projection.

## WebSocket lifecycle and recovery

Slot initialization performs local setup only. With `supports_websockets = true`,
the first completion establishes its socket **inside that request's attempt
budget**. Failed startup and replacement handshakes consume attempts too. A
successful socket is parked in a continuously pumped session that handles
Ping/Pong and terminal frames between model calls; parked healthy sockets have
no inactivity timer.

Each prepared completion gets **four attempts total by default**, including its
initial connection/dispatch/response attempt, across WebSocket and HTTP together.
There is at most one same-transport WebSocket recovery, with an HTTP attempt
reserved when the remaining budget permits. HTTP 426 selects sticky HTTP
immediately; HTTP-only configuration skips WebSocket entirely. A stale-ID full
resend also consumes an attempt. Fallback never starts a new budget. Authentication,
configuration, malformed protocol, semantic failure, and input-size rejection
remain terminal rather than triggering transport recovery.

Connection establishment and request-start work each default to 30-second
limits. Request start means the WebSocket write acknowledgement or HTTP response
headers. After that, one monotonic progress watchdog covers both the first event
and the whole response: warn after **30 seconds without meaningful progress**,
then abandon the attempt after **180 seconds of inactivity**. Accepted lifecycle,
text/reasoning/refusal, function arguments, output-item/content-part advancement,
and hosted-search advancement reset it. Repeated statuses without advancement,
Ping/Pong, comments, empty SSE data, `[DONE]` without a terminal response, ignored
metadata, unknown events, and prior-response events do not. Optional sequence
numbers help deduplicate events but are not required from compatible gateways.

There is **no total duration limit on a progressing completion**. Users of quiet
reasoning models can increase `providers.<name>.network.response_idle_timeout_seconds`.
The complete policy is documented in `responses-compatible.md`. Summary completions
use this watchdog; token counting and remote compaction retain their separate
non-streaming request deadlines. Local tools, approval waits, and external ACP
agents are not subject to these completion timers. External-agent Review deadlines
and the unbounded Plan-worker policy are unchanged.

Timeout follows normal attempt finalization, including hosted-search checkpoint
persistence. HTTP drops its old response stream; WebSocket terminates its pump and
invalidates the ambiguous continuation before replay on a fresh generation.
Cancellation interrupts every phase, including backoff. Budget exhaustion yields
one actionable failure; it never restarts the entire turn. Only that profile's
fallback is sticky; other slots retain independent state.

### Retry presentation contract

Both HTTP and WebSocket recovery announce the actual delay before the next
attempt as `SessionEvent::TurnRetrying.retry_after`. The announced duration is
the same value used for backoff sleep: capped exponential backoff with bounded
75–100% jitter. The attempt number identifies the upcoming attempt within the
shared total budget. A parked reconnect starts attempt one rather than spending
a separate retry cycle. `NetworkStatus` reports attempt start, connecting,
awaiting response, quiet warning, and resumed progress, correlated by turn,
model-call number, and attempt. These events are presentation metadata, not
assistant messages or model input. Frontends still track all phases for lifecycle
and stale-event guards, but hide routine first-attempt start/connect/awaiting
updates. The TUI keeps its ordinary running/streaming display until a quiet
warning or retry occurs; each new model call starts quietly again.

The TUI uses receipt time and its monotonic presentation clock to derive a live
countdown in a non-persisted, roleless status block. It has no Assistant header
or gutter accents, including on wrapped headlines and connection-error details;
the gutter remains reserved whitespace. One layout-owned blank row separates
it from visible transcript content, including streamed messages; the existing
gap after committed history is reused, and no leading gap is added without
visible content. Real messages retain their role headers, semantic gutter
accents, and chat separators. The status and its blank boundary stay outside
message caches and selection/copy history.

A retry headline looks like
`◐ ⚠ reconnecting (attempt 2/4) · next attempt in 1s · 12s`, with warning-colored
text and plain muted connection-error details below it. A positive remainder
rounds up (500 ms displays `next attempt in 1s`); zero announced delay displays
`reconnecting now`; elapsed backoff says it is waiting for the next attempt to
start. The actual attempt-start event removes the countdown and displays the
transport phase, rather than leaving an expired reconnect countdown visible. The `◐ ◓ ◑ ◒` spinner
animates in 250 ms frames throughout positive backoff, immediate reconnect,
and expired backoff, as it does for waiting/tool execution, streaming, and
compaction. Queueing and busy, visible-pane-only redraws can introduce small
display lag. A quiet warning is separate from the preview, for example:
`No response progress for 30s · automatic retry in 2m 30s.` It does not claim the
network is disconnected or erase text already shown. Resumed progress clears it.
New calls, completion, failure, and cancellation clear stale status; child panes
remain isolated. The whole-operation elapsed time continues through recovery and
tools. The worker-only `/retry` command has no new root-turn meaning.

ACP projects a one-time diagnostic through its existing bounded retry channel:
`Provider retry N/M (next attempt in Ss): <error>` for positive delay (seconds
rounded up), or the existing `Provider retry N/M: <error>` for zero. Low-volume
network transitions use that diagnostic mechanism without resetting the answer
stream. Routine first-attempt phases produce no diagnostics, including initial
progress. Resumed progress is reported after a quiet warning or during recovery.
Actual retries retain segment-reset semantics. Stale turn/call/attempt
updates are ignored; ACP-only panes receive no fabricated native timer. Synthetic compaction reporters continue
suppressing provider progress and retries: compaction shows its elapsed line,
not internal summary-request recovery details.

### Live stale response IDs

If a live socket reports `previous_response_not_found` (or a clearly equivalent
relay error), the slot:

1. invalidates continuation with `stale_response_id`;
2. clears the partial preview;
3. spends the next attempt on the already prepared complete request, on the same
   socket only if the WebSocket recovery allowance and shared budget permit it.

A replacement socket never receives an old response ID first; reconnect always
starts with complete replay. Recovery is a fresh generation attempt, **not**
token-level resumption via `previous_response_id`. Failed partial answers never
enter replay input or become completed assistant records; partial tool calls
never dispatch. Previously completed local tools and durable records are untouched.
Dropping a connection cannot prove upstream computation stopped: retries may
repeat provider-side generation or hosted searches and incur additional charges.

## HTTP/SSE behavior

When WebSockets are disabled or a slot has fallen back, every request is an
HTTP POST to that provider's exact configured Responses URL. HTTP never sends
`previous_response_id`; it always sends the complete target-specific input.

The response must be `text/event-stream`. Headers have the request-start deadline;
meaningful response progress has the shared warning/inactivity deadlines throughout
streaming. The SSE parser future stays alive across warnings, preserving partial
frames. HTTP 5xx, request errors, stream loss before terminal output, and typed
`response_idle_timeout` failures use the shared bounded retry policy. Terminal 4xx, malformed events, or semantic failed/incomplete
Responses fail without retry. Error bodies are bounded to 4 KiB.

Streaming previews, native output capture, usage events, and replay-derived
final messages are shared between HTTP and WebSocket paths.

## Native output and replay v1

A completed response is accepted only when complete native output was captured
from `response.output_item.done` and/or the terminal response output array. The
terminal array is authoritative. The adapter constructs:

```text
ProviderReplay {
    provider: "openai.responses",
    version: 1,
    source_profile: { provider: <configured key>, model: <wire model ID> },
    items: <native output>,
}
```

The canonical Rig assistant message is derived from this replay and is used for
display, tool dispatch, persistence, resume, and future requests. There is no
generic terminal-message fallback if native capture or conversion fails.

`openai.responses` names the persisted wire protocol. It is unrelated to a
configured provider key such as `openai`, `deepseek`, or `glm`.

## Compaction routing

A root with more than one selectable catalog profile—even with a single currently
assigned profile—cannot safely install opaque native compaction from one
slot into history shared with foreign slots. Its router returns
`CompactResult::Unsupported` without initializing a slot or dispatching a request,
so core performs local-summary compaction through the active role. Automatic and
manual local summaries try the full effective input first, then descending
one-item-shorter prefixes after local admission or typed provider input-size
rejection. Correlated exchanges cannot straddle a cut, and native envelopes are
indivisible. The saved replacement is summary first plus the exact unsummarized
tail, independent of retained-user metadata limits. A full-summary request being
too large is therefore not immediately terminal.

Completion HTTP 413 and exact structured `context_too_large` /
`context_length_exceeded` codes become core `ModelInputTooLarge` errors, including
WebSocket/SSE errors and failed/incomplete response envelopes. Size classification
precedes upstream-disconnect prose classification; these errors never reconnect or
back off with the same oversized request. Authentication, quota, output exhaustion,
unstructured prose, and count-endpoint failures keep their existing handling.
Summary attempts are bounded by descending nonempty replay-safe prefixes, not a
call counter. Local admission rejects and boundary skips do not dispatch requests.
No nonempty admissible prefix means an irreducible capacity failure without a checkpoint.

Partial live-context checkpoints stay saved even if the rebuilt normal request
still exceeds the hard limit; no oversized request is sent. Edited-prefix
checkpoints install only with accepted edits. Manual compaction refreshes telemetry
without normal dispatch. Opaque tails keep their source-profile restrictions, so
local-summary fallback alone does not guarantee foreign-profile portability.

A single-profile root, Explore child, or Builder child forwards compaction to that profile's
explicit `providers.<key>.compaction.url` in `models.jsonc` when configured. No URL is inferred
from the ordinary endpoint. Correlated tool-output trimming uses that model's
`input_token_limit`, not a global default or the potentially larger physical
`context_window_tokens`. Remote output is stored as source-profile replay-only
history.

## Reset and cancellation

A compatible same-profile reasoning change through `/model-session` or `/model`
is deliberately **not a reset**. It changes only the captured Build/Plan route's
active level, applied to subsequent requests
as top-level `reasoning.effort`. All Build requests share one level; Plan stays
independent even on the same connection slot. Instructions, tools, input, and the
session/provider/model-derived prompt-cache key remain unchanged. The next
WebSocket request detects `request_properties_changed` and sends full input once,
with the identical prefix and no `previous_response_id`; continuation then resumes
normally. The socket stays open. Explicit-profile maintenance receives an explicit
level: destination counts use the requested level and source conversion uses the
captured pre-switch role's level, validated against the source catalog. An opaque
source that does not support it must first be selected with a supported level. A gateway with `send_reasoning`
disabled does not transmit effort and therefore needs no such property reset.

Both model commands save the complete role selection in the version-1 session
header for resume; `/model` also changes that mode's `provider`, `model`, and
`reasoning_level` in `config.toml`. The provider catalog in `models.jsonc` is never
rewritten. The first picker stage chooses a profile locally; the second explicitly
confirms a supported level. Separate `/reasoning` and `/reasoning-session` commands
are removed. Root selections never retarget child roles or permissions.

An accepted genuine profile change via `/model` or `/model-session` resets continuation
ownership in every initialized root slot, including when switching back to an
earlier profile. A compatible already-active `/model-session` selection is a
no-op and keeps continuations; it still requires preflight and may need conversion.
The
next request is full input with no stale `previous_response_id`; retry snapshots
keep the selected profile frozen. Ordinary Build/Plan mode changes
still use safe per-profile continuations, subject to policy and advertised-tool
replay requirements; sharing Build's profile does not bypass them. Synthetic confirmed source-summary requests
also reset ownership on success, failure, and cancellation. Adaptive local prefix
summaries invalidate ownership before each attempted completion and afterward,
including failure/cancellation, so shorter inputs cannot reuse an incompatible
chain. Their streams/usage are silent; all ordered directives are structurally
excluded. Top-level maintenance instructions contain captured application/file
guidance and a no-tool summary policy, without a catalog. Live checkpoint projection
folds effective pinned bodies from preceding history; the next normal request
renders its workflow instruction set, not synthetic maintenance guidance. Explicit model-selection conversion remains separate from adaptive
local-summary retries. Unsupported old instruction histories are rejected, not
migrated.

Both TUI commands use the same idle writable-root picker, captured Build/Plan model
role, compatibility checks, and explicitly confirmed conversion. Build targets
Build; Plan targets Plan. In Build, `/model` updates the Build assignment and
`/model-session` updates only its saved session selection. Orchestration is one
request behavior and does not change those targets. Switching mode alone is not a model change. `/model-session` saves
only that role's complete version-1 session selection ("session only — kept on /new,
fresh handoff and resume; config unchanged"); `/model` also saves its global assignment in `config.toml`. Conversion can cost tokens,
lose detail, and replace the shared context for both root modes, regardless of selection
scope. Config revision is revalidated after asynchronous preparation and before
persistence, without a config-writing transaction for session-only requests.
If a local header save fails, active and durable selections stay unchanged, config
was not modified, and no committed global revision is returned. A portable
checkpoint already saved is reported and can be reused on retry. `/model` retains
checkpoint → global save → header → route ordering: a header failure after a
global save reports the changed default and new revision, without installing the
route. Continuation resets do not make those partial outcomes atomic. Resume
restores the saved identity and reasoning level together; unavailable models or
levels reject resume without fallback, provider calls, or metadata migration.

Resume restores either command's saved Build/Plan identities, even if the root
closed immediately after acknowledgement with no further response. Independent fresh
sessions, including separate CLI/ACP launches and native workers, use global defaults.
TUI `/new` and fresh Plan implementation (command or approval-dialog action) inherit
both current Build/Plan provider, model, and reasoning selections, including choices
originally derived from configuration. `/new` clears conversation, Plan state, and
active skills without generation, not model preferences; a subsequent `/new` after
resume inherits the restored pair. Default changes do not retarget inherited modes.
Fresh implementation uses the inherited Build choice, not the Plan model. The other mutable role's
selection, Review, Explore, Builder, workers, permissions, and other active sessions remain
unchanged. There is no catalog hot reload or ACP model-selection API; ACP loading
a session still restores its saved choices. Replacement reloads catalog capabilities,
limits, and other-role defaults, validates the inherited pair without fallback, and
writes its version-1 header before an opening handoff. Missing catalog entries or
reasoning levels require restoring them or an independent launch with valid defaults,
not another inheriting `/new`. The header remains outside provider input and the
unchanged cacheable prompt-prefix construction; each replacement still has its
normal new root/cache identity and connections.

`reset` means local transcript history changed outside every provider-side
chain—for example an edit, installed checkpoint, explicit model change, or failed turn. The router
invalidates every initialized slot because all routes project the same root
transcript.

`cancel` is narrower. The router records the last dispatched slot and terminates
only that profile's active transport after the completion future is dropped.
Unrelated slots keep their continuation chains. A later request to the cancelled
slot reconnects and replays complete history.

## Logging

Startup logs one sanitized assignment line per mode with mode, provider key,
model ID, and endpoint scheme/host/port/path. Runtime request logs include only
structural metadata such as profile-independent transport mode, socket
generation, retry category, input counts, and replay source (`native`,
`portable_assistant`, `generic_assistant`, or `none`).

Each terminal response log also records provider/model, request mode, retry
number or socket generation, response ID, token usage, cache-write usage, and a
bounded `sha256:<16 hex>` fingerprint of the transmitted prompt-cache key.
HTTP captures a nonempty UTF-8 `x-request-id` before reading the SSE stream or
error body and retains it through retries and terminal error chains as
`upstream_request_id`. Missing, empty, or malformed IDs remain `None`.

A successful WebSocket upgrade's `x-request-id` is retained separately as
`websocket_connection_request_id`. A successful reconnect replaces it; a
failed reconnect preserves the old connection metadata. WebSocket response
events currently carry no transport request ID, so their terminal logs always
set `upstream_request_id=None`; the handshake ID is never presented as a
per-response ID.

Logs never include API-key contents, URL user-info/query/fragment, prompt text,
tool arguments, native replay items, encrypted reasoning, request bodies, full
prompt-cache keys, response headers, or the complete
additional-parameter map.

## Verification expectations

Loopback tests cover:

- lazy slot initialization and exact-profile reuse;
- role-specific URL, bearer token, model, reasoning, compatibility, additional
  parameters, and tool policy;
- A → B → A profile-local continuation;
- reset fan-out and active-slot-only cancellation;
- failed-profile and sticky-HTTP isolation;
- fresh child router state;
- same-profile native replay and foreign portable replay;
- foreign replay-only rejection and hard stale replay schemas;
- multi-profile local-summary restriction and single-profile remote compaction;
- target-profile context-window trimming; and
- bounded WebSocket/HTTP retry and stale-ID behavior.
