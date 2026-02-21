// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::mcp::PolicyViolation;
use crate::domain::security_context::SecurityContext;
use serde_json::Value;

pub struct PolicyEngine;

impl PolicyEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn evaluate(
        &self,
        security_context: &SecurityContext,
        tool_name: &str,
        args: &Value,
    ) -> Result<(), PolicyViolation> {
        security_context.evaluate(tool_name, args)
    }
}
