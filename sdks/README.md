# aegis-orchestrator-sdk

[![Crates.io](https://img.shields.io/crates/v/aegis-orchestrator-sdk.svg)](https://crates.io/crates/aegis-orchestrator-sdk)
[![Docs.rs](https://docs.rs/aegis-orchestrator-sdk/badge.svg)](https://docs.rs/aegis-orchestrator-sdk)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Documentation](https://img.shields.io/badge/docs-docs.100monkeys.ai-brightgreen.svg)](https://docs.100monkeys.ai)

Rust SDK for building and deploying agents on the [100monkeys.ai AEGIS](https://docs.100monkeys.ai) platform. Provides a type-safe, fluent API for defining agent manifests, submitting executions, and watching iteration progress — without depending on the full orchestrator binary.

## Features

- **`AegisClient`** — HTTP client for the AEGIS `/v1` REST API (deploy agents, execute tasks, stream logs)
- **Manifest types** — re-exports `AgentManifest`, `WorkflowManifest`, and all related value objects directly from `aegis-orchestrator-core` so your types always match the orchestrator
- **Single import path** — `use aegis_orchestrator_sdk::AgentManifest` just works; no digging into internal crates

## Installation

```toml
[dependencies]
aegis-orchestrator-sdk = "0.X.0-pre-alpha"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

```rust
use aegis_orchestrator_sdk::{AegisClient, AgentManifest, TaskInput};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = AegisClient::new("http://127.0.0.1:8088")
        .with_api_key("your-jwt-token");

    // Load a manifest from YAML
    let manifest: AgentManifest = serde_yaml::from_str(include_str!("agent.yaml"))?;

    // Deploy the agent
    let deployment = client.deploy_agent(&manifest).await?;
    println!("Deployed: {}", deployment.agent_id);

    // Execute a task
    let result = client.execute_task(
        &deployment.agent_id,
        TaskInput {
            input: "Summarise the latest pull requests".into(),
            context: Default::default(),
        },
    ).await?;

    println!("Output: {}", result.output);
    Ok(())
}
```

## Modules

| Module | Description |
| --- | --- |
| [`client`](https://docs.rs/aegis-orchestrator-sdk/latest/aegis_orchestrator_sdk/client/) | `AegisClient` — wraps `reqwest` with typed request/response pairs |
| [`types`](https://docs.rs/aegis-orchestrator-sdk/latest/aegis_orchestrator_sdk/types/) | `TaskInput`, `TaskOutput`, `DeploymentResponse`, execution watcher helpers |

## Documentation

| Resource | Link |
| --- | --- |
| Getting Started | [docs.100monkeys.ai/docs/getting-started](https://docs.100monkeys.ai/docs/getting-started) |
| Writing Agents | [docs.100monkeys.ai/docs/guides/writing-agents](https://docs.100monkeys.ai/docs/guides/writing-agents) |
| Deploying Agents | [docs.100monkeys.ai/docs/guides/deploying-agents](https://docs.100monkeys.ai/docs/guides/deploying-agents) |
| Agent Manifest Reference | [docs.100monkeys.ai/docs/reference/agent-manifest](https://docs.100monkeys.ai/docs/reference/agent-manifest) |
| gRPC API Reference | [docs.100monkeys.ai/docs/reference/grpc-api](https://docs.100monkeys.ai/docs/reference/grpc-api) |
| Security Model | [docs.100monkeys.ai/docs/concepts/security-model](https://docs.100monkeys.ai/docs/concepts/security-model) |

## Other SDKs

- [aegis-sdk-python](https://github.com/100monkeys-ai/aegis-sdk-python) — Python SDK
- [aegis-sdk-typescript](https://github.com/100monkeys-ai/aegis-sdk-typescript) — TypeScript SDK

## License

Copyright © 2026 100monkeys AI, Inc.

Licensed under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0) (AGPL-3.0).
