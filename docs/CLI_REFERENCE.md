# AEGIS CLI Reference

Complete reference for all `aegis` CLI commands.

## Table of Contents

- [Global Options](#global-options)
- [Daemon Commands](#daemon-commands)
- [Agent Commands](#agent-commands)
- [Task Commands](#task-commands)
- [Config Commands](#config-commands)
- [Environment Variables](#environment-variables)
- [Exit Codes](#exit-codes)

## Global Options

These options apply to all commands:

```bash
aegis [OPTIONS] <COMMAND>
```

| Option | Description | Default |
| -------- | ------------- | --------- |
| `--config <FILE>` | Path to configuration file | Auto-discover |
| `--port <PORT>` | HTTP API port | 8000 |
| `--log-level <LEVEL>` | Log level (trace, debug, info, warn, error) | info |
| `--daemon` | Run as background daemon | false |
| `--help` | Print help information | - |
| `--version` | Print version information | - |

### Environment Variables

- `AEGIS_CONFIG_PATH` - Override config file path
- `AEGIS_PORT` - Override API port
- `AEGIS_LOG_LEVEL` - Override log level

## Daemon Commands

Manage the AEGIS daemon lifecycle.

### `aegis daemon start`

Start the daemon as a background service.

```bash
aegis daemon start [OPTIONS]
```

**Behavior:**

- Checks if daemon is already running
- Validates configuration file
- Spawns detached background process
- Writes logs to `/tmp/aegis.out` and `/tmp/aegis.err`

**Examples:**

```bash
# Start with default config
aegis daemon start

# Start with custom config
aegis daemon start --config ./my-config.yaml

# Start on custom port
aegis daemon start --port 9000
```

**Output:**

```bash
✓ Daemon starting (PID: 12345)
Check status with: aegis daemon status
```

---

### `aegis daemon stop`

Stop the running daemon.

```bash
aegis daemon stop [OPTIONS]
```

**Options:**

| Option                | Description                              | Default |
|-----------------------|------------------------------------------|---------|
| `--force`             | Force kill if graceful shutdown fails    | false   |
| `--timeout <SECONDS>` | Graceful shutdown timeout                | 30      |

**Examples:**

```bash
# Graceful shutdown
aegis daemon stop

# Force kill after 10 seconds
aegis daemon stop --timeout 10 --force
```

---

### `aegis daemon status`

Check daemon status.

```bash
aegis daemon status
```

**Output (Running):**

```bash
✓ Daemon is running
  PID: 12345
  Uptime: 2h 15m
```

**Output (Stopped):**

```bash
✗ Daemon is not running
```

**Output (Unhealthy):**

```bash
⚠ Daemon unhealthy (PID: 12345)
  Process exists but HTTP API check failed: connection refused
  Check logs at /tmp/aegis.out and /tmp/aegis.err
```

---

### `aegis daemon install`

Install daemon as a system service (Linux/macOS only).

```bash
aegis daemon install [OPTIONS]
```

**Options:**

| Option                 | Description                                           |
|------------------------|-------------------------------------------------------|
| `--binary-path <PATH>` | Path to aegis binary (default: current executable)    |
| `--user <USER>`        | User to run service as (Unix only)                    |

**Examples:**

```bash
# Install as systemd service
sudo aegis daemon install

# Install with custom binary path
sudo aegis daemon install --binary-path /usr/local/bin/aegis
```

---

### `aegis daemon uninstall`

Uninstall system service.

```bash
aegis daemon uninstall
```

## Agent Commands

Manage agent deployment and lifecycle.

### `aegis agent deploy`

Deploy an agent from a manifest file.

```bash
aegis agent deploy <MANIFEST>
```

**Arguments:**

| Argument     | Description                          |
|--------------|--------------------------------------|
| `<MANIFEST>` | Path to agent manifest YAML file     |

**Examples:**

```bash
# Deploy single agent
aegis agent deploy ./demo-agents/echo/agent.yaml

# Deploy multiple agents
aegis agent deploy ./agents/*/agent.yaml
```

**Output:**

```bash
Deploying agent: echo
✓ Agent deployed: a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

---

### `aegis agent list`

List all deployed agents.

```bash
aegis agent list
```

**Output:**

```bash
3 agents found:
ID                                   NAME                 VERSION    STATUS
a1b2c3d4-e5f6-7890-abcd-ef1234567890 echo                 1.1        ready
b2c3d4e5-f6a7-8901-bcde-f12345678901 greeter              0.1.0      ready
c3d4e5f6-a7b8-9012-cdef-123456789012 coder                0.1.0      ready
```

---

### `aegis agent show`

Display agent configuration as YAML.

```bash
aegis agent show <AGENT_ID>
```

**Arguments:**

| Argument     | Description |
|--------------|-------------|
| `<AGENT_ID>` | Agent UUID  |

**Examples:**

```bash
aegis agent show a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

**Output:**

```yaml
version: "1.1"
agent:
  name: "echo"
  runtime: "python:3.11"
task:
  instruction: "Echo the input back to the user."
```

---

### `aegis agent logs`

Stream logs for an agent (all executions).

```bash
aegis agent logs <AGENT_ID> [OPTIONS]
```

**Arguments:**

| Argument     | Description        |
|--------------|--------------------|
| `<AGENT_ID>` | Agent UUID or name |

**Options:**

| Option     | Description                | Default |
|------------|----------------------------|---------|
| `--follow` | Follow log output (stream) | false   |
| `--errors` | Show errors only           | false   |

**Examples:**

```bash
# View historical logs
aegis agent logs echo

# Stream logs in real-time
aegis agent logs echo --follow

# Stream errors only
aegis agent logs echo --follow --errors
```

---

### `aegis agent remove`

Remove a deployed agent.

```bash
aegis agent remove <AGENT_ID>
```

**Arguments:**

| Argument     | Description |
|--------------|-------------|
| `<AGENT_ID>` | Agent UUID  |

**Examples:**

```bash
aegis agent remove a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

**Output:**

```bash
✓ Agent a1b2c3d4-e5f6-7890-abcd-ef1234567890 removed
```

## Task Commands

Execute and manage agent tasks.

### `aegis task execute`

Execute an agent task.

```bash
aegis task execute <AGENT> [OPTIONS]
```

**Arguments:**

| Argument  | Description                        |
|-----------|------------------------------------|
| `<AGENT>` | Agent UUID, name, or manifest path |

**Options:**

| Option            | Description                            | Default |
|-------------------|----------------------------------------|---------|
| `--input <INPUT>` | Input data (JSON string or @file.json) | `{}`    |
| `--wait`          | Wait for execution to complete         | false   |
| `--follow`        | Follow execution logs                  | false   |

**Input Formats:**

1. **Plain text** (converted to string):

   ```bash
   aegis task execute echo --input "Hello"
   ```

2. **JSON object**:

   ```bash
   aegis task execute coder --input '{"question": "How to use async?"}'
   ```

3. **File reference** (prefix with `@`):

   ```bash
   aegis task execute poet --input @input.json
   ```

**Examples:**

```bash
# Execute with text input
aegis task execute echo --input "Hello, AEGIS!"

# Execute with JSON input
aegis task execute greeter --input '{"name": "Alice"}'

# Execute and follow logs
aegis task execute coder --input "Explain Rust traits" --follow

# Execute from file
echo '{"topic": "stars"}' > input.json
aegis task execute poet --input @input.json
```

**Output:**

```bash
Executing agent a1b2c3d4-e5f6-7890-abcd-ef1234567890...
✓ Execution started: b2c3d4e5-f6a7-8901-bcde-f12345678901
```

---

### `aegis task status`

Check execution status.

```bash
aegis task status <EXECUTION_ID>
```

**Arguments:**

| Argument         | Description    |
|------------------|----------------|
| `<EXECUTION_ID>` | Execution UUID |

**Examples:**

```bash
aegis task status b2c3d4e5-f6a7-8901-bcde-f12345678901
```

**Output:**

```bash
Execution b2c3d4e5-f6a7-8901-bcde-f12345678901
  Status: completed
  Agent: a1b2c3d4-e5f6-7890-abcd-ef1234567890
  Started: 2026-02-07T13:15:30Z
  Ended: 2026-02-07T13:15:32Z
```

---

### `aegis task logs`

View execution logs.

```bash
aegis task logs <EXECUTION_ID> [OPTIONS]
```

**Arguments:**

| Argument         | Description    |
|------------------|----------------|
| `<EXECUTION_ID>` | Execution UUID |

**Options:**

| Option          | Description       | Default |
|-----------------|-------------------|---------|
| `--follow`      | Follow log output | false   |
| `--errors-only` | Show errors only  | false   |

**Examples:**

```bash
# View logs
aegis task logs b2c3d4e5-f6a7-8901-bcde-f12345678901

# Stream logs
aegis task logs b2c3d4e5-f6a7-8901-bcde-f12345678901 --follow
```

---

### `aegis task list`

List recent executions.

```bash
aegis task list [OPTIONS]
```

**Options:**

| Option              | Description     | Default    |
|---------------------|-----------------|------------|
| `--agent-id <UUID>` | Filter by agent | All agents |
| `--limit <N>`       | Maximum results | 20         |

**Examples:**

```bash
# List all recent executions
aegis task list

# List executions for specific agent
aegis task list --agent-id a1b2c3d4-e5f6-7890-abcd-ef1234567890

# List last 50 executions
aegis task list --limit 50
```

**Output:**

```bash
5 executions:
  b2c3d4e5-f6a7-8901-bcde-f12345678901 - Agent: a1b2c3d4-... - completed
  c3d4e5f6-a7b8-9012-cdef-123456789012 - Agent: b2c3d4e5-... - running
  d4e5f6a7-b8c9-0123-def0-123456789012 - Agent: a1b2c3d4-... - failed
```

---

### `aegis task cancel`

Cancel a running execution.

```bash
aegis task cancel <EXECUTION_ID> [OPTIONS]
```

**Arguments:**

| Argument         | Description    |
|------------------|----------------|
| `<EXECUTION_ID>` | Execution UUID |

**Options:**

| Option    | Description                          | Default |
|-----------|--------------------------------------|---------|
| `--force` | Force kill without graceful shutdown | false   |

**Examples:**

```bash
# Graceful cancellation
aegis task cancel b2c3d4e5-f6a7-8901-bcde-f12345678901

# Force kill
aegis task cancel b2c3d4e5-f6a7-8901-bcde-f12345678901 --force
```

---

### `aegis task remove`

Remove an execution record.

```bash
aegis task remove <EXECUTION_ID>
```

**Arguments:**

| Argument         | Description    |
|------------------|----------------|
| `<EXECUTION_ID>` | Execution UUID |

**Examples:**

```bash
aegis task remove b2c3d4e5-f6a7-8901-bcde-f12345678901
```

## Config Commands

Manage configuration.

### `aegis config show`

Display current configuration.

```bash
aegis config show
```

**Output:**

```yaml
node:
  id: "dev-node-001"
  type: "edge"
  name: "my-development-node"
# ... full config
```

---

### `aegis config validate`

Validate configuration file.

```bash
aegis config validate [--config <FILE>]
```

**Examples:**

```bash
# Validate default config
aegis config validate

# Validate specific file
aegis config validate --config ./my-config.yaml
```

---

### `aegis config generate`

Generate example configuration.

```bash
aegis config generate
```

## Environment Variables Reference

| Variable | Description | Default |
| ---------- | ------------- | --------- |
| `AEGIS_CONFIG_PATH` | Configuration file path | Auto-discover |
| `AEGIS_PORT` | HTTP API port | 8000 |
| `AEGIS_LOG_LEVEL` | Log level | info |

## Exit Codes

| Code | Meaning |
| ------ | --------- |
| 0 | Success |
| 1 | General error |
| 2 | Configuration error |
| 3 | Daemon not running (when required) |
| 4 | Agent not found |
| 5 | Execution failed |

---

**See Also:**

- [Getting Started Guide](GETTING_STARTED.md)
- [Agent Development Guide](AGENT_DEVELOPMENT.md)
- [Troubleshooting](TROUBLESHOOTING.md)
