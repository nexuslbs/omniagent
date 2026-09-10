# OmniAgent HTTP API reference

Canonical API reference for the omniagent core HTTP server (binds
`localhost:8080` inside the container; no auth on localhost).

This file is **curated from `src/server/*.rs` routers** (see "Source of
truth" below) and is copied into the release image at build time:

- `/opt/omni/docs/api.md` (canonical)
- `/app/docs/api.md` (fallback - survives a fresh `/opt/omni` volume mount)

Inside a running container, re-derive with:

```sh
docker exec <container> cat /opt/omni/docs/api.md
docker exec <container> ls /opt/omni/docs
```

## Calling conventions

- Base URL: `http://localhost:8080` (internal only).
- From inside the agent: use the `builtin_omniagent-api` tool with
  `method` + `path` (+ optional `body`) - no host/scheme/port needed.
- From a shell: `docker exec <container> curl -s http://localhost:8080/kanban/tasks`
- All JSON bodies are `application/json`. Responses are
  `{"success":true,"data":...}` or `{"success":false,"error":...}`.

## Core / control

| Method + path | Purpose |
|---|---|
| GET /health | liveness |
| POST/GET /stop/{channel_id} | stop a channel |
| POST /stop-thread/{thread_id} | stop a thread |
| POST/GET /close/{channel_id} | close a channel |
| POST/GET /open/{channel_id} | open a channel |
| GET /status/{channel_id} | channel status |
| GET /prompt/{channel_name} | built prompt for a channel |
| GET /api/context/{channel_name} | context preview |
| POST /api/reload | reload plugin env |
| POST /api/llm/chat | LLM proxy chat |
| GET /api/plugins/ping | "pong" |
| GET /api/plugins/check-state | plugin state diagnostic |
| GET /api/plugins/check-db | DB diagnostic |
| GET /api/plugins/check-env | env diagnostic |

## MCP

| Method + path | Purpose |
|---|---|
| GET /mcp/tools | list registered tools (builtin + plugins) |
| POST /mcp/execute | execute a tool: `{"name":"...","arguments":{...}}` |

## Kanban

| Method + path | Body | Purpose |
|---|---|---|
| GET /kanban/tasks | - | board tasks (flat list) |
| GET /kanban/tasks/{id} | - | task detail |
| POST /kanban/tasks | `{"title","status","board","profile","channel","priority",...}` | create task |
| PATCH /kanban/tasks/{id} | partial fields | update task |
| PATCH /kanban/tasks/{id}/status | `{"status":"running"}` | change status (+ position shift) |
| PATCH /kanban/tasks/{id}/position | `{"position":N}` | change position (cross-column) |
| DELETE /kanban/tasks/{id} | - | delete task |
| GET /kanban/tasks/{id}/dependencies | - | list dependencies |
| POST /kanban/tasks/{id}/dependencies | `{"depends_on_id":N}` | add dependency |
| DELETE /kanban/tasks/{id}/dependencies/{depId} | - | remove dependency |
| GET /kanban/tasks/{id}/threads | - | threads for a task |
| GET /kanban/tasks/{id}/history | - | history log |
| GET /kanban/history | - | all history (task_id query param) |
| GET /kanban/tasks/{id}/subtasks | - | task subtasks |
| POST /kanban/tasks/{id}/workflow/executions/reset | - | reset workflow executions |
| POST /kanban/dispatch | - | dispatch highest-priority eligible todo task |
| POST /kanban/tasks/{id}/redispatch | - | re-create thread for a task |
| POST /review | `{"task_id","decision","comment"}` | review decision |
| GET /workflows | - | list workflows |
| PUT/POST/DELETE /workflows/{key} | workflow def | upsert / delete workflow |
| GET /boards | - | list boards |
| POST /boards | board def | create board |
| DELETE /boards/{key} | - | delete board |

Common create-task body:

```json
{"title": "Fix login bug", "status": "todo", "board": "default",
 "profile": "omni", "channel": "kanban", "priority": 10}
```

## Schedule / cron

| Method + path | Body | Purpose |
|---|---|---|
| GET /schedule | - | list schedules |
| GET /schedule/{id} | - | schedule detail |
| POST /schedule | `{"id","enabled","channel","profile","cron","prompt","plan",...}` | create |
| PATCH /schedule/{id} | partial fields | update |
| PATCH /schedule/{id}/toggle | - | toggle enabled |
| POST /schedule/{id}/run | - | trigger now |
| DELETE /schedule/{id} | - | delete (removes from tasks.yml) |
| GET /schedule/{id}/threads | - | schedule threads |
| GET /schedule/{id}/subtasks | - | schedule subtasks |
| POST /run-cron/{schedule_id} | - | legacy manual fire |

Create-schedule body:

```json
{"id": "nightly_cleanup", "enabled": true, "channel": "cron",
 "profile": "omni", "cron": "0 3 * * *", "prompt": "Run nightly cleanup"}
```

## Plugins (prefix: `/api/plugins` - the builtin tool needs the full path)

| Method + path | Purpose |
|---|---|
| GET /api/plugins | list all plugins |
| GET /api/plugins/{type}/{source}/{name} | plugin detail |
| POST /api/plugins/{type}/{source}/{name}/enable | enable |
| POST /api/plugins/{type}/{source}/{name}/disable | disable |
| POST /api/plugins/{type}/{source}/{name}/restart | restart |
| POST /api/plugins/{type}/{source}/{name}/config | update config: `{"config":{"allow_unsafe_methods":"true"}}` |
| POST /api/plugins/{type}/{source}/{name}/install | install |
| POST /api/plugins/{type}/{source}/{name}/reinstall | reinstall |
| POST /api/plugins/{type}/{source}/{name}/setup | setup |
| POST /api/plugins/{type}/{source}/{name}/download | download |
| POST /api/plugins/{type}/{source}/{name}/rename | rename |
| DELETE /api/plugins/{type}/{source}/{name} | delete |
| POST /api/plugins/install-git | git-install a remote plugin |
| POST /api/plugins/install-url | url-install a remote plugin |

`{type}` ∈ `tools` | `providers` | `platforms`; `{source}` ∈ `built-in` |
`bundled` | `remote` | `invalid`. Enable example:

```sh
curl -s -X POST http://localhost:8080/api/plugins/tools/bundled/fetch/enable
```

## Actions

| Method + path | Purpose |
|---|---|
| GET /actions | list actions |
| POST /actions | create action |
| PUT /actions/{id} | update action |
| DELETE /actions/{id} | delete action |
| POST /actions/{id}/run | run action |

## Hooks

| Method + path | Purpose |
|---|---|
| GET /hooks | list hooks |
| POST /hooks | create hook |
| GET /hooks/{id} | hook detail |
| PATCH /hooks/{id} | update hook |
| PATCH /hooks/{id}/toggle | toggle hook |
| POST /hooks/{id}/fire | fire hook |
| DELETE /hooks/{id} | delete hook |

## Channels

| Method + path | Purpose |
|---|---|
| GET /channels | list channels |
| GET /channels/{id} | channel detail |
| PATCH /channels/{id} | update channel (e.g. provider/model/profile) |
| GET /channels/all | all channels (platforms router) |
| GET /platforms | list platforms |
| GET /platforms/{name}/channels | platform channels |

## Settings

| Method + path | Purpose |
|---|---|
| GET /settings | current settings (max_tokens, budgets, ...) |
| PUT /settings | update settings (partial) |

## Memory / messages / threads / overview / secrets

| Method + path | Purpose |
|---|---|
| GET /memory/stats | memory stats |
| GET /memory/search?q=... | search memory |
| GET /memory/text/{profile}/{type} | memory text file |
| POST /memory/upload/{profile}/{type} | upload memory |
| POST /memory/edit/{profile}/{type} | edit memory |
| GET /messages/filters | message filters |
| GET /messages/events | message events |
| GET /threads | list threads |
| GET /threads/filters | thread filters |
| GET /threads/{id}/subtasks | thread subtasks |
| GET /overview | overview |
| GET /overview/dashboard | dashboard overview |
| GET /secrets | list secrets |
| POST /secrets | create secret |
| GET/PUT /secrets/{name} | get / update secret |
| GET /secrets/{name}/versions | secret versions |
| DELETE /secrets/{name} | delete secret |

## Source of truth

Curated from the axum routers in `src/server/`:
`mod.rs`, `kanban.rs`, `schedule.rs`, `plugins.rs`, `actions.rs`
(in `mod.rs`), `hooks.rs`, `channels.rs`, `settings.rs`, `memory.rs`,
`messages.rs`, `threads.rs`, `platforms.rs`, `overview.rs`, `secrets.rs`.
To regenerate/refresh after a code change, re-derive the route list with:

```sh
grep -rn '\.route(' src/server/ | sed "s/.*\.route(//; s/,.*//" | sort -u
```

Repo: https://github.com/nexuslbs/omniagent (not shipped in the image -
`git clone https://github.com/nexuslbs/omniagent /opt/workspace/omniagent-src`
to read the source).

## Internal latency budget (server-side < 500 ms)

Every route in this reference must serve within **500 ms of internal
server-side handling time**: DB query + business logic + serialization inside
the omniagent process. Client connection and internet latency are out of
scope; the budget is measured network-free. Two rules follow from it:

1. **Independent work runs concurrently.** Sequential `await` of independent
   queries/HTTP calls/file reads inside one handler is a bug: use
   `tokio::join!` / `tokio::try_join!` so the handler pays `max`, not `sum`.
   The same rule applies in the dashboard: a page that needs several
   independent endpoints must issue them in a single concurrent fan-out
   (`allSettledOrNull` in `src/lib/parallel.ts`), not one after another.
2. **List queries must not scale with the table.** The `LIMIT n` page is
   materialized first (CTE, ordered by the primary key) and any per-row
   decoration (last message, per-thread counts, reverse lookups) is applied
   with `LEFT JOIN LATERAL` to that page only, backed by an index; the row
   count for pagination runs concurrently with the page query.

Only genuinely dependent calls stay sequential (`B` needs `A`'s result).

### Measuring it

Every response carries the `x-response-time-ms` header: the time spent inside
the omniagent process (network-free), which is the number the budget applies
to. Requests over 500 ms additionally emit a `slow request` warning log.

### Latency inventory

Measured in the omnidev stack against a production-like volume (3.2k threads /
234k messages / 553 MB), server-side ms from `x-response-time-ms`, p50/p95/max
over repeated calls. All 29 audited endpoints are inside the budget:

| endpoint | p50 | p95 | max |
|---|---|---|---|
| /memory/stats | 78 | 102 | 102 |
| /messages/filters | 54 | 58 | 58 |
| /messages/events?limit=50&offset=0 | 34 | 36 | 36 |
| /messages/events?limit=50&offset=500 | 33 | 35 | 35 |
| /overview | 3 | 26 | 26 |
| /plugins | 14 | 17 | 17 |
| /threads?limit=50&offset=50 | 2 | 12 | 12 |
| /overview/dashboard | 10 | 11 | 11 |
| /threads?limit=50&offset=500 | 2 | 10 | 10 |
| /threads?limit=50&offset=1500 | 2 | 7 | 7 |
| /threads?limit=50&offset=0 | 2 | 6 | 6 |
| /settings | 6 | 6 | 6 |
| /kanban/tasks | 1 | 3 | 3 |
| /channels | 2 | 3 | 3 |
| /threads/filters | 2 | 2 | 2 |
| /channels/all | 2 | 2 | 2 |
| /platforms | 2 | 2 | 2 |
| /hooks | 0 | 1 | 1 |
| /secrets | 0 | 1 | 1 |
| /health, /kanban/history, /kanban/tags, /boards, /workflows, /profiles, /actions, /schedule, /prompt/default, /mcp/tools | 0 | 0 | 0 |

The dashboard calls these as `/api/<path>`; the path without the `/api`
prefix is the internal route listed here.
