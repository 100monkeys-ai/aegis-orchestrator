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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aegis_env_returns_none_when_unset() {
        // Ensure we don't accidentally read a set var from the environment
        std::env::remove_var("AEGIS_ENV");
        assert!(aegis_env().is_none());
    }
}
