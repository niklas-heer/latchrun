# Agent integration

Latchrun exposes a local stdio MCP server:

```sh
latchrun --runtime-dir /absolute/private/runtime agent serve
```

It implements protocol version **2025-11-25**, with `initialize`, `ping`, `tools/list` and `tools/call`. Messages are one JSON-RPC object per line; stdout contains protocol messages only. This follows the MCP [stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports). There is no HTTP transport, resource/prompt server, sampling or task API.

## Operator setup

Start the service and create the allowed session before connecting the agent:

```sh
latchrun service start
latchrun session start demo --profile examples/fake.json
latchrun inspect demo
```

For a custom runtime, use the same `--runtime-dir` in every command and the agent configuration. Its parent directory must exist; Latchrun creates/checks the private runtime itself. After service restart, explicitly resume an interrupted session with `session resume demo --profile examples/fake.json` before asking the agent to execute more work.

Configure your MCP client to launch the absolute Latchrun executable with `agent serve`. Many clients use a configuration shaped like this; the enclosing configuration key and file location are client-specific:

```json
{
  "mcpServers": {
    "latchrun": {
      "command": "/absolute/path/to/latchrun",
      "args": ["--runtime-dir", "/absolute/private/runtime", "agent", "serve"]
    }
  }
}
```

Do not put provider tokens or other secrets in the client configuration. The operator's profile contains provider references, and the guardian resolves the values independently of the agent. The adapter has no tool to create/resume sessions or change profiles.

## Tools

The adapter advertises JSON schemas through the MCP [tools interface](https://modelcontextprotocol.io/specification/2025-11-25/server/tools). Unknown arguments are rejected. Session names/IDs and operation IDs use 1–64 ASCII letters, digits, underscores or hyphens.

| Tool | Arguments | Result |
| --- | --- | --- |
| `latchrun_status` | Optional `session` | Session/operation outcomes |
| `latchrun_events` | Optional `session` | Bounded activity metadata |
| `latchrun_inspect` | Required `session` | Environment declarations, provenance and allowed non-secret values; no credential lookup |
| `latchrun_stop` | Required `session` | Stop the session and active commands |
| `latchrun_refresh` | Required `session` | Invalidate cached credentials for future commands |
| `latchrun_run` | Required `session`, `operation`, `argv` | Execute one exact approved command with null stdin |
| `latchrun_analytics` | Optional `days`: 1, 7, 30 or 90; default 7 | Durable aggregate usage and agent/tool activity |

A fake run request after initialization looks like this:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"latchrun_run","arguments":{"session":"demo","operation":"agent-demo-1","argv":["/usr/bin/printenv","TEST_SECRET"]}}}
```

The tool result supplies both text content and `structuredContent` containing `operation`, `exit_code`, `stdout`, `stderr` and `truncated`. Output has already passed through secret redaction. Each stream retains at most 32 KiB for the response; excess bytes are drained and discarded. Binary output is converted with UTF-8 replacement. A command's nonzero exit appears in `exit_code`; tool/protocol/provider failures set `isError: true` instead.

Each advertised tool call records safe usage metadata after completion. A valid `clientInfo.name` supplied at initialization identifies the client in analytics; otherwise its label is `mcp`. Labels follow the same bounded identifier syntax and do not authenticate the client. Names are fixed advertised labels; arguments/results are never recorded. Nonzero command exits count as failed calls. If recording fails, the original outcome is preserved and a fixed warning appears in text content and `_meta.telemetry_warning`; do not repeat the command to recover telemetry. Unknown tool names are not recorded. See [analytics and external reports](analytics.md).

There is no live output streaming, stdin, resize or TTY tool. Use the CLI's `--stdin`/`--tty` modes for interactive commands. The adapter has no `--shell` convenience tool; an approved shell can only be invoked through the same exact argv rule as another command.

## Instructions for an agent

A short integration instruction can be:

> Use only the operator-designated Latchrun session and its approved commands. Supply a new operation ID for each deliberate execution. Never request credential values or place secrets in arguments. If a response is lost, inspect session status before doing anything else; do not retry automatically. Report unknown outcomes for reconciliation. Stop or refresh access only when the task authorizes that control action.

An operation ID is reserved durably before command launch and cannot be reused, including after pruning or service restart. `latchrun_status` and `session reconnect` inspect an outcome; they do not replay the command or old output. If an outcome is unknown, inspect the external effect and obtain whatever authorization the task requires before deliberately submitting a new operation ID. The journal is not a transaction with a remote API.

The adapter processes requests serially. While one run is active, use another adapter process, the CLI or dashboard for status/stop. MCP cancellation notifications are not implemented. Losing the adapter/client connection does not cancel an accepted operation; its normal deadlines and session controls remain active.

## Scope of protection

The tools expose pre-created sessions, bounded metadata and redacted results. The agent receives no provider reference or credential endpoint. Dashboard and CLI inspection share the same safe metadata rules; only explicitly exposed non-secret environment values are visible.

The OS user and profile author are trusted. An agent with independent unrestricted shell access can invoke the service CLI or socket itself, so this adapter is not whole-agent confinement. Optional [sandboxing](sandbox.md) restricts approved children. It cannot prevent a child from disclosing an injected credential through encoded output or permitted files/network access, and it does not reduce the credential's remote permissions.
