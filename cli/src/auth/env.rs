// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
/// Returns the value of AEGIS_KEY env var if set and non-empty.
pub fn aegis_key() -> Option<String> {
    std::env::var("AEGIS_KEY").ok().filter(|v| !v.is_empty())
}

/// Returns the value of AEGIS_ENV env var if set and non-empty.
pub fn aegis_env() -> Option<String> {
    std::env::var("AEGIS_ENV").ok().filter(|v| !v.is_empty())
}
