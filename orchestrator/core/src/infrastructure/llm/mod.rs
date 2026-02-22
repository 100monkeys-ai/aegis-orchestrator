// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # LLM Provider Anti-Corruption Layer (ADR-009)
//!
//! Adapter implementations translating between the orchestrator's internal
//! `LLMProvider` domain trait and vendor-specific APIs.
//! Following the Anti-Corruption Layer pattern (AGENTS.md §Anti-Corruption Layer),
//! external LLM concepts (model names, message formats, rate limits) are
//! translated at this boundary and never leak into domain types.
//!
//! | Module | Provider | Notes |
//! |--------|----------|-------|
//! | [`openai`] | OpenAI `gpt-*` / Azure OpenAI | Default provider |
//! | [`anthropic`] | Anthropic `claude-*` | |
//! | [`ollama`] | Ollama local models | Dev/offline use |
//! | [`registry`] | `ProviderRegistry` | Selects provider by manifest `spec.runtime.model` |
//!
//! See ADR-009 (LLM Provider Strategy).

// LLM Provider Infrastructure - Anti-Corruption Layer Implementations
//
// Implements LLM provider adapters following ADR-009.
// Each provider adapter translates between our domain interface and external APIs.

pub mod openai;
pub mod ollama;
pub mod anthropic;
pub mod registry;

pub use registry::ProviderRegistry;
