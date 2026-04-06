# mcp-ssh-stateless

A stateless SSH MCP server for [goose](https://github.com/block/goose). Runs one-shot SSH commands over a fresh connection with no persisted session state.

## Install

```bash
uvx mcp-ssh-stateless@latest
```

Or add to your goose config:

```json
{
  "type": "stdio",
  "cmd": "uvx",
  "args": ["mcp-ssh-stateless@latest"]
}
```

## Tools

### `ssh_exec`

Run a single SSH command. Each invocation opens a new connection and tears it down when done — no ambient agent state, no stored credentials.

**Parameters:**

- `host` (str, required) — hostname or IP
- `username` (str, required) — SSH username
- `command` (str, required) — command to execute
- `password` (str, optional) — password auth
- `key_path` (str, optional) — path to private key (supports `~` expansion)
- `port` (int, default 22) — SSH port

Either `password` or `key_path` must be provided.

## Development

```bash
uv run --project extensions/ssh-stateless python -m pytest extensions/ssh-stateless/tests/
```
