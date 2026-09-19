+++
schema_version = 1
id = "01M2XHZ6GKKP68FNVZSAEZPDBX"
title = "Durable usage analytics"
date = "2026-09-19"
status = "accepted"
tags = ["tooling", "ai"]
supersedes = []
superseded_by = []
depends_on = ["01M2XHZ6GDSMGWDK3D5DE9KC26"]
related_to = []
+++
Status: Accepted under the owner's delegated analytics implementation task; implemented
Extends: [0004: Full runtime and integrations](2026-09-19_192325581_full-runtime-and-integrations.md)

## Context and decision

The recovery journal deliberately bounds session/operation detail and prunes old records while preserving command IDs. That is useful for safe recovery, but cannot provide durable usage trends or distinguish agent-tool activity outside individual command execution. The owner requested persistent analytics and agent attribution. Preserve the journal's recovery contract and add a separate metadata-only SQLite store instead of retaining command output or expanding recovery snapshots indefinitely.

Use rusqlite with bundled SQLite for a reproducible local database, with private same-user filesystem ownership and permissions. Default to `$XDG_CONFIG_HOME/latchrun` when set to an absolute path, otherwise the OS account's `~/.config/latchrun`. Allow explicit `--data-dir`/`LATCHRUN_DATA_DIR`. An explicit runtime doubles as the data directory unless overridden, keeping isolated tests/custom runtimes self-contained. The daemon's startup choice owns the database location; clients access analytics through its private socket. Add the data directory to automatic sandbox protection. Hold a private lifetime file lock so only one daemon can own a data directory, independently of runtime-socket locks. Lock files must not be removed or replaced while running.

Store accepted operation metadata, outcomes, duration, cache hit/miss/unknown, provider kind and CLI/MCP/unknown origin; record service denials and explicit tool-activity metadata separately. Do not store argv, output, project/purpose, profiles, provider references, credential values, tool arguments or tool results. Names and labels remain visible metadata and must be non-sensitive.

Retain analytics rows indefinitely across service restart and recovery-detail pruning. Mark stale reserved/running database rows unknown on open, then reconcile still-available journal operations after restart or upgrade. This also handles loss of a temporary runtime journal without leaving permanent running counts. Missing historical origin/cache/provider fields remain unknown, and previously pruned operations or unrecorded denials/tool calls cannot be reconstructed. There is no claim of complete historical coverage before the feature was enabled.

Provide 1/7/30/90-day views through `stats`, the dashboard and an MCP analytics tool. Bucket in UTC; use hourly buckets for one day and daily buckets for longer windows, including the current partial bucket. Attribute command outcomes/latency to operation start time. Compute exact nearest-rank latency percentiles rather than retaining approximate sketches; null marks absent samples. Only succeeded/failed operation durations enter latency percentiles; unknown outcomes are excluded even when they have partial durations. Cache hit rate excludes unknown cache states. Limit returned agent/tool groups to 100 and flag truncation while preserving full activity totals.

Instrument advertised MCP tool calls after completion. Use a generated report ID, a bounded validated clientInfo name or fallback `mcp`, the fixed advertised tool name, elapsed duration, success/failure and source `mcp`. Nonzero command exits count as failed calls. Unknown tool names and argument contents are not recorded. Preserve the original tool outcome if post-call telemetry recording fails, with a fixed warning that does not invite command replay.

Expose an explicit `activity record` CLI for other integrations, accepting only ID, agent/tool labels, duration and success/error. Mark these reports `external`; they are self-reported, not independent observations or command authorization. Identical report IDs and payloads deduplicate without moving their timestamp. Conflicting payloads under a used ID fail. This idempotency is for metadata delivery, never permission to replay commands. Same-user clients remain trusted; provenance labels do not authenticate them.

## Failure and privacy consequences

Command admission must fail closed if required persistence fails. Once work has executed, a failed final persistence operation may produce an unknown outcome and must not cause an automatic retry. External report failure returns an error without stopping unrelated work; MCP telemetry failure leaves its original command/tool result intact. SQLite and the journal do not form a distributed transaction with external side effects.

The separate store preserves useful history without weakening bounded recovery or writing secrets. It introduces a native database dependency, indefinite local metadata growth and maintenance/backup considerations. Query windows limit displayed history, not stored history. Deleting the database discards analytics and external-report deduplication and is not an automated recovery/retention procedure.

Command totals and tool-call totals count different events. One MCP command can contribute to both; they must not be summed as unique work. External instrumentation expands coverage only where an integration reports it. Latchrun does not read private agent logs or automatically observe every unrelated tool. Benchmark database/query overhead before making performance claims.

## Verification

Public CLI regressions cover command/cache/denial counts surviving history pruning and restart; external-report deduplication and conflicts; MCP analytics, failed-command attribution, safe client labels and suppression of unknown tool names; and preserving a completed operation when its telemetry write fails. Storage-path tests cover defaults, overrides, background-service propagation and sandbox protection. Backend tests cover windowing, percentile calculations, persistence validation and retention behavior. On 2026-09-19, native macOS `mise run check` and `mise run build` and Linux Dagger `mise run ci` passed with 77 tests on each platform. This includes ten deterministic analytics tests, five public analytics tests and three storage-path tests. Desktop visual inspection verified populated charts and agent activity using fake providers; latest mobile/range interaction checks were limited by the unavailable isolated browser and concurrent user browser activity. Both platforms also passed formatting and strict Clippy; the SQLite release build requires no separate database installation.

The operator contract is documented in [analytics.md](../analytics.md), [agent integration](../agent-integration.md) and [README](../../README.md). Recheck filesystem/SQLite compatibility and query cost when changing schema, storage location or retention policy. Restart an older daemon and its dashboard/MCP clients for the new protocol: idempotent service start and an unchanged package version do not establish that a resident daemon has upgraded.
