# Durable usage analytics

Latchrun keeps usage metadata in a local SQLite database, separate from the bounded recovery journal. Query it from the CLI:

```sh
latchrun stats
latchrun stats --days 1
latchrun stats --days 30
latchrun data path
```

The default window is seven days; supported windows are 1, 7, 30 and 90 days. One day displays 24 UTC-aligned hourly buckets. Longer windows display daily UTC buckets including the current partial bucket. Command outcomes and latency use the operation's start-time cohort, so completing an operation updates the bucket in which it began.

## Measures and interpretation

| Measure | Meaning |
| --- | --- |
| Accepted and outcomes | Distinct accepted operations, grouped as succeeded, failed, unknown or still running |
| Denied | Policy/access denials recorded by the service; these are not accepted operations |
| Sessions | Sessions represented by accepted operations in the selected window |
| Latency p50/p95/p99 | Exact nearest-rank percentiles of succeeded/failed operation durations, including provider lookup; unknown/unfinished outcomes are excluded |
| Cache hits/misses | Whether an operation reused cached credentials or needed a fresh resolution |
| Cache hit rate | Hits divided by hits plus misses; null when no eligible samples exist |
| Agent/tool calls and errors | Automatically observed MCP calls and explicit external reports, grouped separately by agent, tool and source |

Latency percentiles are null without duration samples. Unknown outcomes are excluded even when a partial duration is available. Unfinished operations do not acquire an invented duration. Operations with caching disabled or no credential declarations do not contribute cache hit/miss samples. A cache miss measures the lookup path, not a successful password-manager unlock. Historical entries without cache/provider/origin metadata remain unknown rather than being inferred. Command totals and agent/tool calls measure different events: an MCP execution can contribute one command and one MCP tool call. Do not add them together as a count of unique work.

The JSON response contains `totals`, `latency`, `cache`, `timeline`, `agents` and `activity`, plus window boundaries. `agents` returns at most 100 groups and `agents_truncated` identifies omitted groups; `activity` retains the full call/error totals for the selected window. The dashboard's analytics view reads the same service response. Statistics do not contain command lines, tool arguments/results, output, project paths or credential references.

## Storage and lifecycle

The default data directory is `$XDG_CONFIG_HOME/latchrun` when XDG_CONFIG_HOME is set, otherwise the OS account's `~/.config/latchrun`. A set XDG_CONFIG_HOME must be absolute; an invalid relative value is rejected. Select another with `--data-dir PATH` before the command or `LATCHRUN_DATA_DIR`. For isolated/custom runtimes, an explicit `--runtime-dir` or `LATCHRUN_RUNTIME_DIR` also becomes the data directory unless a data override is supplied. `data path` prints the selected directory and `analytics.sqlite3` path without needing the service.

```sh
latchrun --runtime-dir /absolute/private/runtime --data-dir /absolute/private/data service start
latchrun --runtime-dir /absolute/private/runtime stats --days 7
```

The service owns the database location chosen at startup. Other clients query it through the private service socket; a client's data-path option does not move an already running service's database. Directories and database files must be private to the current OS user. A lifetime file lock permits only one active daemon per data directory, even when the daemons use different runtime sockets. Give concurrent independent services separate data directories. Do not delete or replace analytics lock files while a service is running. The sandbox protects the service's data directory as well as its runtime, even when a profile grants a broader parent directory.

SQLite retains metadata rows indefinitely. `history prune` only removes bounded recovery details, never analytics or its deduplication records. Restart preserves analytics. On database open, stale reserved/running rows become unknown before the available recovery journal is reconciled. This prevents permanent running counts when a temporary runtime journal was lost, for example after reboot; it cannot reconstruct a missing final outcome. Journaled operations still available when upgrading are reconciled into the database; previously pruned details, historical denials, cache outcomes and external tool activity cannot be reconstructed. Recording begins when the supporting implementation is used.

SQLite analytics and the journal serve different purposes: the journal protects command identity/recovery; analytics preserves usage history. Neither contains credentials, cached values, raw output, full profiles or provider references. Names/IDs and external labels are visible metadata, so they must never contain secrets. Removing the analytics database discards usage history and external-report deduplication; it is not a retention control or a recovery step to automate.

A command-admission persistence failure denies execution. A failure to persist an already executed command's final state can produce an unknown outcome; reconcile the command before doing anything else, and never rerun it to repair metrics. An external activity-recording failure returns an error without stopping unrelated work. The MCP adapter preserves the original tool result and adds a fixed telemetry warning if its post-call recording fails.

## Upgrading a running service

Stop the old service before replacing/restarting it with the new binary, preserving the runtime journal and selected data directory. `service start` is idempotent and will keep an already running older daemon; it does not upgrade its protocol or enable analytics in place. Restart the dashboard and MCP adapter too. The package version may remain unchanged between repository builds, so a matching `--version` alone does not prove the daemon implements the new analytics requests. Resume interrupted/stopped sessions explicitly with their profiles; no work is replayed.

## Agent activity

The stdio adapter automatically records each advertised MCP tool call after completion: a generated ID, the validated client label, advertised tool name, elapsed duration, success/failure and source `mcp`. The agent label comes from `initialize.params.clientInfo.name` only when it contains 1–64 ASCII letters, digits, underscores or hyphens; missing or invalid names become `mcp`. Labels are caller-supplied metadata, not authenticated identities. A command with a nonzero exit counts as a failed call even though the MCP tool response itself successfully conveys that exit code. Unknown tool names are not recorded, and argument/result contents never enter analytics. An analytics query records its own call after taking the snapshot, so that call first appears in the next query.

Other agents or integrations can explicitly report activity:

```sh
latchrun activity record --id editor-read-1 --agent editor --tool read_file --duration-ms 125 --outcome success
```

This accepts metadata only; there is no field for arguments, results, file paths or output. IDs and labels use 1–64 ASCII letters, digits, underscores or hyphens; duration is 0–86400000 milliseconds. Keep labels stable and non-sensitive. External reports have source `external` and are self-reported, not proof that Latchrun observed or authorized the underlying operation. Same-user callers are trusted; provenance labels are not an authenticity boundary against them.

An identical repeated report ID and payload succeeds without counting twice or moving its original timestamp. Reusing an ID with different metadata fails with `activity_conflict`. Preserve an event's ID/payload when reconciling a lost reporting response; a new ID describes a new event. This idempotency applies to metadata reports only: command operation IDs still reject duplicate execution and must never be replayed automatically.

Only mediated commands, adapter-observed calls and explicitly reported external events are visible. Latchrun does not inspect an agent's private logs or automatically observe every tool on the machine. See [agent setup](agent-integration.md) and [dashboard authorization](dashboard.md).
