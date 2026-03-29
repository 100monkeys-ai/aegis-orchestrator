// SPDX-License-Identifier: AGPL-3.0-only

//! Environment variable guard for execution contexts.
//!
//! Prevents orchestrator-internal environment variables from leaking into
//! agent containers, workflow steps, or any child execution context.
//! Only explicitly allowlisted AEGIS-prefixed variables and user-defined
//! variables that pass validation are permitted.

use std::collections::HashMap;

/// Environment variable prefixes that are BLOCKED from being passed to
/// execution contexts. These are orchestrator-internal variables.
const BLOCKED_PREFIXES: &[&str] = &[
    "DATABASE_",
    "DB_",
    "POSTGRES",
    "REDIS_",
    "OPENBAO_",
    "VAULT_",
    "SEAWEEDFS_",
    "KEYCLOAK_",
    "OIDC_",
    "OAUTH_",
    "JWT_SECRET",
    "HMAC_",
    "NFS_",
    "GRPC_",
    "OTEL_",
    "OTLP_",
    "CARGO",
    "RUSTUP",
    "RUST_LOG",
    "RUST_BACKTRACE",
];

/// Exact environment variable names that are BLOCKED from being passed
/// to execution contexts.
const BLOCKED_EXACT: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "SHELL",
    "HOSTNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "AEGIS_LOG_LEVEL",
    "AEGIS_NODE_SECRET",
    "AEGIS_APPROLE_SECRET_ID",
    "AEGIS_ADMIN_TOKEN",
    "AEGIS_INTERNAL_API_KEY",
    "AEGIS_ENCRYPTION_KEY",
    "AEGIS_SIGNING_KEY",
    "AEGIS_DB_URL",
    "AEGIS_REDIS_URL",
    "AEGIS_OPENBAO_TOKEN",
    "AEGIS_OPENBAO_ADDR",
    "AEGIS_GRPC_TLS_CERT",
    "AEGIS_GRPC_TLS_KEY",
    "AEGIS_WEBHOOK_SECRET",
    "SECRET_ID",
    "ROLE_ID",
];

/// AEGIS-prefixed variables that ARE allowed in execution contexts
/// (injected by the orchestrator for agent bootstrap).
const ALLOWED_AEGIS_VARS: &[&str] = &[
    "AEGIS_ORCHESTRATOR_URL",
    "AEGIS_BOOTSTRAP_DEBUG",
    "AEGIS_ITERATION",
    "AEGIS_ITERATION_HISTORY",
    "AEGIS_AGENT_ID",
    "AEGIS_EXECUTION_ID",
    "AEGIS_WORKFLOW_ID",
    "AEGIS_MODEL_ALIAS",
    "AEGIS_AGENT_INSTRUCTION",
    "AEGIS_PROMPT_TEMPLATE",
    "AEGIS_LLM_TIMEOUT_SECONDS",
];

/// Validates whether an environment variable name is safe to pass to
/// an execution context (agent container, workflow step, etc.).
///
/// Returns `true` if the variable is allowed, `false` if it should be blocked.
pub fn is_env_var_allowed(key: &str) -> bool {
    let key_upper = key.to_uppercase();

    // Check exact blocklist first
    if BLOCKED_EXACT.iter().any(|&b| key_upper == b) {
        return false;
    }

    // Check prefix blocklist
    if BLOCKED_PREFIXES.iter().any(|&p| key_upper.starts_with(p)) {
        return false;
    }

    // AEGIS-prefixed vars must be on the allowlist
    if key_upper.starts_with("AEGIS_") {
        return ALLOWED_AEGIS_VARS.iter().any(|&a| key_upper == a);
    }

    // All other user-defined vars are allowed
    true
}

/// Filters a map of environment variables, removing any that are blocked.
///
/// Returns the filtered map and a list of variable names that were removed.
/// The removed list can be used for audit logging.
pub fn filter_env_vars(env: &HashMap<String, String>) -> (HashMap<String, String>, Vec<String>) {
    let mut allowed = HashMap::new();
    let mut blocked = Vec::new();

    for (key, value) in env {
        if is_env_var_allowed(key) {
            allowed.insert(key.clone(), value.clone());
        } else {
            blocked.push(key.clone());
        }
    }

    (allowed, blocked)
}

/// Validates an `env:VAR_NAME` reference to ensure it doesn't resolve
/// orchestrator-internal variables. Returns `Ok(var_name)` if safe,
/// `Err(reason)` if the variable is blocked.
pub fn validate_env_ref(env_ref: &str) -> Result<&str, String> {
    let var_name = env_ref.strip_prefix("env:").unwrap_or(env_ref);

    if !is_env_var_allowed(var_name) {
        return Err(format!(
            "Environment variable '{}' is blocked: orchestrator-internal variables cannot be referenced from execution contexts",
            var_name
        ));
    }

    Ok(var_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_user_defined_vars() {
        assert!(is_env_var_allowed("MY_APP_KEY"));
        assert!(is_env_var_allowed("NODE_ENV"));
        assert!(is_env_var_allowed("API_ENDPOINT"));
        assert!(is_env_var_allowed("CUSTOM_SETTING"));
    }

    #[test]
    fn allows_aegis_bootstrap_vars() {
        assert!(is_env_var_allowed("AEGIS_ORCHESTRATOR_URL"));
        assert!(is_env_var_allowed("AEGIS_BOOTSTRAP_DEBUG"));
        assert!(is_env_var_allowed("AEGIS_ITERATION"));
        assert!(is_env_var_allowed("AEGIS_ITERATION_HISTORY"));
        assert!(is_env_var_allowed("AEGIS_AGENT_ID"));
        assert!(is_env_var_allowed("AEGIS_EXECUTION_ID"));
    }

    #[test]
    fn blocks_database_vars() {
        assert!(!is_env_var_allowed("DATABASE_URL"));
        assert!(!is_env_var_allowed("DB_PASSWORD"));
        assert!(!is_env_var_allowed("POSTGRES_PASSWORD"));
        assert!(!is_env_var_allowed("REDIS_URL"));
    }

    #[test]
    fn blocks_secret_management_vars() {
        assert!(!is_env_var_allowed("OPENBAO_TOKEN"));
        assert!(!is_env_var_allowed("VAULT_ADDR"));
        assert!(!is_env_var_allowed("AEGIS_OPENBAO_TOKEN"));
        assert!(!is_env_var_allowed("AEGIS_NODE_SECRET"));
        assert!(!is_env_var_allowed("SECRET_ID"));
        assert!(!is_env_var_allowed("ROLE_ID"));
    }

    #[test]
    fn blocks_unknown_aegis_internal_vars() {
        assert!(!is_env_var_allowed("AEGIS_LOG_LEVEL"));
        assert!(!is_env_var_allowed("AEGIS_ADMIN_TOKEN"));
        assert!(!is_env_var_allowed("AEGIS_INTERNAL_API_KEY"));
        assert!(!is_env_var_allowed("AEGIS_DB_URL"));
        assert!(!is_env_var_allowed("AEGIS_SOME_NEW_INTERNAL_VAR"));
    }

    #[test]
    fn blocks_infrastructure_vars() {
        assert!(!is_env_var_allowed("KEYCLOAK_ADMIN"));
        assert!(!is_env_var_allowed("OIDC_CLIENT_SECRET"));
        assert!(!is_env_var_allowed("GRPC_TLS_CERT"));
        assert!(!is_env_var_allowed("OTEL_EXPORTER_ENDPOINT"));
    }

    #[test]
    fn blocks_system_vars() {
        assert!(!is_env_var_allowed("HOME"));
        assert!(!is_env_var_allowed("PATH"));
        assert!(!is_env_var_allowed("SHELL"));
    }

    #[test]
    fn filter_env_vars_separates_allowed_and_blocked() {
        let mut env = HashMap::new();
        env.insert("MY_APP_KEY".to_string(), "value1".to_string());
        env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        env.insert(
            "AEGIS_ORCHESTRATOR_URL".to_string(),
            "http://localhost".to_string(),
        );
        env.insert(
            "AEGIS_DB_URL".to_string(),
            "postgres://internal".to_string(),
        );

        let (allowed, blocked) = filter_env_vars(&env);

        assert_eq!(allowed.len(), 2);
        assert!(allowed.contains_key("MY_APP_KEY"));
        assert!(allowed.contains_key("AEGIS_ORCHESTRATOR_URL"));
        assert_eq!(blocked.len(), 2);
        assert!(blocked.contains(&"DATABASE_URL".to_string()));
        assert!(blocked.contains(&"AEGIS_DB_URL".to_string()));
    }

    #[test]
    fn validate_env_ref_blocks_internal_vars() {
        assert!(validate_env_ref("env:DATABASE_URL").is_err());
        assert!(validate_env_ref("env:AEGIS_OPENBAO_TOKEN").is_err());
        assert!(validate_env_ref("env:SECRET_ID").is_err());
    }

    #[test]
    fn validate_env_ref_allows_safe_vars() {
        assert!(validate_env_ref("env:MY_REGISTRY_CREDS").is_ok());
        assert!(validate_env_ref("env:DOCKER_TOKEN").is_ok());
    }

    #[test]
    fn case_insensitive_blocking() {
        assert!(!is_env_var_allowed("database_url"));
        assert!(!is_env_var_allowed("Database_Password"));
        assert!(!is_env_var_allowed("aegis_db_url"));
    }
}
