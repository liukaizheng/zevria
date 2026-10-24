# Zevria Privacy Policy

Last updated: October 24, 2026

Zevria is an open-source coding assistant that runs on the machine where you
launch it. **Running locally does not mean all processing stays local.** Zevria
sends information to the model endpoints and integrations used for your tasks.

This policy describes the Zevria application and installer scripts maintained in
this repository. It does not replace the privacy policies of model providers,
gateways, editors, external agents, command-line tools, or distribution services.
Their processing, retention, and use of information are governed by their own
policies and your arrangements with them.

## Information used for coding assistance

Depending on the task, configuration, and tools used, Zevria's model requests can
include:

- Prompts, conversation history, assistant responses, and context summaries.
- Images you submit, including pasted screenshots.
- Application instructions, user and project guidance, and activated skill
  instructions or resources.
- Workspace and file paths, source code, file contents, and changes read or
  produced by tools.
- Shell commands, tool arguments, command output, errors, and tool results.
- Delegated-agent reports and evidence used to prepare a combined response.

Tools can add information to later requests beyond what you initially type.
Source files, screenshots, and command output may contain credentials, personal
information, or confidential material. Do not assume such content is automatically
identified or redacted before transmission or local storage.

The terminal UI reads the native clipboard when you invoke its paste action in
an editable composer; it does not continuously monitor the clipboard. Submitted
images become part of the conversation and its saved history. See
[image input and clipboard behavior](docs/image-input.md).

## Configured model providers and supporting services

You configure the model endpoints and credentials in `models.jsonc` before model
use. An endpoint can belong to a model provider, a gateway, or a locally hosted
service. The endpoint operator receives the request content and ordinary network
metadata, such as the connecting IP address.

In addition to ordinary model requests, Zevria can make these requests:

- **Exact input-token counting:** enabled by default and used conditionally, such
  as for image-bearing or capacity-sensitive requests. Counting can send the
  prepared instructions and full input to an endpoint derived from the Responses
  URL or to a separately configured counting URL. It is not just a transmission
  of a token total. You can disable it with `input_token_count.enabled = false`
  in the provider configuration.
- **Context summarization and compaction:** automatic or user-requested context
  reduction can send conversation context to a model. If you configure a remote
  `compaction.url`, Zevria can also send context to that endpoint. No separate
  remote-compaction URL is configured by default; ordinary model-based
  summarization can still occur without one.
- **Provider-hosted web search:** disabled by default. When enabled through
  `web_search.enabled` and available to the task, the provider performs search.
  Search terms, requested URLs, and any configured approximate location may be
  processed by the provider and its search services. Zevria does not
  automatically discover your location for this setting.

Your configured API key is sent for authentication to the model, token-counting,
and remote-compaction endpoints that are used. Separately configured supporting
endpoints can be on different hosts. Only configure endpoints you trust with
both your content and credentials.

Requests also contain model settings and tool definitions, and may contain
response IDs, prompt-cache identifiers, or a configured session-ID header. Normal
Responses model requests send `store: false` by default. Compatibility settings
and other request types differ; for example, token-counting requests omit that
field. **These request fields are not guarantees about a provider's logging,
retention, caching, or use of data for training.** Review the policies of every
provider and gateway you use. Zevria does not control their processing locations
or deletion practices.

See [provider configuration and compatibility](docs/responses-compatible.md) for
endpoint and feature settings.

## External programs and editor integrations

Zevria's command tool runs programs with the operating-system permissions of the
Zevria process. Commands can read files outside the workspace, access inherited
environment variables, and contact external services. Tool output can enter the
model context and saved history. **Zevria does not provide an operating-system
sandbox.** Plan and Explore restrictions are not security isolation.

Ensemble workflows launch configured ACP agents as local subprocesses. Workers
can receive task input, workspace information, supported image attachments, and
follow-up feedback. Their reports and selected evidence can be sent to the model
that synthesizes the combined response. External agents may use their own
credentials, provider connections, tools, and storage. They can inherit process
environment variables. The default ensemble configuration includes third-party
adapters invoked through `npx`, which may download packages when launched. These
programs' behavior and privacy practices are separate from Zevria's own provider
configuration. See [ensemble workflows](docs/ensemble.md).

When you run Zevria as an ACP agent, the connected editor or client sends prompts
and receives responses and tool activity, which can include file contents and
changes. Clients can also access saved sessions exposed by the ACP interface.
The ACP frontend does not request client permission before executing tools. Only
connect clients you trust with the workspace and its history. See
[ACP integration](docs/acp-agent.md).

## Local storage and diagnostics

Zevria saves data locally to support configuration, conversation continuity,
resume, recovery, and troubleshooting. Common locations are:

| Location | Information it can contain |
| --- | --- |
| `~/.zevria/config.toml` and `~/.zevria/models.jsonc` | Settings, provider endpoints, and literal API credentials. `ZEVRIA_CONFIG` selects another configuration file and its sibling `models.jsonc`. |
| `<workspace>/.zevria/sessions/` | Root conversation history, submitted images, skill activation records, tool calls and results, model settings, and provider replay data. |
| `<workspace>/.zevria/subsessions/` and `ensemble-sessions/` | Histories of delegated subtasks and native ensemble workers. |
| `<workspace>/.zevria/agent-runs/` and `plans/` | Ensemble evidence, worker journals and diagnostics, and generated plans. |
| `~/.zevria/logs/zevria.log` | Operational logs, errors, paths, request or response identifiers, usage information, and other diagnostic details. The directory and log level are configurable under `[log]`; `RUST_LOG` overrides the configured level. |

Here `~` means the home directory resolved by the runtime running Zevria. On
native Windows, the workspace state directories above are under
`<workspace>/.zevria/windows/` instead. WSL uses its Linux home, configuration,
and state separately. External agents can create additional files elsewhere.
Builds with the optional cache-diagnostics feature can also write local
fingerprints and request/cache metadata beside session history.

Skill activation records retain instruction bodies even after a skill is revoked;
revocation does not erase saved history. Application and file guidance are not
saved as engine instruction records, but conversation text and summaries can
quote them. See [guidance](docs/guidance.md) and [skills](docs/skills.md).

**Zevria does not encrypt configuration, history, or log files at the application
level.** Submitted images are embedded as base64 in saved history; base64 is not
encryption. Worker journals and diagnostics can also contain sensitive prompts,
messages, tool inputs and outputs, or images. Limited diagnostic redaction is
not a general secret scrubber. Protect these locations from unauthorized access,
accidental commits, backups, and synchronization to services you do not intend
to receive them.

Zevria has no built-in first-party analytics or automatic crash-report upload
service. Application logs, panic messages, and cache diagnostics are written
locally rather than automatically uploaded to the maintainers. This does not
prevent configured providers, external agents, commands, or other software from
making their own network requests.

## Retention and deletion

Saved conversation histories, worker artifacts, and the application log file do
not have a general automatic age-based expiry. They may remain until you remove
them. Summarizing model context or starting a new conversation does not erase
previously saved history.

To delete Zevria's workspace history and generated artifacts, first stop **all
sessions and workers using that workspace**, then run `zevria clean` from that
workspace. **It deletes the five workspace state directories `sessions/`,
`subsessions/`, `ensemble-sessions/`, `agent-runs/`, and `plans/` without
confirmation**, using the current runtime's storage root.

This cleanup does not remove global configuration or logs, skills, unlisted
files, external agents' separately stored records, or backups. Remove those
separately if needed. It is ordinary filesystem deletion, not a secure-wiping
facility. Deleting local data does not delete copies already received by
providers, gateways, clients, or other services; use those services' deletion
processes separately.

## Your choices and security

- Choose endpoints, providers, agents, and ACP clients whose privacy practices
  meet your needs before submitting sensitive information.
- Review prompts, screenshots, guidance, skills, and tool-accessible material.
  Keep credentials and private history out of version control and shared logs.
- Disable optional provider-hosted search, exact token counting, or remote
  compaction as appropriate in your provider configuration. Disabling supporting
  features does not make ordinary model requests local or offline.
- Use HTTPS/WSS endpoints for encrypted transport. Zevria also accepts HTTP/WS
  endpoints, so encrypted transport is not enforced for every configuration.
- Use an appropriately restricted OS account, container, VM, or sandbox when
  filesystem, credential, or network isolation is needed.
- To stop new local work, cancel active tasks, exit Zevria, and stop external
  agents or tool processes. This cannot recall information already sent or ensure
  that a remote service stops processing it.

## Installation and third-party hosting

The repository's installer scripts contact GitHub to resolve a selected release
and download release archives and checksums. Installing through a package manager,
building from source, or launching package-based agent adapters can also contact
the relevant hosting services or package registries. Those services receive
ordinary download/request metadata under their own privacy policies. This is
separate from Zevria's model requests and local diagnostics.

## Questions and policy updates

For questions about this policy or Zevria's own data handling, contact the
maintainers through the
[Zevria issue tracker](https://github.com/liukaizheng/zevria/issues). GitHub issues
are public: do not include API keys, confidential code, sensitive personal
information, or unredacted logs and histories. If confidential handling is needed,
ask for a private contact method before sharing details.

Changes to Zevria's data handling will be reflected in updates to this file and
its last-updated date. For information already sent to a third-party service,
contact that service about its privacy and retention practices.
