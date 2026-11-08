# Responses-Compatible Providers

This guide covers providers whose `protocol` is `openai.responses` (also the
default when omitted). OpenAI, DeepSeek, GLM, or any other model family can use
this adapter **when the endpoint implements the OpenAI Responses wire protocol**.
A provider key names an endpoint/capability catalog; its `protocol` selects the
compiled adapter, not the model's vendor name. Native Claude is supported through
[`anthropic.messages`](anthropic-messages.md), with its own strict schema;
both protocols can coexist in one catalog and session router.

Zevria does not implement Chat Completions and does not translate
`/chat/completions`, `choices[].delta`, `reasoning_content`, or Chat tool-call
chunks into Responses.

## Provider-hosted web search

Search is **disabled by default**, per provider. Enable only on an endpoint/model
supporting the stable Responses `web_search` tool:

```jsonc
{
  "providers": {
    "openai": {
      "web_search": {
        "enabled": true,
        "external_web_access": true,
        "search_context_size": "medium",
        "return_token_budget": "default",
        "filters": {
          "allowed_domains": [
            "openai.com"
          ],
          "blocked_domains": [
            "example.org"
          ]
        },
        "user_location": {
          "country": "GB",
          "city": "London",
          "region": "London",
          "timezone": "Europe/London"
        }
      }
    }
  }
}
```

Omitting filters permits all domains. Allowed and blocked lists can coexist;
subdomains are included upstream. Domain names use DNS/ASCII form (use punycode
for internationalized names), not URLs, ports, IP addresses, or wildcards.
Location uses the documented `approximate` discriminator on the wire; country is
a two-letter uppercase ISO code and timezone is an IANA name. Unknown fields,
invalid enums, blank locations, and malformed domains are configuration errors.

The [current stable-tool guide](https://developers.openai.com/api/docs/guides/tools-web-search?api-mode=responses)
documents all these controls, including both filters and their 100-entry limits.
Some SDK-generated tool types lag the guide for `blocked_domains` and
`return_token_budget`; Zevria sends the documented stable controls, not a preview
substitute. `unlimited` is for GPT-5+ reasoning web research and can increase cost
and latency. Location is not supported by deep-research models. Opt-in does not
guarantee that a gateway or every model in its catalog accepts each control.
Rejections retain upstream details and identify the profile/configuration; Zevria
never retries a request with search or its controls silently removed.

Search is allowed in Build (including request-local orchestration), Plan, Review, Explore,
Builder, native ACP workers, and synthesis's configured task role. Empty task
allow-lists disable it. Compaction, checkpoint conversion, and internal summaries
retain the active policy's declarations and hosted-search gate to preserve the
normal request prefix. Their trailing instruction forbids tools; returned calls
are never executed and cause one retry with `tool_choice: "none"`. Ordinary task
selection defaults to automatic; explicit `tool_choice` must reference declared
function tools or an enabled, policy-permitted hosted capability.

Search runs at the configured provider, never in a local executor or shell.
HTTP/SSE, WebSocket, retries, stale-continuation recovery, HTTP fallback, and exact
input-token counting share the same prepared definition. Count-endpoint fallback
does not change the real completion's capabilities. Command confinement and
workflow/inspection/confirmation gates are unchanged.

For every non-silent Responses call, **received answer text streams progressively**,
whether search is disabled, unused or used, including one long content part. Refusals,
reasoning and search/open-page/find-in-page progress also remain live. Previews
are provisional: completed native parts/items and the validated final output
reconcile the same source-addressed answer in place. Delivery uses coalesced
latest-state snapshots, not a frame or transcript record for every token. A relay
that sends only final output still works, but Zevria cannot display deltas the
relay has not delivered.

Source-title links appear next to claims as usable annotations arrive. Offsets
address the original unsanitized text; ranges beyond a growing part wait for more
text rather than being treated as permanently invalid. Provisional citation
markers (including unfinished tokens) stay hidden while metadata arrives, without
withholding surrounding prose. Final invalid offsets append usable
links after a block without deleting narrative; only recognized citation markers
are replaced. Only safe HTTP(S) destinations are linked. Terminal controls are
removed from display copies, never from raw native text or annotations. TUI URLs
are fully visible and copyable; the terminal emulator determines URL recognition
and activation gestures (there is no in-app opener or universal click shortcut).

Completed native output and unknown annotations are preserved for exact-profile
replay. Switching profiles carries readable titles and URLs, but not hosted items,
provider IDs, or opaque state. Portable links contribute to destination token
estimates. Every transmission has a provider-neutral live response attempt,
independent of search advertisement. Successful responses persist only native
replay; live commit and restore derive their display from that same ledger.
Hosted actions absent from final output are dropped at commit.

An abandoned attempt with readable output or hosted activity persists once as
an immutable `zevria_incomplete_response` v1 record. Its failed/interrupted text
remains headerless display evidence, not model input, compaction source or
capacity accounting. Tool bindings are dropped. The provider waits for durable
failed-attempt acknowledgement before retry/clear. Core releases the provider
future, drains the collector, closes uncommitted attempts, and appends recovery
then the response before awaiting frontend publication or running local tools.
A response discarded because cancellation wins becomes interrupted recovery,
not a committed assistant message. Ordinary cancellation preserves observed text;
a process crash still loses an in-flight, uncheckpointed attempt. There are no
in-progress checkpoints. Failed writes retain memory and report degradation.

## First-run setup

Zevria creates whichever setup files are missing, then exits before opening a
session: `~/.zevria/config.toml` for ordinary settings and all five complete mode
assignments, and its sibling `~/.zevria/models.jsonc` for provider/model capabilities.
`ZEVRIA_CONFIG=/x/work.toml` selects `/x/work.toml` **and** `/x/models.jsonc`;
there is no separate models environment variable. Removing only `models.jsonc`
recreates only that file and stops, without overwriting the ordinary settings.

Both skeletons are commented, non-secret, and created with mode `0600` on Unix.
The JSONC skeleton's body is intentionally `{}` plus commented guidance. Configure
at least one provider and its models in `models.jsonc`, and all five `[modes]`
assignments with `provider`, `model`, and `reasoning_level` in `config.toml`, then
start Zevria again. There is no built-in endpoint, credential, model, or assignment.
`config.toml` keeps session, skills, theme, ACP, ensemble, command, and log settings.
Provider definitions in TOML are rejected. JSONC `modes` and model-level
`reasoning_level` are obsolete and rejected; no automatic migration is performed.
Offline skill operations may read incomplete `[modes]` without loading a catalog.

## Provider/model/mode contract

Put this document in `models.jsonc`. Comments and trailing commas are accepted;
other JSON5/loose syntax is not. `base_url` is the exact Responses endpoint, not a
host root or `/chat/completions`. API keys are literal strings or explicit local
command objects; strings have no interpolation or environment lookup. Optional `compaction.url` and `input_token_count.url` can name explicit
auxiliary endpoints; `additional_params` can hold gateway-specific fields.

```jsonc
{
  "providers": {
    "openai": {
      "base_url": "https://api.openai.com/v1/responses",
      "api_key": "replace-with-api-key",
      "supports_websockets": true,
      "network": {
        "connect_timeout_seconds": 30,
        "request_start_timeout_seconds": 30,
        "stall_warning_seconds": 30,
        "response_idle_timeout_seconds": 180,
        "recovery_window_seconds": 300,
        "max_attempts": 30
      },
      "compatibility": {
        "send_reasoning": true,
        "send_reasoning_encrypted_content": true,
        "strict_tools": true,
        "send_prompt_cache_key": true,
        "send_store": true
      },
      "compaction": {
        "request_timeout_seconds": 300
      },
      "input_token_count": {
        "enabled": true,
        "derive_url": true,
        "request_timeout_seconds": 30
      },
      "models": {
        "gpt-5.6-sol": {
          "context_window_tokens": 272000,
          "input_token_limit": 272000,
          "retained_user_tokens": 20000,
          "reasoning_levels": [
            "low",
            "medium",
            "high",
            "xhigh"
          ],
          "reasoning_summary_level": "detailed"
        }
      }
    }
  }
}
```

Store the matching complete assignments in `config.toml`:

```toml
[modes]
plan = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
build = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "medium" }
review = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
explore = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "low" }
builder = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
```

Provider keys and model IDs are exact, case-sensitive, nonblank strings. They
are durable replay identities: rename them only when intentionally changing
identity. A model object key is the upstream wire model ID itself; there is no
local alias or `id` field. JSON object keys, including dotted IDs, are quoted.

The same wire model ID may exist under different providers. A provider's
`models` object is a user-owned map and cannot contain two entries for one key.
`providers` is also user-owned; Zevria does not field-merge it with a built-in
catalog. Valid unreferenced providers and models are legal and remain lazy—they
are never connected merely because they are declared.

Required provider fields are `base_url`, `api_key` (literal string or command object),
`supports_websockets`, and a nonempty `models` object. Every model requires:

- `context_window_tokens` greater than zero;
- an optional `input_token_limit` between one and `context_window_tokens`;
  omission makes it equal the physical context window;
- `retained_user_tokens` no greater than that model's automatic compaction
  trigger, which is derived from `input_token_limit`;
- a nonempty, duplicate-free `reasoning_levels` array drawn from `none`,
  `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`;
- `reasoning_summary_level`: `auto`, `concise`, or `detailed`.

All five model-role assignments in `config.toml` are required, including
`modes.builder`. Each requires an explicit `reasoning_level` supported by the
assigned provider/model. Shared models may have different levels across roles.
Builder never falls back to Build or Explore, even for an otherwise Plan-only
configuration. **Orchestration is explicit request behavior in root Build**, not
a mode or sixth model role. It uses the `build` assignment and the same session-local
Build selection. There is no `modes.orchestrate` assignment. Ordinary Plan and
Ensemble Plan use `plan`; Ensemble Review uses `review` even though its frontend
workflow mode is Build. Explore children use `explore`; mutating Build children
use the distinct child-only `builder` role. Both kinds run nominal Build child
turns without inheriting the root's model role or tool capabilities.

Standard Build retains full parent implementation tools and optional Explore
delegation, but forbids native Builders. `/orchestrate <prompt>` temporarily
permits Explore and Build children for that request and requires at least two
distinct accepted children in one launch batch; the parent can still implement
directly. Its activation, bounded correction, and next Standard boundary are
append-only request directives, keeping fixed instructions/catalog/tool schemas
unchanged across the sequence. Plan's
existing restricted policy permits only configurable Explore delegation, never
builders. Native ensemble worker Plan/Review policies are unchanged. Selecting a
mode is a session-local frontend control, not a model tool or a new model selection.

### Multiple providers

```jsonc
{
  "providers": {
    "gateway": {
      "base_url": "https://gateway.example/v1/responses",
      "api_key": "replace-with-gateway-key",
      "supports_websockets": false,
      "compatibility": {
        "send_reasoning": false,
        "send_reasoning_encrypted_content": false,
        "strict_tools": false,
        "send_prompt_cache_key": false,
        "send_store": false
      },
      "additional_params": {
        "gateway_routing": "deepseek-pool",
        "thinking": {
          "type": "enabled"
        }
      },
      "models": {
        "deepseek-reasoner": {
          "context_window_tokens": 128000,
          "retained_user_tokens": 10000,
          "reasoning_levels": [
            "low",
            "medium",
            "high",
            "xhigh"
          ],
          "reasoning_summary_level": "detailed"
        }
      }
    },
    "backup": {
      "base_url": "https://backup.example/responses",
      "api_key": "replace-with-backup-key",
      "supports_websockets": true,
      "models": {
        "deepseek-reasoner": {
          "context_window_tokens": 64000,
          "retained_user_tokens": 5000,
          "reasoning_levels": [
            "low",
            "medium",
            "high",
            "xhigh"
          ],
          "reasoning_summary_level": "concise"
        }
      }
    }
  }
}
```

Store the matching complete assignments in `config.toml`:

```toml
[modes]
plan = { provider = "backup", model = "deepseek-reasoner", reasoning_level = "high" }
build = { provider = "gateway", model = "deepseek-reasoner", reasoning_level = "medium" }
review = { provider = "gateway", model = "deepseek-reasoner", reasoning_level = "high" }
explore = { provider = "backup", model = "deepseek-reasoner", reasoning_level = "low" }
builder = { provider = "gateway", model = "deepseek-reasoner", reasoning_level = "high" }
```

Each unique `(provider key, model ID)` gets independent lazy transport and
continuation state. Exact matches are reused by modes in the same root router.
A failed Plan endpoint, WebSocket fallback, or stale continuation cannot poison
a different Build profile.

## Session routing headers

Some gateways require a session ID in an HTTP header. Set the optional
`session_id_header` **directly in that provider's object** to the name required
by its gateway. Omitting the setting sends no session header; there is no
built-in name, provider-name detection, or automatic opt-in after an error.

These are fragments to merge into existing provider objects, not complete
provider configurations:

```jsonc
{
  "providers": {
    "deepseek": {
      "session_id_header": "x-opencode-session"
    },
    "gateway": {
      "session_id_header": "x-conversation-id"
    }
  }
}
```

The configured string is the **header name**, not a static ID or a template.
Zevria supplies the stable allocated conversation ID as its value. A root or
resumed session uses its intended transcript filename stem, even before a new
session's transcript is materialized. The value stays stable across
turns, tool follow-ups, model changes, resets, retries, and reconnects. Fresh
roots, Explore and Build children, and native Zevria ensemble-worker sessions use their
own IDs. External ACP agents manage their own provider transports.

The header is sent on completion HTTP requests, WebSocket handshakes and
reconnects, exact input-token counts, and configured remote-compaction requests.
It is independent of the profile-specific `prompt_cache_key`: disabling prompt
caching, or omitting that JSON field for counting, does not disable session
routing. All profiles of one provider use its configured name; other providers
can choose different names or omit the setting entirely.

Names are case-insensitive HTTP header names. Blank, whitespace-padded,
malformed, or non-string settings are rejected. Authentication and transport
headers cannot be repurposed: `authorization`, `proxy-authorization`, `host`,
`accept`, `content-type`, `content-length`, `content-encoding`, `connection`,
`upgrade`, `transfer-encoding`, `te`, `trailer`, `expect`, and all
`sec-websocket-*` names are reserved. Errors identify
`providers.<key>.session_id_header`. A blank or header-invalid runtime session
ID fails locally before transport setup when routing is configured.

Routing values are marked sensitive for transport debug formatting and are not
added to transport diagnostics. Nevertheless, the configured provider's
completion and auxiliary endpoints receive a stable correlation ID, and those
endpoints may have different origins. Configure only trusted endpoints; this
setting does not change the existing redirect policy.

This is not a general custom-header map. Do not put the setting under
`.compatibility`, `.additional_params`, or a model object: `additional_params`
controls JSON fields, not HTTP headers. There is no `send_opencode_session`
boolean. Restart Zevria or recreate the session runtime after changing the
provider configuration.

## Strict validation and credentials

Configuration uses strict unknown-field rejection. Loading fails for:

- an empty provider catalog or blank provider/model keys;
- missing required provider, model, or mode fields, or duplicate JSONC property
  names (including escaped-equivalent names);
- malformed endpoints or schemes other than HTTP(S)/WS(S);
- blank or invalid HTTP-header literal credentials;
- malformed credential commands, zero helper timeouts, or unrepresentable durations;
- invalid or transport-owned session-routing header names;
- zero context windows, input ceilings, or request timeouts;
- input ceilings larger than their physical context windows;
- invalid reasoning values, empty/duplicate supported levels, a default outside
  the supported set, or retained-user budgets above the model trigger;
- provider/mode tables left in `config.toml`, or loose JSONC syntax (unquoted keys,
  missing commas, single quotes, hexadecimal or unary-plus numbers);
- dangling mode references, with the offending mode and available candidates;
- reserved structural additional parameters; and
- legacy `[openai]`, `[openai.models]`, `[openai.reasoning]`, or removed
  session-level `context_window_tokens` / `retained_user_tokens` fields.

There is no migration or fallback assignment. `ZEVRIA_API_KEY` is ignored;
Zevria performs no interpolation and no per-provider environment lookup. API
keys are redacted from Rust debug output, diagnostics, and logs. Startup logs
include one line per mode with only mode, provider key, model ID, and a
sanitized endpoint URL; URL user-info, query, and fragment are omitted.

The `models.jsonc` file can contain literal secrets or commands executed with your
privileges. Protect it with filesystem permissions and secret-management practices
appropriate to the host.

### Command-provided API keys

Every provider in either protocol can obtain its key from an explicitly configured,
non-interactive local helper instead of storing a literal secret:

```jsonc
"api_key": {
  "command": "auth",
  "args": ["--key"],
  "timeout_seconds": 30,
  "refresh_interval_seconds": 300
}
```

- `command` is required: a nonblank program name on PATH, an absolute path, or a
  relative path containing a separator, resolved against the directory containing
  `models.jsonc`. `args` defaults to `[]`; its entries are passed directly, without
  shell interpretation. Pipelines require an explicit shell, for example
  `"command": "sh", "args": ["-c", "your pipeline"]`.
- `timeout_seconds` defaults to 30 and must be positive. `refresh_interval_seconds`
  defaults to 300; zero runs the helper for every authenticated HTTP request or
  WebSocket handshake. Unknown object fields and other JSON types are rejected.
  Strings are **always literal keys**, never commands or interpolated values.
- The helper inherits Zevria's environment and runs with the absolute
  `models.jsonc` directory as its working directory, independent of the workspace.
  This also governs tools such as `npx`, `uv run`, and `python -m`. No per-helper
  environment map is supported. Stdin is closed: helpers must not prompt or need
  a TTY. Stdout and stderr are captured and capped at 64 KiB each. Timeout and turn
  cancellation kill the process tree, including children that have not detached.
- Stdout must be UTF-8. A leading BOM and surrounding whitespace are removed;
  the remaining value must be nonempty, single-line, and a valid HTTP header
  value. JSON with expiry metadata is not supported. Only the key goes into the
  `Authorization` or `x-api-key` header; it never enters the request body.
- Execution is lazy: parsing/validation, revision checks, router construction,
  and `/model` preparation never run the helper. An HTTP request, token count,
  compaction call, or WebSocket handshake resolves it when authentication is
  needed. Its runtime is bounded by the helper timeout, not charged to connect,
  request-start, token-count, or compaction deadlines; the existing Connecting
  status remains in effect during completion authentication.
- One in-memory cache belongs to each provider entry per loaded configuration.
  All of its models, root/model-switch routers, and Explore/Builder children share
  it. Concurrent callers serialize helper execution and reuse a cached key within
  the interval. Each new/resumed session reloads the file with an empty cache;
  separate processes, including ensemble workers, have independent caches.
- HTTP **401 only** discards the rejected command key, refreshes once, and repeats
  the same exchange with the same prepared body and `prompt_cache_key`. Concurrent
  stale rejections cannot invalidate a replacement key. A second 401 is terminal;
  403 does not refresh, so choose an interval shorter than the key's lifetime for
  gateways that return 403 on expiry. This refresh is inside the HTTP exchange or
  handshake: it consumes no recovery attempt or WebSocket fallback budget and
  emits no `TurnRetrying` event. Literal-key 401s remain terminal.
- Healthy WebSockets authenticate only at handshakes. Expiry does not close an
  open socket, rerun the helper for incremental messages, or alter continuation
  state. In-band WebSocket errors do not trigger this credential refresh policy.
  Key rotation needs **no `models.jsonc` edit and no restart**.
- A missing program, nonzero exit, timeout, excessive output, or invalid stdout
  stops the request immediately: no network send, recovery retry, or HTTP fallback
  for that failed authentication. Errors identify `providers.<key>.api_key`, the
  program, exit status when available, and a bounded stderr excerpt with URLs
  removed. They do not include args or stdout; helpers must keep stderr free of
  secrets too. Debug output redacts literals, arguments, leases, and cached keys.
  Generated keys stay in memory and are never persisted in transcripts or logs;
  logs record only provider/program/timing and rejection request IDs.

**Security:** configuring a helper authorizes arbitrary local execution with
Zevria's privileges, not a sandbox. Only configure helpers you trust in your
user-owned `models.jsonc`. On Windows, executables resolve on PATH; `.cmd` and
`.ps1` helpers require explicit `cmd.exe /C` or `powershell -File` invocation.

## Protocol requirement

For an HTTP profile Zevria sends:

```text
POST <providers.<key>.base_url>
Authorization: Bearer <resolved providers.<key>.api_key>
Accept: text/event-stream
```

A compatible endpoint must provide the Responses semantics Zevria relies on:

- streaming HTTP/SSE Responses events and a terminal `response.completed`,
  `response.failed`, `response.incomplete`, or compatible `response.done`;
- Responses output items for assistant messages, reasoning, and function
  calls;
- stable response and call-correlation IDs;
- correlated `function_call_output` handling on later complete-history HTTP
  requests or WebSocket continuations;
- terminal output arrays that can be persisted for replay; and
- standard Responses usage fields when usage reporting is available.

A gateway returning Chat Completions is incompatible even if it serves the
same underlying model. A direct vendor URL is usable only if that exact route
implements Responses; otherwise place a Responses-compatible gateway in front
of the vendor service.

## Compatibility controls

Provider-level compatibility flags default to `true`:

| Setting | Disabled behavior |
| --- | --- |
| `send_reasoning` | Omit the top-level Responses `reasoning` object. Model reasoning settings remain configured. |
| `send_reasoning_encrypted_content` | Omit `include: ["reasoning.encrypted_content"]`. This can make stateless native reasoning replay incomplete. |
| `strict_tools` | Advertise declared function tools without a `strict` field, including inline additions. |
| `additional_tools` | Fold additions into top-level tools (replace same-name entries in place, append new names); correct calls, but the tool prefix changes. |
| `send_prompt_cache_key` | Omit the profile-specific prompt-cache key. |
| `send_store` | Omit `store`; when enabled Zevria sends `store: false`, never `true`. |
| `developer_messages` | Project ordered skill directives with `role: "user"` instead of `role: "developer"`. |

`instructions` is a deterministic join of engine-owned modules in this order:
engine protocol, optional Application guidance, optional File guidance (sorted by
component key), Workflow policy (one-line JSON declaration then role text), Command
conventions when permitted, Hosted search when permitted, Inspection and scratch
policy when selected, and Eligible skills (selection module plus JSON, or an
unavailable sentence). Stable protocol/application/file sections precede workflow
changes to preserve the provider's cacheable prefix. `session.preamble` replaces
only Application guidance, never the protocol or capability modules.
Skill bodies/revocations, request boundaries/corrections and additional function
declarations project at their recorded positions through preflight, counting,
HTTP and WebSocket. Compaction reuses this same prefix and appends a summary
instruction instead of replacing instructions or stripping declarations/directives.
Raw system messages are rejected rather than relocated into `instructions`.

For a gateway that explicitly requires user-role instruction input:

```jsonc
{
  "providers": {
    "gateway": {
      "compatibility": {
        "developer_messages": false
      }
    }
  }
}
```

This affects only ordered skill directives, not top-level instructions. It is an
authority tradeoff, not semantic equivalence to developer messages.
Code-enforced tool permissions and skill eligibility still apply. Zevria never
silently retries with a different role after an endpoint rejects developer input.
Changing compatibility is a genuine request-property/input-shape boundary.

`supports_websockets` is separate:

- `true` enables best-effort Responses WebSocket use, reconnect, stale-ID full
  replay, and profile-local HTTP fallback (provisional after a network outage);
- `false` skips WebSocket entirely and sends complete history over HTTP/SSE.

Zevria does not support alternate vendor socket dialects.

### Completion networking and automatic recovery

`providers.<name>.network` is optional; omission (including individual omitted
fields) enables the defaults shown above. Durations are positive integer seconds
that must fit monotonic deadlines. The warning must be shorter than the idle
limit, and `max_attempts` must be positive. Zero does not mean unlimited.

A completion warns after 30 seconds of no meaningful response progress and
retries its immutable prepared request after 180 seconds of inactivity. The
watchdog covers the wait for the first meaningful event and all subsequent
streaming. Reasoning, function arguments, and hosted-search advancement count;
keepalives, unknown metadata, repeated statuses, and late prior-response events
do not. A steadily progressing response can run longer than three minutes.
Increase the idle threshold for unusually quiet reasoning models.

Connection establishment and request start initially have independent 30-second
limits. HTTP request start ends at response headers; each local header timeout
doubles the next header deadline, capped at the idle limit: **30 → 60 → 120 →
180 → 180 seconds** by default. Longer header waits show the same quiet warning.
WebSocket request start ends when the socket write is acknowledged. There is no
HTTP total-body timeout. Exact token counts and remote compaction retain their
separate deadlines; synthetic summaries inherit completion recovery and the
watchdog (without normal UI progress).

The shared controller schedules recovery within a **300-second outage window**
(`recovery_window_seconds`). The clock starts at the first attempt or the last
meaningful response progress, whichever is later. A retry is scheduled only if
its delay fits in the remaining window. The window **never interrupts an attempt
already running**: per-attempt silence timers still apply, and progressing work
has no total duration limit. `max_attempts: 30` is an absolute per-request cap,
including handshakes, stale-ID resends and fallback, not a separate budget per
transport. Recovery also stops after the **fourth interrupted response** (an
attempt that failed after progress), bounding repeated partial-generation cost.

Backoff starts at 0.5 seconds, doubles for consecutive failures without progress,
and caps at 15 seconds, with 75–100% jitter. Progress resets the exponent. The
announced duration exactly matches the sleep. `retry-after-ms` or numeric
`Retry-After` (integer or decimal delta-seconds) replaces backoff; a hint longer
than the remaining window stops immediately and names the requested delay.
Malformed hints and HTTP-date values are ignored. HTTP 409 is not retried by
default. `x-should-retry: true/false` overrides default HTTP status policy, except
that input-size rejection and Responses `insufficient_quota` stay terminal.

| Failure | Policy / actionable hint |
| --- | --- |
| Local header timeout | Retry with an escalating header deadline; inspect `request_start_timeout_seconds` |
| Response inactivity | Retry; inspect `response_idle_timeout_seconds` for quiet models |
| Connect, request, stream loss or upstream disconnect prose | Retry; check the network or proxy |
| HTTP 408, 429 (except `insufficient_quota`), 5xx | Retry; provider unavailable/rate limited; honor server hints |
| In-stream `server_error` / `rate_limit_exceeded`, including `response.failed` | Retry on HTTP and WebSocket |
| Structured input-size rejection, ordinary other 4xx, malformed events, other semantic failures | Terminal; input size remains typed `ModelInputTooLarge` |

WebSocket gets two attempts before HTTP fallback (one after a re-probe). HTTP
426 selects sticky HTTP immediately. Otherwise, fallback is sticky only if its
**first HTTP attempt succeeds**. If that attempt also fails retryably, HTTP is
provisional for the rest of this request; the next request re-probes WebSocket
with a fresh generation and full input. A later HTTP success in that same request
does not commit it. HTTP-only configuration never re-probes.

Cancellation interrupts every wait. Exhaustion returns one self-contained
Display error: attempts and elapsed time, stop reason (window, attempt cap,
interrupted-response cap, or excessive server delay), `[category]: category: full
cause chain`, and the category-specific hint. It names
`providers.<name>.network.recovery_window_seconds`; reqwest URLs are removed and
socket/DNS/TLS causes retained. Retry events carry `category: full cause chain`.
Shared WARN logs include the category, attempt, elapsed time, upstream request ID,
progress flag, next delay, header deadline, remaining window and bounded error
detail. Recovery never restarts the entire turn.

Healthy first attempts keep the ordinary running/streaming display, without
connection-phase details or ACP network diagnostics. Those details appear only
for quiet warnings and recovery. Quiet warnings preserve the preview and state
uncertainty rather than claiming a disconnection. Actual retries start a new live attempt rather than resuming tokens; readable
abandoned output remains as an incomplete-response row.
Incomplete answers and partial function calls are never committed or executed;
completed local tool work is not rerun. Upstream generation or hosted searches
may nevertheless repeat and incur charges because disconnecting cannot prove
that remote work stopped. Provider timers do not apply to local tools, approval
waits, parked healthy sockets, or external ACP agents; external-worker Plan and
Review deadline policies are unchanged.

Network policy follows the resolved provider profile through root sessions,
model switches, and internal Build/Explore children. Timing, retry counts, and
network statuses are never included in Responses payloads, instructions, tool
definitions/order, prepared input, routing headers, or `prompt_cache_key`. The
provider's cacheable prompt prefix remains unchanged.

When enabled, `prompt_cache_key` is a 64-character lowercase SHA-256 digest.
Its length-delimited inputs are a format-version marker, the session or child
session ID, provider key, and model ID. This keeps those identities isolated
without exceeding the Responses API's 64-byte limit. Upgrading from the older
raw composite format creates one expected cold cache identity; transcripts and
continuation records require no migration. Disabling `send_prompt_cache_key`
skips both key validation and transmission.

## Additional Responses parameters

`providers.<key>.additional_params` is a deterministic top-level map reused
for the initial request, reconnect/full replay, stale-ID recovery, HTTP
fallback, and HTTP retry. Values may be strings, numbers, booleans, arrays, or
nested JSON values.

These structural keys are reserved:

```text
model
input
instructions
tools
stream
include
previous_response_id
store
background
reasoning
prompt_cache_key
```

They are owned by Zevria and cannot be overridden. Zevria does not log the
complete map; still treat gateway routing values as potentially sensitive.

Migration: remove any `providers.<key>.additional_params.include` setting. It
is now rejected so Zevria can keep replay behavior deterministic. Use
`compatibility.send_reasoning_encrypted_content = false` only when a compatible
gateway cannot accept the typed encrypted-reasoning request. Normal HTTP/SSE
and WebSocket response-producing requests include it by default;
`/responses/input_tokens` and `/responses/compact` requests always omit it.

### Explicit prompt-cache comparison (operator opt-in)

To request an upstream comparison, merge this into the selected provider's
existing `additional_params` object. Replace the example with an **explicitly
chosen recent completed response ID**; preserve any existing option siblings.

```json
{
  "additional_params": {
    "prompt_cache_options": {
      "comparison_response_id": "resp_operator_selected_baseline"
    }
  }
}
```

OpenAI documents this field as requesting a diagnostic comparison, **not** loading
conversation history or changing caching behavior. It is unrelated to
`previous_response_id`, which Zevria owns for socket-scoped continuation. Zevria
never chooses/updates a comparison ID, modifies operator configuration, or adds
baseline/retry requests for diagnostics. A configured ID stays fixed until the
operator changes it and can become stale. See the official
[comparison API guide](https://developers.openai.com/api/docs/guides/prompt-caching/diagnostics).
Support is model/endpoint dependent; Responses-compatible syntax does not prove
a gateway implements or forwards the comparison. Missing/unsupported results
are not evidence of a hit or miss. Do not change model selection to enable this
feature without a separate decision.

Normal HTTP and WebSocket bodies retain the exact configured comparison. Only
this nested leaf is excluded when comparing continuation/cache-sensitive
properties; every other option, including mode, retention, tools, instructions,
and reasoning, remains significant. An empty options object is normalized away
only when removing the comparison leaf created it. No configured comparison
means unchanged request bytes and behavior, with or without instrumentation.
Counting and remote compaction remove the completion-only leaf, retain unrelated
options, and do not become completion baselines.

<!-- BEGIN TEMPORARY CACHE OBSERVER -->
Build with `--features cache-diagnostics` to observe returned
`prompt_cache_diagnostics` before SDK projection on both transports. The
allowlisted outcome/reason and presence-aware provider token estimates remain
separate from raw usage and local prefix checks. A comparison hit does not imply
that all input tokens were reused. No missing, malformed, or future unknown
outcome is converted to success. See `docs/cache-diagnostics.md` for categories,
privacy, persistence versioning, and the separately authorized bounded synthetic
reproduction. This paragraph and that observer are temporary; request
interoperability above is available in both feature configurations.
<!-- END TEMPORARY CACHE OBSERVER -->

## Exact input-token counting

Exact counting is enabled by default. With `derive_url = true` and no explicit
`url`, Zevria appends `/input_tokens` to the configured Responses endpoint; for
example, `/v1/responses` becomes `/v1/responses/input_tokens`. Set an explicit
HTTP(S) URL for a gateway-specific route, set `derive_url = false` to require an
explicit route, or set `enabled = false` to disable the capability.

For image-bearing input, or when a capacity decision is near or ambiguous,
Zevria prefers posting the complete
logical request to this endpoint: model, the exact rendered instruction set, selected
tools and compatibility rewrites, additional prompt parameters, native or
portable replay, and the full ordered input including directive history.
Endpoint-excluded non-prompt fields, such as cache metadata, are omitted. Counting never uses an incremental
`previous_response_id` body. It does not perform a completion, publish
completed-response usage, or mutate continuation ownership.

A nonnegative `input_tokens` response becomes the authoritative measurement.
HTTP 404, 405, and 501 mark exact counting unsupported for that initialized
profile runtime and are cached. Authentication errors, rate limits, malformed
responses, timeouts, and transport failures are transient diagnostics: the
current assessment falls back locally, further attempts are suppressed for the
rest of that turn, and a later turn may retry. The configured timeout bounds
each attempt, and endpoint diagnostics omit URL credentials, query strings,
and fragments.

## Image input

Validated raster prompts stay on the existing Responses-compatible boundary.
Ordered text and image blocks become `input_text` and `input_image` items; embedded
images use MIME-bearing `data:image/png;base64,...` URLs (or their original
validated JPEG/WebP/GIF MIME). HTTP/SSE, full-history WebSocket requests,
continuation comparisons, reconnect fallback and token-count requests use the
same canonical content. Provider/model vision restrictions are surfaced as
errors; Zevria does not fetch images or silently resize/drop them.

Local canonical and wire accounting charge approximately **1,600 tokens per
image**, plus ordinary text/protocol overhead, rather than base64 string length.
Only temporary accounting projections omit encoded bytes; persisted messages,
outgoing requests, digests and continuation identity remain unchanged. The
historical `conservative_tokens` field is not an upper bound for image cost.
Exact endpoint counts are preferred when available. Correlated tool-output
trimming uses the same image accounting instead of deleting useful tool text to
make room for storage bytes. Source images remain real blocks in compaction and
summarization requests; textual summaries do not reproduce the image bytes.
See [image input](image-input.md) for formats, limits, clipboard and history privacy.

## Source-aware replay v1

Successful Responses output is persisted as raw native items plus its configured
source profile:

```json
{
  "provider": "openai.responses",
  "version": 1,
  "source_profile": {
    "provider": "openai",
    "model": "gpt-5.6-sol"
  },
  "items": []
}
```

`openai.responses` identifies the persisted **wire protocol**. It is unrelated
to a configured provider key such as `openai`, `deepseek`, or `glm`.

When the target exactly matches `source_profile`, Zevria replays native items
losslessly, including ordering and unknown fields. When a shared root transcript
moves to a foreign profile, Zevria builds a deterministic portable projection:

- retain visible output text and refusals;
- rebuild function calls and results with deterministic provider-neutral
  correlation IDs while preserving names, arguments, and result content;
- omit reasoning, encrypted/signature/opaque content, provider-specific text
  metadata, unknown native output types, message/output-item IDs, and provider
  call IDs. Untagged assistants receive the same portable treatment;
  today's assignment never supplies missing provenance. Ambiguous tool handles
  and malformed known replay fail locally before connection or model dispatch.

A replay-only opaque checkpoint has no safe foreign representation and causes a
clear error. Unsupported replay versions or current-v1 records without `source_profile` are
not migrated: resume fails explicitly instead of treating the line as generic
malformed recovery and silently changing history.

Provider and model keys therefore have persistence meaning. Reusing one key for
a materially different endpoint/model can invalidate same-profile replay
assumptions.

Capacity estimates do not charge the native replay's serialized wire envelope.
Replay-backed rows are measured from their canonical message for both same- and
cross-profile use, so encrypted reasoning, IDs, and JSON escaping cannot inflate
one route relative to another. For replay-only checkpoints, known Responses
items are decoded semantically and unknown future items use an opaque-stripped
conservative fallback.

## Incremental function tools

`config.toml` enables append-only declaration history by default:

```toml
[session]
incremental_tools = true
```

The first dispatch after session start or a checkpoint records an `Initial` tool
snapshot in registry order. Top-level function `tools` remains that snapshot for
the window. Newly permitted tools or changed definitions append persisted
`Additional` records, rendered at their input position as
`{"type":"additional_tools","role":"developer","tools":[...]}`. The role is
always `developer`, independently of `developer_messages`. Host-only annotations
are never transmitted. Tools that leave the permission list stay declared but
local calls are denied by the engine. Hosted `web_search` remains top-level and
policy-gated; it is not persisted.

A window with an `Initial` stays incremental even after disabling the flag; a
window without one follows the current policy per request and writes no records.
Existing transcripts load without migration. After compaction, the next dispatch
records a fresh current-policy `Initial`. Mode-specific instructions still change
on Plan/Build switches; incremental tools do not stabilize that separate prefix.

Gateways that reject `additional_tools` need
`providers.<key>.compatibility.additional_tools: false`. This folds additions into
top-level tools, preserving existing name positions and appending new names. The
workaround changes the cached tools when additions occur; it never silently drops
a declaration. Unsupported gateways may first fail when the tool set changes.

## Context and compaction

`[session.compaction]` contains only session-wide settings:

```toml
[session.compaction]
auto_trigger_percent = 90
# summary_prompt = "Custom local summary instructions"
```

The active model profile supplies a physical `context_window_tokens`, a hard
`input_token_limit`, and a retained-user budget. The input limit defaults to
the physical window, preserving existing configurations. To retain a 272K
paid-input ceiling for a model or gateway advertising a larger physical window,
configure the values separately, for example:

```jsonc
{
  "providers": {
    "gateway": {
      "models": {
        "large-context-model": {
          "context_window_tokens": 400000,
          "input_token_limit": 272000,
          "retained_user_tokens": 20000,
          "reasoning_levels": [
            "low",
            "medium",
            "high",
            "xhigh"
          ],
          "reasoning_summary_level": "detailed"
        }
      }
    }
  }
}
```

Before accepting or dispatching a request, Zevria selects one projected-input
snapshot. Exact provider counts win; otherwise same-profile completed usage
plus appended semantic deltas wins; without a valid usage baseline, an
opaque-safe conservative estimate is used. Raw serialized replay size is never
an unconditional maximum. The same snapshot controls the automatic trigger
and hard gate.

At or above the trigger, or above the hard limit, Zevria can compact once for
that prepared dispatch, rebuild the complete request, and remeasure it. It
blocks normal dispatch when the rebuilt request still exceeds `input_token_limit`;
a request above the trigger but within the limit proceeds after that attempt.
Compaction itself may also fail (for example, irreducible input, cancellation,
or provider failure).

Local-summary fallback applies to automatic pre-turn, mid-turn, edited-prefix,
and manual compaction. It snapshots the exact normal request: rendered active-policy
instructions, window tools (or current desired tools before an initial record),
full input including directives and additions, role and permissions. The full-history
attempt appends exactly one user message: the engine-owned `compact-request.md`
preface followed by the configured summary prompt. Reasoning and cache identity
are unchanged.
If local size admission rejects it, or the completion returns a typed input-size
error, it tries prefixes one item shorter at a time. It stops at the first success,
without binary search, chunk/reduce summarization, or a fixed retry cap. Cuts that
split correlated tool exchanges are skipped; message items, batched results, and
native replay envelopes remain indivisible. Malformed source/replay input remains
an error, not permission to discard newer records.

The saved replacement is the generated summary **followed by the complete verbatim
unsummarized tail**, not bounded retained-user copies followed by a summary. The
tail keeps its values, variants, IDs, opaque data, and order regardless of
`retained_user_tokens`, even when it is zero. Retained-user metadata is still saved
for other consumers. Task state is restored separately after this replacement;
typed live directives and declarations are excluded from replacement history and
folded/redeclared at the new window. Version-1 checkpoints persist no instruction
snapshot. Retry boundaries are based on summarizable history but mapped back to
positions in the full input, so every shorter candidate remains an exact prefix
with unchanged instruction/tool declarations. Destination-fit checks include replacement history, effective pinned
bodies and the current rendered instruction set, never authority from summary prose.

A successful summary does not guarantee the rebuilt request fits. A successful
partial checkpoint remains saved for live pre-turn/mid-turn and manual compaction
when the preserved tail still exceeds capacity; only the blocked turn fails, with
no oversized normal dispatch or repeated automatic-compaction loop. A new prompt
is not committed before admission. Manual `/compact` refreshes telemetry without
normal dispatch. Edited-prefix checkpoints remain transactional with edit
acceptance: rejection or cancellation preserves the entire original conversation.
If no nonempty replay-safe prefix fits, no checkpoint or empty-source summary is
created. Recovery may require a larger compatible profile or a fresh session.

Summary attempts are bounded by the descending nonempty replay-safe source
prefixes, not a call counter. Local rejects and replay-boundary checks make no
model calls. Many prefix attempts may add latency and paid calls; cancellation
and non-size provider errors still stop the operation. Only exact
structured completion codes `context_too_large` and `context_length_exceeded`, or
completion HTTP 413, carry the core `ModelInputTooLarge` marker. This includes
WebSocket/SSE error events and structured failed/incomplete terminal responses.
They return directly to core without transport reconnect/backoff of the same
oversized request. Generic 400, authentication/quota/output-limit errors, and
misleading prose are not size signals. Count-endpoint errors retain conservative
estimate fallback rather than becoming completion retries. Tool-call summary replies
are never executed: one retry at the same boundary changes only
`forbid_tool_calls` to emit `tool_choice: "none"`; a second tool-call reply fails.
The engine resets provider continuation after each synthetic dispatch, not before
it, so a full-history summary may use `previous_response_id` and send only the
trailing instruction.

A local-summary checkpoint can retain opaque replay-only tail items and is then
not automatically portable. The `/model` and `/model-session` preflight and
explicit confirmed conversion still enforce source-profile provenance. Explicit conversion keeps
its existing full-summary/retained-user behavior; adaptive prefix retries do not
change that contract. Unsupported persisted formats fail before conversion or
provider calls; there is no skill-history migration path.

Remote Responses compaction remains opt-in per provider:

```jsonc
{
  "providers": {
    "openai": {
      "compaction": {
        "url": "https://api.openai.com/v1/responses/compact",
        "request_timeout_seconds": 300
      }
    }
  }
}
```

No URL is inferred from `base_url`. Remote output is opaque replay-only history,
so a root with more than one **selectable catalog profile** must use local-summary
compaction, even when Build/Plan/Review currently share one profile. Single-profile roots and
single-profile Explore and Builder children may use their configured remote endpoint,
with independent role-specific context limits and compaction lifetimes.
Remote requests reuse the normal request's instructions, tools and full input
without trimming tool outputs. Returned `role: "developer"` and
`type: "additional_tools"` items are removed before the remaining items are stored
verbatim as opaque replay-only history; an empty result after filtering fails.
Zevria re-folds directives and declares tools in the new window. With
`developer_messages: false`, user-role directive copies cannot be distinguished
from ordinary user content and may persist in remote output.

## Changing models without clearing history

In the TUI, bare `/model-session` selects an existing configured profile for the
root composer's captured model role and then requires explicit confirmation of a
supported reasoning level. It retains the complete per-mode selection for live
TUI replacements and, once a conversation is saved, for resume, **without changing
config**. Model/reasoning-only changes in a new session stay in memory and create
no transcript file; global-default changes still persist through `/model`. The picker says
**session only — kept on /new, fresh handoff and resume; config unchanged**. Bare `/model` uses the same searchable picker, visibly labeled
as saving the role's global default too.

Build targets Build; Plan targets Plan. In Build, `/model-session` saves the
session selection, while `/model` also saves the global Build assignment
(`modes.build.provider/model/reasoning_level`) in `config.toml`. Request-local
orchestration uses that same selection; it adds no model setting. Neither command creates providers,
discovers models, changes credentials, or modifies the other mutable role's
selection, Review, Explore, Builder, workers, or permissions. Only an idle writable
interactive root can select; active work, recall, inspect-only panes, and pending
Plan approval block management rather than queue it. The first Enter only opens
the reasoning stage; a second Enter confirms the complete selection. The current
level is highlighted if supported, otherwise the first listed level. Left/Backspace
returns to profiles and Esc cancels before submission without changing files or routes.
Both commands are argument-free; a leading space sends either as literal text.
There is no ACP model-selection API, new model-role config key, CLI flag, or
automatic catalog reload.

Independent fresh sessions (including separate CLI/ACP launches and native workers)
read global defaults from the `[modes]` assignments in the startup-captured
`config.toml`, validated against its sibling catalog, including a custom
`ZEVRIA_CONFIG`. TUI `/new` and `/implement-fresh` replacements, including the
fresh-approval dialog action, inherit **both current Build and Plan selections**
(provider, model, and reasoning level), even when originally derived from configuration.
The modes remain independent: fresh implementation uses the saved Build selection,
not the Plan model. `/new` resets conversation, Plan state, and active skills without
an automatic provider request; it does not reset model preferences. Later default
changes do not replace the inherited pair. Resumed roots restore their last
successfully selected Build and Plan identities **and reasoning levels** from a
version-1 `zevria_session_models` header, including a session-only switch acknowledged
just before closing with no later assistant response, once a conversation has been
saved. A subsequent `/new` inherits that restored pair. A new unused session has
no transcript and is not resumable after close; live `/new` inheritance still
retains its current choices. Existing historical files keep their current rules:
empty/models-only abandoned roots are excluded, but mode-bearing roots remain
resumable even without messages. Review, Explore, and
Builder adopt current globals; external workers are unchanged. Exact, case-sensitive
provider/model keys resolve against current endpoints, credentials, capabilities
and limits, not a frozen historical config. Removing a saved catalog identity or
supported reasoning level blocks resume or inherited startup with actionable guidance;
no fallback is inferred. Restore the entry/level or independently launch with valid
defaults; another inheriting `/new` cannot fix an unavailable choice. Resume does not save globals or make model,
counting, or conversion calls to recover selections. Other active sessions keep
their routes. Original native replay and source identities are preserved;
destination projection—not transcript relabeling—makes visible history portable,
even for two models under the same provider. Inherited routing and compaction policies
use current catalog capabilities and limits. The complete pair is staged in memory
and published with the first accepted conversation, including an opening Plan
handoff, before generation. Publication does not rotate session/cache identity. Selection metadata is not
provider input, instructions, or Plan content; it does not change cacheable prefix
construction, and a replacement still has a new session/cache identity.

An opaque checkpoint from another profile or a smaller input ceiling may require
confirmed conversion. The dialog names source/destination and warns that a model
call costs tokens, can lose summary detail, and changes shared root context for
both root modes, even when only one model role's session selection changes.
It also states whether config will change. A configured compatible source summarizes filtered
conversation with captured application/file and explicit no-tool maintenance
guidance in `instructions`; all ordered directives are excluded. Source calls use
the captured pre-switch role's explicit reasoning, not a catalog default. If the
opaque source does not support that level, select it with a supported level first.
Destination counting and capacity rechecks use the requested reasoning level. Incompatible source input, unavailable sources, multiple opaque
sources, empty summaries, or a summary still above the destination limit fail
safely. No foreign checkpoint reaches a destination's completion **or counting**
endpoint. Staying with its source or starting fresh are recovery options.

Both scopes prepare the same route, limits, replay preflight, capacity checks,
and optional conversion. Configuration revision is revalidated after asynchronous
preparation and before persistence. A session-only switch retains that revision
and never calls a config writer or enters its transaction; a readable, read-only
config is sufficient. A validated portable checkpoint is saved first if needed,
then the selected role's session header is replaced before the active route is
installed and success acknowledged. This is memory-only for pending new sessions;
materialized transcripts retain their existing atomic header-replacement behavior. If this header save fails, active and saved
selections are unchanged, **config was not modified**, and no newly committed
global revision is returned. Any already-saved checkpoint is reported explicitly;
retrying `/model-session` can reuse it without another summary call.

`/model` keeps its checkpoint → global assignment → session header → route order.
If saving the default fails, a checkpoint may remain while the active model and
default stay unchanged. If the later header save fails, the global default **has
changed** but active and saved session selections have not; the rejection returns
the committed global revision for retry. For a materialized transcript, a crash
after header commit restores the selection without a UI acknowledgement. A compatible already-active
`/model-session` selection is a no-op with no transcript rewrite or route reset,
but preflight is never skipped when conversion is required. `/model` still saves
an already-active session-local choice as the default. Only the selected TOML
assignment changes; comments, value decoration, unrelated settings, other modes,
permissions, and no-op modification times remain preserved. Both inline assignments
and ordinary mode tables are supported. JSONC remains byte-for-byte unchanged and
need not be writable. Both source snapshots are rechecked before publication.
Model, skill, and theme writers coordinate using the same `<config>.skills.lock`.
Every switch saves the explicitly confirmed reasoning level alongside its profile.
Preview correlation includes that level, so altering it invalidates confirmation. Reusing
provider/model keys for a materially different deployment remains unsafe: identities
are the replay trust boundary, not an endpoint fingerprint. Gateway incompatibility
never triggers a retry that leaks foreign raw records or clears history.

Root and native-worker transcripts require valid saved Build and Plan selections.
Missing, malformed, duplicate, misplaced, or unsupported headers reject startup
before writable repair or provider calls. The offline `sessions recover-models`
command is removed: there is no inference from responses/defaults, metadata
initialization, or automatic backup. Original bytes remain untouched; start a
fresh session or use a matching older binary for older data. Missing saved
profiles or reasoning levels require restoring their exact supported catalog entry
or starting a new session. Only current version-1 complete selections are accepted; no saved metadata is upgraded.

Current provider interoperability and explicit profile conversion remain
supported; they do not relax persisted-format validation. A stable `.jsonl.lock`
root lease rejects cooperating live writers, survives transcript replacement, and
is held through shutdown/cleanup; it cannot coordinate older binaries or external
editors. Leases remain independent of file existence; shutdown does not flush a
pending metadata-only session. Older binaries may reject the new header. Existing
empty/models-only abandoned roots without a mode are skipped in listings and
`--continue` and cleaned up on shutdown; existing mode-bearing, malformed and
substantive logs remain discoverable. No historical purge is performed. Metadata is never model input or frontend
history, and history edits preserve the current selection snapshot.

## Reasoning within model selection

Use `/model-session` or `/model` and choose the same profile to edit only its
reasoning level. These are the only interactive model/reasoning commands.
`/reasoning` and `/reasoning-session` have been removed; no compatibility alias
is registered. All Build requests use the Build selection; Plan is independent.
Review, Explore, and Builder are configured only through their TOML assignments.

Compatible same-profile reasoning edits use the same config/header transaction
and partial-save reporting as any model selection, but install the live level in
place. Subsequent requests use top-level `reasoning.effort`. This **does not rewrite
instructions, tools, or input, change the prompt-cache key, reset the router, or
reconnect the WebSocket**. Changed request properties cause one full-input resend
on the same socket, preserving the identical prefix; normal continuation resumes
thereafter. Reasoning is never part of the profile/cache key.

Fresh roots stage explicit Build and Plan selections, including their effective
levels, in memory. The first accepted conversation saves the complete current
header; resume then retains both even after defaults change. An unsupported saved level
is an error, never a startup fallback or silent header rewrite. Review/Explore/Builder
retain their opening/resume-config snapshot behavior. Management and its metadata
never enter the prompt transcript, prompt history, instructions, or ACP replay.

## Prompt caching and child isolation

When enabled, the cache key is derived from the root or child session ID plus
the provider key and model ID. Each unique root profile keeps its own key and
continuation chain. Every Explore or Build child receives a fresh single-profile
Explore or Builder router, transcript, cache identity, transport, continuation
state, and compaction lifetime—even when it selects the same configured profile
as a root mode. Neither roots nor children have a model-call count ceiling.
Root model-role routes remain Build/Plan/Review only; request-local orchestration
uses Build and a selectable child profile does
not install a child-role route. Both child factories retain their session-opening
configured assignments across root Build/Plan mode or model changes. The factory
creates the router with `create(session_id)`; each engine supplies explicit
request declarations from its own registry. Explore shares command-only tools;
Build gets a private directory-rooted command/task/edit/write/delete registry.

Cache hits are observational upstream state, not a correctness requirement.
Full local replay remains authoritative after cache expiry, reconnect, or
process resume.

## Diagnosing `MissingSessionID` from OpenCode Go

An HTTP 400 error stating `MissingSessionID` and "Request is missing
x-opencode-session" means the gateway requires that routing header. Add
`"session_id_header": "x-opencode-session"` to the affected provider's existing
`providers.<key>` object in the sibling `models.jsonc`. Keep its credentials and
all other settings unchanged; do not create a duplicate object.
For a local provider key named `deepseek`, use `providers.deepseek`—the key
itself does not enable vendor-specific behavior.

After installing a binary supporting this setting, restart Zevria, resume the
session, and submit a harmless diagnostic prompt. Upgrading alone does not opt
providers in, and the 400 remains terminal rather than causing an automatic
retry or inferred header. Other gateways may require a different name; consult
their configuration contract. Resolving the missing header does not establish
complete Responses compatibility or fix unrelated subsequent errors.

## Diagnosing an incompatible endpoint

Common Chat-shaped incompatibility signs include:

- IDs beginning with `chatcmpl-`;
- `object: "chat.completion"` or `"chat.completion.chunk"`;
- top-level `choices` arrays;
- `choices[].delta.content`, `reasoning_content`, or Chat `tool_calls`; and
- no Responses `type`, output items, response ID, or terminal status.

Confirm that `providers.<key>.base_url` is the exact Responses route and inspect
a bounded sample outside Zevria. Changing model IDs or compatibility flags
cannot turn a Chat-only service into a Responses endpoint.
