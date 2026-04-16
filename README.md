# AEGIS Orchestrator

The runtime and orchestration layer for AEGIS: policy-governed infrastructure for running autonomous AI agents without giving up control.

[![License](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Documentation](https://img.shields.io/badge/docs-docs.100monkeys.ai-brightgreen.svg)](https://docs.100monkeys.ai)

AEGIS is built for teams that want automation to be useful in production, not just impressive in a demo. If you are a DevOps engineer, SRE, or developer trying to run agents with predictable behavior, strong isolation, and operational visibility, this repository is the place to start.

## Why Teams Use AEGIS

- **Deterministic control:** explicit workflows, typed manifests, and orchestrator-managed execution reduce hidden behavior and ad hoc agent sprawl.
- **Built-in security controls:** network and filesystem access are policy-governed, runtimes are isolated, and production deployments can layer IAM and SEAL gateway controls on top of the orchestrator boundary.
- **Reliable operator workflows:** the CLI provides repeatable stack bring-up, status, update, and shutdown paths with health checks and machine-readable output.
- **Auditable automation:** structured logs, machine-readable CLI output, and orchestrator-mediated dispatch make agent activity easier to inspect and automate around.
- **Clear path to production:** Docker supports local development, while Firecracker-backed isolation is the intended production direction.

## What AEGIS Is

AEGIS Orchestrator is the layer that manages agent lifecycle, workflow execution, runtime isolation, security enforcement, and the operator experience around all of it. It gives you a controlled environment for running autonomous systems while preserving the things infrastructure teams care about most: predictable change, traceability, and blast-radius containment.

## Quick Start

### 1. Install the CLI

```bash
curl -fsSL https://get.100monkeys.ai | bash
```

That installer installs the `aegis` CLI from a pinned GitHub release, installs local prerequisites such as Docker when needed, and then runs `aegis up --tag <pinned-version>` to bring the local stack online. Re-running it is intended to be idempotent.

If you prefer to install the CLI from source yourself instead of using the installer:

```bash
cargo install aegis-orchestrator
```

### 2. If you installed the CLI manually, initialize a local AEGIS stack

```bash
aegis init
```

`aegis init` is the guided first-run path when you are not using the one-shot installer. It prepares stack files, configures the node, starts Docker Compose services, verifies health, and syncs built-in agents and workflows.

### 3. Check health

```bash
aegis status
```

### 4. Deploy an agent

```bash
aegis agent deploy <manifest.yaml>
```

See the [Agent Manifest Reference](https://docs.100monkeys.ai/docs/reference/agent-manifest) for the schema and the [Deploying Agents guide](https://docs.100monkeys.ai/docs/guides/deploying-agents) for examples.

### 5. Execute a task

```bash
aegis task execute hello-world --input "Write a Python function that sums 2 numbers together and outputs the result."
```

### 6. Stop the stack when you are done

```bash
aegis down
```

## What You Just Started

A local AEGIS setup gives you an orchestrator API, a gRPC surface, and supporting services needed for workflows and agent execution. The generated local stack typically exposes:

- Orchestrator API: `http://localhost:8088`
- gRPC: `localhost:50051`
- Temporal UI: `http://localhost:8233`

This is enough to start understanding the operator model: the orchestrator owns lifecycle, policy enforcement, and execution visibility, while agents run inside controlled runtimes. Local defaults are tuned for development bring-up rather than full production hardening.

## Core Concepts

### Deterministic execution

AEGIS is designed to make automation easier to reason about. Workflows are explicit, configuration is typed, and execution flows through the orchestrator rather than through a loose collection of direct agent integrations. That structure gives operators a more reproducible orchestration layer and gives developers a clearer contract for how work is scheduled, executed, and observed.

### Security and isolation

AEGIS centers security controls in the orchestrator. Network and filesystem policy are defined in manifests, development runs use Docker-backed isolation, and the production architecture is aimed at Firecracker micro-VM isolation. At the protocol layer, SEAL verifies tool calls end to end, and orchestrator-mediated dispatch keeps agent actions behind a control boundary. Local stack templates prioritize fast setup, so IAM and SEAL gateway auth are not enabled in every local dev path by default.

### Reliability and auditability

The CLI supports repeatable bring-up and shutdown flows such as `aegis init`, `aegis up`, `aegis status`, and `aegis down`. Structured logging via `tracing`, health checks, and machine-readable `--output <text|table|json|yaml>` modes make AEGIS friendlier to both human operators and automation pipelines, while explicit stack commands reduce drift in routine local operations.

### Building blocks

- **Agents:** stateless units of autonomous work defined by manifests.
- **Executions:** individual agent runs with a managed lifecycle.
- **Workflows:** explicit orchestration across states and agents.
- **Swarms:** multi-agent coordination with bounded control.
- **Security policies:** the permissions and runtime constraints that keep execution contained.

## Common Commands

```bash
# Start the stack if it has already been initialized
aegis up

# Show current stack health
aegis status

# Generate a node config file
aegis config generate --out ./aegis-config.yaml

# List agents in a machine-readable format
aegis --output json agent list

# Start the daemon with verbose logs
RUST_LOG=debug aegis daemon start
```

Interactive and streaming workflows remain text-first. For the current command surface, flags, and behavior, use the [CLI Reference](https://docs.100monkeys.ai/docs/reference/cli).

## Learn More

Start here if you are new:

- [Getting Started](https://docs.100monkeys.ai/docs/getting-started)
- [Execution Engine Architecture](https://docs.100monkeys.ai/docs/architecture/execution-engine)
- [Security Model](https://docs.100monkeys.ai/docs/concepts/security-model)

Use these when you are ready to configure and build:

- [CLI Reference](https://docs.100monkeys.ai/docs/reference/cli)
- [Node Config Reference](https://docs.100monkeys.ai/docs/reference/node-config)
- [Agent Manifest Reference](https://docs.100monkeys.ai/docs/reference/agent-manifest)
- [LLM Providers Guide](https://docs.100monkeys.ai/docs/guides/llm-providers)
- [Deploying Agents](https://docs.100monkeys.ai/docs/guides/deploying-agents)
- [Building Workflows](https://docs.100monkeys.ai/docs/guides/building-workflows)
- [Building Swarms](https://docs.100monkeys.ai/docs/guides/building-swarms)

## Billing

Stripe billing integration is available for SaaS deployments (e.g. Zaru at myzaru.com). Billing is a deployment-level concern configured via environment variables in the Zaru client and `aegis-platform-deployment`, not in the orchestrator node config. Self-hosted deployments can omit all Stripe variables and the billing UI is hidden automatically. See the [Billing Configuration](https://docs.100monkeys.ai/docs/guides/billing) guide for setup details.

## Development

### Git hooks

A pre-push hook lives in `.githooks/pre-push`. Activate it once after cloning:

```bash
git config core.hooksPath .githooks
```

Every `git push` will then run:

```bash
cargo fmt --all && \
  cargo clippy --workspace --locked -- -D warnings && \
  cargo build --release && \
  cargo test --workspace --locked && \
  cargo doc --no-deps
```

The push is blocked if any step fails.

### Building from source

If you want to work from source instead of the install script:

```bash
git clone https://github.com/100monkeys-ai/aegis-orchestrator
cd aegis-orchestrator
cargo build --release -p aegis-orchestrator
```

The CLI README and crate READMEs go deeper on command surface and internals:

- [`cli/README.md`](./cli/README.md)
- [`orchestrator/core/README.md`](./orchestrator/core/README.md)
- [`orchestrator/swarm/README.md`](./orchestrator/swarm/README.md)

## License

Copyright © 2026 100monkeys AI, Inc.

Licensed under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0) (AGPL-3.0).
