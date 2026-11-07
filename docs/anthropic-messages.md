# Native Claude: Anthropic Messages

Zevria includes a native `anthropic.messages` wire adapter alongside
`openai.responses`. Choose the protocol **per provider** in `models.jsonc`.
Different roles and `/model` selections can use different protocols in one root;
Explore and Builder children get independent runtimes for their selected profile.
This is not Claude Code: the ensemble agent named `claude` runs Claude Code over
ACP, with its own installation and authentication. See [ensemble.md](ensemble.md).

## Configuration

`models.jsonc` (keep literal credentials private):

```jsonc
{
  "providers": {
    "anthropic": {
      "protocol": "anthropic.messages",
      "base_url": "https://api.anthropic.com/v1/messages",
      "api_key": "replace-with-api-key",
      // "auth": "bearer", // default: "x-api-key"; gateways/OAuth tokens
      // "session_id_header": "X-Conversation-ID",
      "compatibility": {
        "prompt_caching": true,
        "system_messages": true,
        "eager_tool_input_streaming": true,
        "strict_thinking_binding": false
      },
      "beta_headers": [],
      "input_token_count": {
        "enabled": true,
        "derive_url": true,
        "request_timeout_seconds": 30
      },
      "models": {
        "claude-opus-5-5": {
          "context_window_tokens": 1000000,
          "max_output_tokens": 64000,
          "retained_user_tokens": 20000,
          "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
          "thinking_display": "summarized",
          "refusal_fallback": "default"
        }
      }
    }
  }
}
```

The generated template includes this Claude example. Confirm the model, limits,
and features with your endpoint; Zevria does not discover capabilities. All five
roles remain required in `config.toml`, even when every role uses the same model:

```toml
[modes]
build = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }
plan = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }
review = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }
explore = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "medium" }
builder = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }
```

The provider key and exact model key are stable, case-sensitive replay identities.
Omitting `protocol` means `openai.responses`, not auto-detection. Schemas are
strict: Responses-only `supports_websockets`, `compaction`, `web_search`, and
`reasoning_summary_level` are not accepted on Messages providers/models.

`max_output_tokens` is required and positive. The default `input_token_limit` is
`context_window_tokens - max_output_tokens`; an explicit limit cannot exceed that
ceiling. Context, retained-user budget, reasoning-set uniqueness, network and
session-header validators also apply. `none` and `minimal` are rejected for
Messages models. Credentials are literal; no environment interpolation occurs.
`base_url` must be a full HTTP(S) Messages endpoint, not just a host root.

Gateway-specific `additional_params` are allowed except Zevria-owned fields:
`model`, `messages`, `system`, `tools`, `tool_choice`, `stream`, `max_tokens`,
`thinking`, `output_config`, `cache_control`, `fallbacks`, `temperature`, `top_p`,
`top_k`, and `context_management`. Beta entries must be nonempty HTTP tokens.

## Wire contract and gateways

The adapter uses raw HTTP, not an SDK or a Responses translation gateway:

- completion: `POST {base_url}`, JSON with `stream: true`;
- counting: `POST {base_url}/count_tokens`, unless explicitly overridden;
- headers: `anthropic-version: 2026-10-21`, `content-type: application/json`,
  `accept: text/event-stream`, and `x-api-key` by default;
- `auth: "bearer"` instead sends `authorization: Bearer <api_key>`;
- an optional configured session header carries the persistent conversation ID;
  authentication and transport headers cannot be overridden through this option;
- `anthropic-beta` is the deduplicated union of `beta_headers` and implied betas.

The endpoint must emit `message_start`, `content_block_start`,
`content_block_delta`, `content_block_stop`, `message_delta`, and `message_stop`
SSE events. Ping events are accepted but do not reset the inactivity clock.
Text, thinking, signature, partial tool JSON, and citation deltas are accumulated.
Unknown **blocks** survive in native replay; unknown **deltas** fail closed rather
than corrupt a signature or tool input. There is no WebSocket or server-side
continuation. Sampling parameters are deliberately never sent under this wire
version.

## Effort and thinking display

Every completion sends adaptive thinking and `output_config.effort`.
`low`, `medium`, `high`, `xhigh`, and `max` map one-to-one from the selected
`reasoning_level`. The per-model `thinking_display` is `summarized` (default),
`omitted`, or `updates`. `updates` implies
`thinking-display-updates-2026-08-18`. Readable thinking streams as reasoning
previews. Omitted display can still produce an empty thinking string with a
signature; it must not be discarded or trimmed.

## Prompt caching

By default there are exactly two explicit `{ "type": "ephemeral" }` breakpoints:

1. The single system text block containing `ModelRequest.instructions`. Tools
   render before it and share this stable cached prefix.
2. The last non-thinking block of the final message. This breakpoint moves on
   each new turn; no marker is placed on a thinking/redacted-thinking block.

Tools retain registry order after allow-list filtering. Routing IDs, retry data,
and other per-request metadata never enter system/tools/messages. Set
`compatibility.prompt_caching: false` to omit both markers for a gateway without
explicit cache control. Cache eligibility still depends on the endpoint's
minimum-length, model, and lifetime rules; a short prompt need not yield a hit.

Each terminal SSE response logs an INFO record with `input_tokens`,
`cache_creation`, `cache_read`, `output_tokens`, `served_model`, `stop_reason`,
`upstream_request_id`, and `stripped_thinking_turns`. Verify growing cache reads
on repeated, sufficiently long prompts. Canonical usage counts total input as
ordinary input + cache creation + cache read, with cached tokens = cache read.
Cache creation is also logged separately, not confused with cache reads.

## Preserved thinking and binding

Native ledgers retain raw assistant blocks, including all signature bytes,
redacted thinking, unknown blocks and their order. A replay is native only when
**both** its source profile and protocol match the destination.

Thinking is bound to the preceding system, tools and conversation. The adapter
stores a versioned, length-prefixed SHA-256 binding over canonical JSON, excluding
cache-control markers. At each historical assistant position it compares the
actual projected prefix to the saved binding. A match preserves thinking
verbatim. A mismatch sends only that turn's text/tool-use blocks, not its
thinking, redacted thinking or unknown opaque blocks.

This intentionally loses reasoning continuity after keep-tail compaction,
Plan/Build instruction or tool-policy changes, maintenance requests, or edited
file guidance/skills on resume. It does not alter the saved ledger: switching
back to an unchanged original prefix can restore its original thinking. New
post-compaction responses bind to the new history and replay normally.

A specific HTTP 400 saying thinking is “bound to a different conversation”
triggers one WARN-logged retry with all historical thinking removed (unless
`x-should-retry: false` forbids it). It shares the recovery controller and attempt
cap, recomputes the binding, and never repeats tool work.
A second mismatch is terminal.

For CI-style live checks, opt into `compatibility.strict_thinking_binding: true`.
It adds `thinking-binding-controls-2026-08-01` and
`thinking.block_binding.prefix_mismatch_behavior: "error"`. Nonempty
`input_transformations` are logged at WARN so silent upstream repairs are visible.
No live API tests run as part of the local test suite.

## Ordered directives and compatibility

Typed developer/request directives project as mid-conversation `role: "system"`
messages. They follow a user message and are last or precede an assistant turn.
Pending directives defer past the next user message as necessary; adjacent
same-role messages and consecutive directives merge deterministically.
Raw generic `Message::System` input is rejected, never lifted into the prefix.

For models without system-message support (for example Sonnet 5, Opus 4.6/4.7,
Sonnet 4.6, Haiku 4.5), set `compatibility.system_messages: false`. Directives
then become text blocks in the adjacent user turn, with weaker authority. Tool
results always precede other user content, including these directive blocks.

## Tools and eager inputs

Client tools use `{name, description, input_schema}` without `strict`; the
registry schemas include optional fields. `eager_input_streaming: true` is added
by default so large write/edit inputs keep meaningful SSE progress flowing.
Tool JSON is parsed strictly only at `content_block_stop`; incomplete/invalid
JSON is a retryable failed attempt and **never executes**. On exhaustion the
error names `compatibility.eager_tool_input_streaming`, which can be disabled as
a gateway workaround. A completed tool call must have exactly one result in the
next user message. IDs in portable histories are deterministically re-minted and
results are re-correlated by occurrence.

No-tool maintenance requests omit tools unless the retained history contains a
tool exchange. In that case they send the full registry with
`tool_choice: {"type":"none"}` so historical tool blocks remain valid while
new calls remain disabled.

## Counting, stops and recovery

Counting sends only `model`, `system`, `tools`, `tool_choice`, `messages`, and
`thinking`, with the same prompt shaping as completion. The shared
`input_token_count` table supports `enabled`, `derive_url`, `url`, and
`request_timeout_seconds`. HTTP 404/405/501 cache an unsupported result for that
runtime; subsequent counts do not keep hitting an absent endpoint.

- `end_turn`, `stop_sequence`, `tool_use`: successful completion.
- `max_tokens`, `model_context_window_exceeded`: terminal; inspect
  `max_output_tokens` and context limits. Partial output is discarded.
- `pause_turn`: terminal in v1 (no server tools).
- `refusal`: terminal, including `stop_details.category`; partial output is
  discarded, not committed as a successful assistant answer.
- Empty assistant content is terminal.

`refusal_fallback: "default"` opts into `fallbacks: "default"` and
`server-side-fallback-2026-07-01` (enabled in the example). The last fallback
marker establishes the echo boundary: only text before it survives; old
thinking, client/server-tool calls and unknown blocks are dropped along with
all fallback markers. Only post-boundary tool calls can execute. Check
`served_model`, which may differ from the configured model.

HTTP 408, 429, 5xx/529, SSE overload/API or `server_error`/`rate_limit_exceeded`
errors, transport failures, idle timeouts, premature EOF and malformed tool JSON
use the same `recovery.rs` controller as Responses: a **300-second outage window**
from the first attempt or latest meaningful progress, **30-attempt absolute cap**,
and **four interrupted-response cap**. The window schedules retries, never
interrupts a running attempt or total useful work. Internal children and synthetic
summaries inherit it; exact counts retain separate deadlines.

Backoff begins at 0.5 seconds, caps at 15 seconds with 75–100% jitter, and resets
after progress. `retry-after-ms` and numeric `Retry-After` (including decimal
seconds) replace backoff for retryable failures, including 429, 503 and 529. A
hint longer than the remaining window stops immediately with the requested delay;
there is no fixed 60-second cutoff. Malformed and HTTP-date hints are ignored.
`x-should-retry` overrides HTTP status defaults except typed input-size failures.
HTTP 400/401 otherwise stay terminal except the existing one-time, zero-delay
thinking-binding repair, which also passes through the controller. HTTP 413 or a
400 “prompt is too long” remains typed `ModelInputTooLarge`.

Local header timeouts escalate the next request-start deadline 30 → 60 → 120 →
180 seconds, capped at the response idle timeout; longer header waits warn at the
stall threshold. Quiet warnings carry controller-computed `will_retry`.
`TurnRetrying.attempt` is the **upcoming attempt** (2, 3, …), with the exact delay
and next-start window remainder; neither event exposes an attempt denominator.
Upstream `request-id` and original transport source chains are retained (reqwest
URLs removed). Retry details are `category: full chain`; final Display includes
attempts/elapsed time, stop reason, full last cause and actionable advice for
headers, idle timeout, connectivity/proxy, or provider unavailability. Shared WARN
logs record each failure and exhausted recovery. See `responses-compatible.md`
for the policy and classification table.

Failed-attempt previews are cleared; partial tool calls never execute. Transient
recovery reuses the immutable `PreparedRequest`, preserving byte-identical bodies,
headers, instructions and cacheable prefix. Only the existing explicit
thinking-binding semantic repair changes its prepared body; network metadata is
never serialized.

## Mixing protocols and limitations

`/model` and `/model-session` can switch between Messages and Responses profiles.
Cross-profile/protocol projection keeps visible text and supported calls/results,
but drops signatures, reasoning, opaque blocks and native IDs. Replay-only
opaque checkpoints still require confirmed portable conversion. Repointing a
provider key to another protocol never sends foreign native items.

Messages has no native remote compaction. `compact()` reports `Unsupported`,
which selects core's existing local-summary path. Roots with multiple selectable
profiles also keep the existing portable-compaction policy. Responses requests,
registered tool prefixes and old transcript fixtures are unchanged.

Hosted web search is a follow-up, not part of this adapter. Native Bedrock and
Vertex authentication are not supported; use the Claude API or a compatible
HTTP gateway. PDF documents and images use base64 sources; unsupported content
fails locally. Claude image limits (for example 8,000 pixels per side) can be
stricter than Zevria's 16,384-pixel admission limit. Following the existing image
policy, stricter upstream image rejections surface as provider errors, not
silent resizing or mutation of saved content.
