# AEGIS Rust SDK

[![License](https://img.shields.io/badge/license-BSL%201.1-blue.svg)](LICENSE)

Build secure, autonomous agents with the AEGIS runtime.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
aegis-sdk = "0.1"
```

## Quick Start

```rust
use aegis_sdk::{AegisClient, AgentManifest, TaskInput};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a client
    let client = AegisClient::new("https://api.100monkeys.ai")
        .with_api_key("your-api-key");

    // Load agent manifest
    let manifest = AgentManifest::from_yaml_file("agent.yaml")?;

    // Deploy the agent
    let deployment = client.deploy_agent(&manifest).await?;
    println!("Agent deployed: {}", deployment.agent_id);

    // Execute a task
    let input = TaskInput {
        prompt: "Summarize my emails from today".to_string(),
        context: serde_json::json!({}),
    };

    let output = client.execute_task(&deployment.agent_id, input).await?;
    println!("Result: {:?}", output.result);

    Ok(())
}
```

## Features

- **Type-safe API**: Full type safety with Rust's type system
- **Async/await**: Built on Tokio for high-performance async operations
- **Manifest validation**: Compile-time validation of agent configurations
- **Error handling**: Comprehensive error types with context

## Documentation

See the [main documentation](../../README.md) for more details.

## 📜 License

Business Source License 1.1 - See [LICENSE](LICENSE) for details.
