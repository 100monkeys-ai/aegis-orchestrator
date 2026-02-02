/// Core domain types for the AEGIS orchestrator.
/// 
/// This module implements Domain-Driven Design principles with pure business logic.
/// Infrastructure concerns (IO, databases, API) are handled by adapters.

pub mod agent;
pub mod runtime;
pub mod security;
pub mod swarm;
pub mod memory;

pub use agent::*;
pub use runtime::*;
pub use security::*;
pub use swarm::*;
pub use memory::*;
