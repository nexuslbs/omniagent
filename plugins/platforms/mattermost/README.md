# Mattermost Platform Plugin

A standalone Rust binary that implements the OmniAgent platform plugin protocol for [Mattermost](https://mattermost.com/). It communicates with the Mattermost REST API and WebSocket to send, edit, delete, and receive messages.

## Plugin Architecture

The Mattermost plugin is a daemon process that communicates with the OmniAgent core over **stdio JSON-RPC**. One JSON object per line on stdin/stdout.

### Protocol Flow

```
initialize → configure → (normal operation) OR (setup → exit)
```

#### 1. `initialize`
OmniAgent sends an `initialize` request. The plugin responds with its name, version, type, and capabilities.

#### 2. `configure`
OmniAgent sends all resolved config values as a JSON-RPC params object. The plugin deserializes them into `PluginConfig` (with serde defaults for missing fields).

#### Normal Operation (after configure)

After configure, the plugin:

- **Authenticates** using the provided `access_token` (or auto-recovers via admin credentials).
- **Discovers channels** the bot is a member of and merges any explicitly configured `channel_ids`.
- **Starts inbound processing**: either a WebSocket event loop (`connection_mode: "websocket"`) or a polling loop (`connection_mode: "polling"` and `polling_enabled: true`).
- **Enters the request-response loop** on stdin, handling these methods:

| Method | Purpose |
|---|---|
| `deliver` | Send a message to a Mattermost channel |
| `edit_message` | Edit an existing post |
| `delete_message` | Delete a post |
| `react` | Add an emoji reaction to a post |

Inbound messages (new posts detected via polling or WebSocket) are sent as `inbound_message` notifications to stdout.

#### Setup Mode (optional)

If the `setup` capability is requested:
```
initialize → configure → setup → exit
```

The `setup` method creates a team, channel, bot user, and personal access token idempotently. Configuration for setup is provided via `SetupParams` (sent as the params of the `setup` JSON-RPC call).

## Config Fields

All fields are optional in `plugin.json` and have sensible defaults via `serde(default)`. The OmniAgent core resolves them and sends the full set to the plugin.

| Field | Type | Default | Description |
|---|---|---|---|
| `server_url` | string | `"http://mattermost:8065"` | Base URL of the Mattermost server |
| `access_token` | string (secret) | N/A | Personal access token for the bot account |
| `connection_mode` | enum | `"websocket"` | `"polling"` or `"websocket"` |
| `polling_enabled` | boolean | `true` | Whether polling is active (if mode is `"polling"`) |
| `polling_interval` | integer | `15` | Seconds between polls (min 5, max 300) |
| `channel_ids` | string | `""` | Comma-separated channel IDs to watch in addition to auto-discovered ones |
| `setup_team` | string | `""` | Team name for setup mode |
| `setup_channel` | string | `"setup"` | Channel name for setup mode |
| `bot_username` | string | `"omniagent"` | Bot account username |
| `bot_user` | string | `"omniagent"` | Bot username for setup |
| `bot_password` | string (secret) | N/A | Bot password for setup |
| `admin_user` | string | N/A | Admin username for auto-recovery |
| `admin_password` | string (secret) | N/A | Admin password for auto-recovery |
| `test_user` | string | `""` | Test user to create during setup |
| `test_password` | string (secret) | N/A | Test user password for setup |
| `env_path` | string | `"/opt/omni/.env"` | Path to `.env` file for token persistence |

## Inbound Message Detection

### WebSocket Mode (default)
Connects to `wss://<server>/api/v4/websocket` and listens for `posted` events. On each event, triggers a cursor-based poll of that specific channel to catch any missed messages. Reconnects with exponential backoff on disconnect.

### Polling Mode
Periodically polls all watched channels for new posts using `GET /api/v4/channels/<id>/posts`. Channel list is auto-discovered every 4 polling cycles and merged with `channel_ids` from config (comma-separated list).

## Communication Protocol

All messages are JSON-Lines (one JSON object per line) over stdin/stdout. The plugin never reads or writes files for IPC.

### Request Format (from OmniAgent to plugin)
```json
{"id": 1, "method": "initialize", "params": {}}
```

### Response Format (from plugin to OmniAgent)
```json
{"id": 1, "result": {"name": "mattermost", "version": "2.0.0", ...}}
```

### Error Format
```json
{"id": 1, "error": {"code": -1, "message": "description"}}
```

### Inbound Notification (plugin to OmniAgent)
```json
{"method": "inbound_message", "params": {"channel_id": "...", "text": "...", ...}}
```

## Setup Mode

When OmniAgent calls `setup`, the plugin:

1. Validates that `setup_team`, `setup_channel`, and `bot_user` are non-empty.
2. Authenticates with the provided `access_token`.
3. Creates or finds the team.
4. Creates or finds the channel.
5. Creates the bot user if needed (or uses existing).
6. Creates a personal access token for the bot.
7. Returns `team_id`, `channel_id`, `bot_token`, and bot user info.

The plugin then exits (setup mode is a one-shot operation).

## Building

```bash
cd /opt/workspace/omni-stack/plugins/platforms/mattermost
cargo build --release
```

The binary is at `target/release/mattermost-platform`.
