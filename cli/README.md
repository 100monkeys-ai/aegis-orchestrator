# aegis-orchestrator

[![Crates.io](https://img.shields.io/crates/v/aegis-orchestrator.svg)](https://crates.io/crates/aegis-orchestrator)
[![Docs.rs](https://docs.rs/aegis-orchestrator/badge.svg)](https://docs.rs/aegis-orchestrator)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Documentation](https://img.shields.io/badge/docs-docs.100monkeys.ai-brightgreen.svg)](https://docs.100monkeys.ai)

The `aegis` CLI and daemon binary for the [100monkeys.ai AEGIS](https://docs.100monkeys.ai) platform — a secure, serverless runtime for autonomous AI agents.
For the complete command reference, use [the canonical docs](https://docs.100monkeys.ai/docs/reference/cli).

## Install

```bash
cargo install aegis-orchestrator
```

Or build from source:

```bash
git clone https://github.com/100monkeys-ai/aegis-orchestrator
cd aegis-orchestrator
cargo build --release -p aegis-orchestrator
```

## Quick Start

```bash
# 1. Start the daemon
aegis daemon start

# 2. Deploy an agent
aegis agent deploy agent.yaml

# 3. Run a task
aegis task execute my-agent --input "Summarise the README"

# 4. Use machine-readable output for automation
aegis --output json agent list
aegis --output yaml workflow describe my-workflow

# 5. Stream logs
aegis agent logs my-agent

# 6. Stop the daemon
aegis daemon stop
```

## Output Formats

Scriptable commands support a global `--output <text|table|json|yaml>` flag.
Use `json` or `yaml` for automation, and keep the default text/table output for operator workflows.

Examples:

```bash
aegis --output json daemon status
aegis --output json task list
aegis --output yaml config show
```

Streaming and interactive commands remain text-only in this pass, including:
`aegis init`, `aegis up`, `aegis down`, `aegis restart`, `aegis uninstall`,
`aegis task logs`, `aegis agent logs`, and `aegis workflow logs`.

`aegis config generate` now uses `--out <path>` for the destination file:

```bash
aegis config generate --out ./aegis-config.yaml
```

## Command Surface

Implemented top-level commands: `daemon`, `task`, `node`, `config`, `agent`, `workflow`, `update`, `init`, `down`, `up`, `restart`, `status`, `uninstall`.

`aegis node leave` currently exists in the CLI surface, but it returns the baseline-protocol error and should not be documented as a working workflow.

Use the docs reference for current subcommands, flags, and behavior.

### Debug Logging

```bash
# Enable verbose structured logs (includes bootstrap.py stdout/stderr)
RUST_LOG=debug aegis daemon start

# Or via config (aegis-config.yaml)
spec:
  observability:
    logging:
      level: debug   # trace | debug | info | warn | error
```

## Configuration

The daemon is configured via `aegis-config.yaml` in the working directory:

```yaml
apiVersion: 100monkeys.ai/v1
kind: NodeConfig
metadata:
  name: my-node
spec:
  node:
    id: my-node-001
    type: edge
  llm_providers:
    - name: local
      type: ollama
      endpoint: http://localhost:11434
      enabled: true
      models:
        - alias: default
          model: phi3:mini
          capabilities: [code, reasoning]
  llm_selection:
    strategy: prefer-local
    default_provider: local
```

See the [Node Config Reference](https://docs.100monkeys.ai/docs/reference/node-config) for all fields.

## Documentation

| Resource | Link |
| --- | --- |
| Getting Started | [docs.100monkeys.ai/docs/getting-started](https://docs.100monkeys.ai/docs/getting-started) |
| CLI Reference | [docs.100monkeys.ai/docs/reference/cli](https://docs.100monkeys.ai/docs/reference/cli) |
| Node Config Reference | [docs.100monkeys.ai/docs/reference/node-config](https://docs.100monkeys.ai/docs/reference/node-config) |
| Agent Manifest Reference | [docs.100monkeys.ai/docs/reference/agent-manifest](https://docs.100monkeys.ai/docs/reference/agent-manifest) |
| Deploying Agents | [docs.100monkeys.ai/docs/guides/deploying-agents](https://docs.100monkeys.ai/docs/guides/deploying-agents) |
| Docker Deployment | [docs.100monkeys.ai/docs/deployment/docker](https://docs.100monkeys.ai/docs/deployment/docker) |
| Firecracker Deployment | [docs.100monkeys.ai/docs/deployment/firecracker](https://docs.100monkeys.ai/docs/deployment/firecracker) |
| Local Testing | [docs.100monkeys.ai/docs/guides/local-testing](https://docs.100monkeys.ai/docs/guides/local-testing) |

## Related Crates

| Crate | Description |
| --- | --- |
| [`aegis-orchestrator-core`](https://crates.io/crates/aegis-orchestrator-core) | Domain logic and runtime primitives |
| [`aegis-orchestrator-swarm`](https://crates.io/crates/aegis-orchestrator-swarm) | Swarm coordination |
| [`aegis-orchestrator-sdk`](https://crates.io/crates/aegis-orchestrator-sdk) | Rust SDK for agent authors |

## License

Copyright © 2026 100monkeys AI, Inc.

Licensed under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0) (AGPL-3.0).
