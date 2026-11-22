# MCP servers

Zevria connects interactive root sessions to Model Context Protocol (MCP) servers over **stdio** or **Streamable HTTP** using `rmcp` 3.11.0. A server can expose model-callable tools, readable resources and URI templates, and prompts you can run yourself. Explore and Build subtasks and native Ensemble workers do not inherit these connections or their tools.

## Configure a server

Add entries to your global `config.toml` (`~/.zevria/config.toml`, or the path selected by `ZEVRIA_CONFIG`). No MCP setup is necessary for ordinary sessions.

```toml
[mcp]
enabled = true
startup_timeout_seconds = 30
tool_timeout_seconds = 120
max_output_bytes = 262144

[mcp.servers.catalog]
command = "/path/to/catalog-mcp"
args = ["--stdio"]
cwd = "."
env = { CATALOG_PROFILE = "work" }
plan_mode = "read_only"

[mcp.servers.notes]
transport = "http"
url = "https://notes.example.org/mcp"
headers = { "X-Workspace" = "engineering" }
bearer_token = { command = "./mcp-token", args = ["notes"], timeout_seconds = 15, refresh_interval_seconds = 300 }
plan_mode = "none"
plan_tools = ["search_notes"]
```

Names use 1–48 lowercase ASCII letters, digits, hyphens, or underscores. Do not start or end with punctuation or use `__`. Zevria infers the transport from `command` or `url`; use only one transport's settings. Stdio runs the executable directly, not through a shell, and resolves relative `cwd` from the startup workspace. HTTP connections do not follow redirects. Use HTTPS for credentials except on a trusted loopback endpoint.

A bearer token or custom header can be a literal string or a helper command. Helpers run without interaction or shell interpolation; relative executable paths resolve next to `config.toml`. A helper prints a single UTF-8 HTTP header value to stdout. Zevria bounds the output and runtime, caches successful values for the configured interval, and retries once with refreshed command credentials after HTTP 401. Use `bearer_token` without the `Bearer ` prefix, or a custom `Authorization` header, not both. URLs in the inspector omit query strings, and credentials, environment values, and stdio arguments are not included in inspection output. A server's own results or diagnostics can still contain sensitive material.

Use `enabled = false` to keep a server in configuration without starting it. An unavailable optional server is reported in the startup notices and inspector; `required = true` makes a failure prevent session startup. Set individual startup and tool timeouts on a server to override the global defaults. Text results share the configured byte budget; supported images are checked and prepared separately using Zevria's image pipeline. Oversized and unsupported results may be truncated or replaced with a notice.

## Build and Plan access

Build can use all enabled tools from connected servers, subject to `enabled_tools` and `disabled_tools`. Those lists and `plan_tools` use **original server tool names**, not the provider-facing names. An explicit disable wins over any allow-list. Plan applies the following per-server policy:

| `plan_mode` | Tools available in Plan | Resources and explicit prompt invocation |
| --- | --- | --- |
| `read_only` (default) | Tools with `readOnlyHint: true` and no `destructiveHint: true`, plus exact `plan_tools` entries | Available |
| `none` | Only exact `plan_tools` entries | Unavailable |
| `all` | All enabled tools | Available |

The hints come from the server and are not enforcement or a sandbox. A stdio server or credential helper has the process's OS permissions. A remote server receives the arguments and resources you ask it to process. Connect only to servers you trust; use an explicit Plan allow-list if you do not trust their annotations. Stage changes in tool catalogs take effect at an idle or turn-start boundary. If a connection dies, subsequent calls are denied.

## Inspect, read, and run

- **`/mcp`** opens the inspector. It shows connection state, transport, and the tools, resources/templates, and prompts each server advertises. It can be opened during a turn; reload, reconnect, and enable/disable require an idle session. TUI controls: `j`/`k` or arrows to move, `Enter` to inspect, `Tab` to switch sections, `Space` to toggle, `r` to reconnect, `R` to reload, and `Esc` to go back. A session started without MCP servers needs a new session to adopt its first configuration.
- **Resources** are available to the model through `mcp_list_resources` and `mcp_read_resource`. The read call takes a server name and a concrete URI, including one expanded from a template. Zevria sends the URI to that server; it does not open files or download links based on the URI itself.
- **Prompts** are user-controlled. In the Prompts tab, select a prompt, fill its arguments, and run it. You can also type `/prompt notes:summarize topic="release notes" format=short`. Quotes protect spaces; `\"` and `\\` escape quotes and backslashes inside a quoted value. The expansion enters the conversation as a normal user prompt with its server/name provenance, not as an assistant or developer message.

## CLI without model configuration

```sh
zevria mcp list [--json]
zevria mcp inspect <name> [--json]
zevria mcp test [<name>] [--json]
zevria mcp enable <name>
zevria mcp disable <name>
```

`list` reads configuration without running servers or credential helpers. `inspect` and `test` use the configured transport and discover capabilities, then disconnect. They do not run tools or prompts, and never start disabled servers. `test` returns a nonzero status for failed connections. No subcommand requires `models.jsonc` or provider credentials.

## ACP and caching

Interactive ACP clients may supply `mcpServers` to `session/new`, `session/load`, or `session/resume`. Stdio and Streamable HTTP are supported; legacy SSE is rejected. Delegated servers are session-local, must use distinct names, and do not change `config.toml`. Supply the delegation again on a later load or resume. See [ACP agent](acp-agent.md) for the trust boundary.

Without configured or delegated servers, Zevria registers no MCP resource helpers and leaves the existing instruction and tool bytes unchanged. With MCP active, the fixed instructions contain no server names, tool names or server-supplied instructions. Zevria pins the initial tool list for the active history window; newly discovered or revised tools are appended as additional declarations. Providers configured for inline declarations keep the top-level tools unchanged; provider compatibility modes that fold declarations into top-level tools cannot preserve that part of the wire prefix. Server instructions and revocations are separate ordered records. Disabling a tool changes execution permission immediately at the boundary; it does not rewrite past declarations. Compaction can install a new initial list.
