// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

// LLM Provider Infrastructure - Anti-Corruption Layer Implementations
//
// Implements LLM provider adapters following ADR-009.
// Each provider adapter translates between our domain interface and external APIs.

pub mod openai;
pub mod ollama;
pub mod anthropic;
pub mod registry;

pub use registry::ProviderRegistry;
