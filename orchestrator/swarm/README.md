# aegis-orchestrator-swarm

[![Crates.io](https://img.shields.io/crates/v/aegis-orchestrator-swarm.svg)](https://crates.io/crates/aegis-orchestrator-swarm)
[![Docs.rs](https://docs.rs/aegis-orchestrator-swarm/badge.svg)](https://docs.rs/aegis-orchestrator-swarm)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL%203.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Documentation](https://img.shields.io/badge/docs-docs.100monkeys.ai-brightgreen.svg)](https://docs.100monkeys.ai)

Swarm coordination layer for the [100monkeys.ai AEGIS](https://docs.100monkeys.ai) orchestrator. Implements the **Swarm** bounded context — parallel multi-agent orchestration with fan-out dispatch, result aggregation, and supervisor-managed lifecycles.

## What's in this crate

| Layer | Contents |
| --- | --- |
| **Domain** | `Swarm` aggregate, `SwarmSpec`, member state machine, fan-out / aggregation strategies |
| **Application** | `SwarmService` — create, dispatch, aggregate, and resolve swarms |

## Key Concepts

- **Swarm** — a named group of agents that execute the same stimulus in parallel and whose results are aggregated by a configurable strategy (e.g. `first-success`, `majority-vote`, `all`)
- **Supervisor** — an optional coordinating agent that reviews aggregated swarm output before surfacing a final result
- Swarms compose naturally with **Workflows** — a workflow step can fan out to a swarm and collect a single resolved output

```markdown
Stimulus ──► SwarmService.dispatch()
                │
          ┌─────┴─────┐
          ▼           ▼
       Agent A     Agent B     … Agent N
          │           │
          └─────┬─────┘
                ▼
           Aggregator
                │
           (Supervisor)
                │
                ▼
           Final Result
```

## Usage

```toml
[dependencies]
aegis-orchestrator-swarm = "0.12.0-pre-alpha"
```

```rust
use aegis_orchestrator_swarm::application::SwarmService;
```

## Documentation

| Resource | Link |
| --- | --- |
| Swarms Concept | [docs.100monkeys.ai/docs/concepts/swarms](https://docs.100monkeys.ai/docs/concepts/swarms) |
| Building Swarms Guide | [docs.100monkeys.ai/docs/guides/building-swarms](https://docs.100monkeys.ai/docs/guides/building-swarms) |
| Architecture Overview | [docs.100monkeys.ai/docs/architecture](https://docs.100monkeys.ai/docs/architecture) |

## License

Copyright © 2026 100monkeys AI, Inc.

Licensed under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0) (AGPL-3.0).
