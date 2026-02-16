// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use aegis_core::application::validation_service::ValidationService;
use aegis_core::domain::agent::AgentId;
use aegis_core::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction};
use aegis_core::domain::events::{ExecutionEvent, ValidationEvent};
use aegis_core::infrastructure::event_bus::{EventBus, DomainEvent};
use aegis_core::application::execution::ExecutionService;
use std::sync::Arc;
use async_trait::async_trait;
use tokio::time::timeout;
use std::time::Duration;

struct MockExecutionService;

#[async_trait]
impl ExecutionService for MockExecutionService {
    async fn start_execution(&self, _agent_id: AgentId, _input: ExecutionInput) -> anyhow::Result<ExecutionId> {
        Ok(ExecutionId::new())
    }
    async fn get_execution(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        // Return a completed execution so ValidationService can parse output
        let mut exec = Execution::new(AgentId::new(), ExecutionInput { intent: None, payload: serde_json::Value::Null }, 3);
        
        // Use public methods to populate state
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        
        let output = r#"
```json
{
    "score": 0.95,
    "confidence": 0.9,
    "reasoning": "Excellent code quality",
    "signals": []
}
```
"#;
        exec.complete_iteration(output.to_string());
        exec.complete();
        
        Ok(exec)
    }
    // We need to return iterations separately as per trait?
    async fn get_iterations(&self, _exec_id: ExecutionId) -> anyhow::Result<Vec<Iteration>> { 
        let output = r#"
```json
{
    "score": 0.95,
    "confidence": 0.9,
    "reasoning": "Excellent code quality",
    "signals": []
}
```
"#;
        let iteration = Iteration {
            number: 1,
            status: aegis_core::domain::execution::IterationStatus::Success,
            action: "validate".to_string(),
            output: Some(output.to_string()),
            validation_results: None,
            error: None,
            code_changes: None,
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            llm_interactions: vec![],
        };
        Ok(vec![iteration])
    }
    
    async fn cancel_execution(&self, _id: ExecutionId) -> anyhow::Result<()> { Ok(()) }
    async fn stream_execution(&self, _id: ExecutionId) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn stream_agent_events(&self, _id: AgentId) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<DomainEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn list_executions(&self, _agent_id: Option<AgentId>, _limit: usize) -> anyhow::Result<Vec<Execution>> { Ok(vec![]) }
    async fn delete_execution(&self, _id: ExecutionId) -> anyhow::Result<()> { Ok(()) }
    async fn record_llm_interaction(&self, _execution_id: ExecutionId, _iteration: u8, _interaction: LlmInteraction) -> anyhow::Result<()> { Ok(()) }
}

// ValidationService::run_judge logic:
// 1. calls get_execution -> checks status
// 2. calls iterations().last() on the returned execution object

// If the Execution struct stores iterations internally and we can't populate them via constructor, 
// then MockExecutionService::get_execution needs to return an Execution with iterations.
// Let's assume Execution has a way to add iterations or we can mock it differently?
// Or maybe we can rely on `get_iterations` being called? 
// No, `run_judge` calls `exec.iterations().last()`.
// If `Execution` struct manages iterations, we need to populate them.

#[tokio::test]
async fn test_validation_event_streaming() {
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let exec_service = Arc::new(MockExecutionService);
    let val_service = ValidationService::new(event_bus.clone(), exec_service.clone(), None);
    
    let execution_id = ExecutionId::new();
    let mut receiver = event_bus.subscribe_execution(execution_id);
    
    let request = aegis_core::domain::validation::ValidationRequest {
        content: "fn test() {}".to_string(),
        criteria: "valid rust".to_string(),
        context: None,
    };
    
    // Updated to use tuples with weights (judge_id, weight)
    let judges = vec![(AgentId::new(), 1.0)];
    
    // Spawn validation in background
    let handle = tokio::spawn(async move {
        val_service.validate_with_judges(
            execution_id,
            request,
            judges,
            None,  // Use default consensus config
            60,    // timeout_seconds
            500,   // poll_interval_ms
        ).await
    });
    
    // Listen for events
    // Expect GradientValidationPerformed
    let event1 = timeout(Duration::from_secs(5), receiver.recv()).await
        .expect("Timeout waiting for event 1")
        .expect("Event channel closed");
        
    match event1 {
        ExecutionEvent::Validation(ValidationEvent::GradientValidationPerformed { execution_id: eid, score, .. }) => {
            assert_eq!(eid, execution_id);
            assert_eq!(score, 0.95);
        },
        _ => panic!("Expected GradientValidationPerformed, got {:?}", event1),
    }

    // Expect MultiJudgeConsensus
    let event2 = timeout(Duration::from_secs(5), receiver.recv()).await
        .expect("Timeout waiting for event 2")
        .expect("Event channel closed");
        
    match event2 {
        ExecutionEvent::Validation(ValidationEvent::MultiJudgeConsensus { execution_id: eid, final_score, .. }) => {
            assert_eq!(eid, execution_id);
             // Consensus of one judge with 0.95 and high confidence should be close to 0.95
            assert!(final_score > 0.9);
        },
        _ => panic!("Expected MultiJudgeConsensus, got {:?}", event2),
    }
    
    // Ensure validation completed successfully
    let result = handle.await.unwrap();
    assert!(result.is_ok());
}
