# aegis-orchestrator-core

[![Crates.io](https://img.shields.io/crates/v/aegis-orchestrator-core.svg)](https://crates.io/crates/aegis-orchestrator-core)
[![Docs.rs](https://docs.rs/aegis-orchestrator-core/badge.svg)](https://docs.rs/aegis-orchestrator-core)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Documentation](https://img.shields.io/badge/docs-docs.100monkeys.ai-brightgreen.svg)](https://docs.100monkeys.ai)

Core domain logic and runtime primitives for the [100monkeys.ai AEGIS](https://docs.100monkeys.ai) orchestrator. This crate is the **pure domain kernel** — it has no dependency on any particular runtime, database, or HTTP framework except through trait abstractions.

## What's in this crate

| Layer | Contents |
| --- | --- |
| **Domain** | `Agent`, `Execution`, `Workflow`, `Swarm`, `SmcpSession`, `SecurityContext`, `Policy`, `NodeConfig`, `RuntimeRegistry` value objects and entities |
| **Application** | Orchestration services: `InnerLoopService`, `LifecycleService`, `ToolInvocationService`, `ValidationService`, `StorageRouter`, `VolumeManager`, `NfsGateway` |
| **Infrastructure** | Trait implementations for Docker runtime, LLM providers, Temporal client, Cortex/memory, gRPC (AEGIS Runtime Proto), SeaweedFS, OpenBao secrets |
| **Presentation** | Axum HTTP handlers for the `/v1` REST API and gRPC service endpoints |

## Key Concepts

- **Agent** — a stateless, containerised unit of autonomous work defined by an [`AgentManifest`](https://docs.100monkeys.ai/docs/reference/agent-manifest)
- **Execution** — a single agent invocation; lifecycle: `Pending → Running → Succeeded | Failed | Cancelled`
- **SMCP** — Secure Model Context Protocol (ADR-035); every tool call is signed with Ed25519 and validated end-to-end
- **Dispatch Gateway** — all tool calls route through the orchestrator proxy at `/v1/dispatch-gateway` (ADR-040); agents never call external APIs directly
- **Security Policy** — default-deny network and filesystem policies enforced at container/VM boot time

## Usage

This crate is not intended for direct use by agent authors — use [`aegis-orchestrator-sdk`](https://crates.io/crates/aegis-orchestrator-sdk) instead. This crate is consumed internally by the other workspace members and may be useful for advanced integrations.

```toml
[dependencies]
aegis-orchestrator-core = "0.12.0-pre-alpha"
```

```rust
use aegis_orchestrator_core::domain::agent::{AgentManifest, AgentSpec};
use aegis_orchestrator_core::domain::execution::Execution;
```

## Documentation

| Resource | Link |
| --- | --- |
| Getting Started | [docs.100monkeys.ai/docs/getting-started](https://docs.100monkeys.ai/docs/getting-started) |
| Architecture Overview | [docs.100monkeys.ai/docs/architecture](https://docs.100monkeys.ai/docs/architecture) |
| Execution Engine | [docs.100monkeys.ai/docs/architecture/execution-engine](https://docs.100monkeys.ai/docs/architecture/execution-engine) |
| SMCP | [docs.100monkeys.ai/docs/architecture/smcp](https://docs.100monkeys.ai/docs/architecture/smcp) |
| Agent Manifest Reference | [docs.100monkeys.ai/docs/reference/agent-manifest](https://docs.100monkeys.ai/docs/reference/agent-manifest) |
| Node Config Reference | [docs.100monkeys.ai/docs/reference/node-config](https://docs.100monkeys.ai/docs/reference/node-config) |
| Security Model | [docs.100monkeys.ai/docs/concepts/security-model](https://docs.100monkeys.ai/docs/concepts/security-model) |

## License

Copyright © 2026 100monkeys AI, Inc.

Licensed under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0) (AGPL-3.0).
