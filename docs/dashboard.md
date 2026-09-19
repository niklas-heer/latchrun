# Local dashboard

Start the service, then run the dashboard in a terminal:

```sh
latchrun service start
latchrun dashboard serve
```

Open the private URL from its single JSON startup line in your browser. By default,
the operating system chooses an available port. For a fixed port, use
`latchrun dashboard serve --port 9187`. Pass `--runtime-dir PATH` before
`dashboard` to inspect a different local service. Ctrl-C or SIGTERM stops the
dashboard without stopping the service or its sessions.

The dashboard shows active and historical sessions, operation IDs, status,
duration, exit status, service statistics, access decisions, policy denials and
environment provenance. Select a session to switch between Commands,
Environment and Access. Refresh access clears the session's credential cache;
the next command resolves credentials again. Stop session denies new execution
and terminates active commands. Both actions require an explicit browser
confirmation. Neither action recalls a credential already delivered to a child
or revokes a credential at its remote provider.

The usage-history section provides 24-hour and 7/30/90-calendar-day views, outcome trends, p50/p95/p99 command latency, credential-cache hit rates, and agent/tool activity. Charts have legends and an expandable exact-data table. UTC periods include the current partial period; exact boundaries are shown. These statistics persist in SQLite independently of session-history pruning. Agent rows distinguish adapter-observed MCP calls from explicitly reported external activity; neither implies visibility into every tool on the machine. See [analytics storage and definitions](analytics.md).

The environment inspector shows declared names, sources, presence rules,
precedence and expiry. It does not resolve credentials. Only values explicitly
classified for exposure by the service's environment policy are visible; secret
values and provider references are unavailable. Command arguments, project paths,
purpose, raw output and full profiles are also unavailable through the dashboard.
The session access timeline uses the service's bounded history; usage analytics retains its separate local metadata.

## Browser authorization

Each dashboard process generates a new 256-bit bearer capability. Treat the
startup link as access to this local session inspector and its stop/refresh
controls. The capability is not a credential-provider secret. It exists only in
process memory, the startup terminal and the authorized browser tab. The URL
fragment is never sent in an HTTP request. The page removes it from the address
bar immediately and retains authorization in tab-scoped `sessionStorage` so
reload works. If browser storage is unavailable, authorization lasts until the
page reloads. Closing the tab clears its stored authorization; restarting the
dashboard invalidates every previous capability.

The listener binds only IPv4 `127.0.0.1`. It requires the exact assigned numeric
host and port, rejects foreign browser origins and cross-site requests, and
requires the bearer header before serving metadata. Mutation endpoints require
POST, an exact same-origin Origin header and `application/json`. There is no
cookie authentication, URL-query authentication or permissive CORS. Do not put
the listener behind a proxy or publish it remotely. This is a local-user trust
boundary: hostile processes running as the same user are not isolated.

Responses disable caching, referring-page disclosure, MIME sniffing and framing.
A restrictive nonce-based Content Security Policy blocks external and injected
scripts. UI metadata uses DOM text nodes rather than HTML interpretation. The
HTTP parser permits one request per connection, rejects duplicate headers and
transfer encodings, limits headers to 16 KiB and bodies to 4 KiB, enforces a
three-second total read deadline, and bounds concurrent clients to 32. Excess
connections are closed. No access logs or dashboard tokens are written to files.
The dashboard is an inspection/control interface, not a generic execution proxy.

## API surface

API callers must send `Authorization: Bearer <capability>` and the correct Host.
Browser callers must use the exact same origin. No route accepts a raw secret.

| Method | Route | Result |
| --- | --- | --- |
| GET | `/` | Public UI shell without service metadata |
| GET | `/api/status` | Service statistics and session/operation metadata |
| GET | `/api/events` | Bounded service activity metadata |
| GET | `/api/analytics?days=7` | Durable usage analytics; days must be 1, 7, 30 or 90 |
| GET | `/api/inspect?session=ID` | Secret-safe environment and session metadata |
| POST | `/api/session/refresh` | Invalidate cached credentials for the session |
| POST | `/api/session/stop` | Stop the session and active commands |

POST bodies contain exactly `{"session":"ID"}`. Session names can also be used.
Errors use fixed or service-controlled diagnostics and never echo malformed
request bodies. Inspection does not replay commands, and the dashboard never
requests raw command output.
