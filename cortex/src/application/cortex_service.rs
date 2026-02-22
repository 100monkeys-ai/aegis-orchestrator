// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # CortexService — Pattern Storage & Retrieval (BC-5, ADR-018/024)
//!
//! Application service implementing the 100monkeys learning loop:
//! every failed→refined iteration is stored as a [`CortexPattern`], enabling
//! semantic retrieval at the start of future executions that encounter the same
//! error class.
//!
//! ## Deduplication
//!
//! When a new pattern's embedding similarity to an existing pattern exceeds
//! `deduplication_threshold` (default 0.95), the existing pattern's `weight`
//! is incremented instead of creating a duplicate. This prevents the store
//! from growing unboundedly on common errors (ADR-018 §Weight).
//!
//! ## Reinforcement Signals
//!
//! - **Dopamine** (`apply_dopamine`): Boost `success_rate` and `weight` when
//!   a pattern led to a successful execution.
//! - **Cortisol** (`apply_cortisol`): Penalise `success_rate` when a pattern
//!   was tried and failed, marking it less likely to be retrieved again.
//!
//! See ADR-018 (Weighted Cortex Memory), ADR-024 (Holographic Cortex Memory),
//! ADR-029 (Time-Decay Parameters).

use std::sync::Arc;
use async_trait::async_trait;
use anyhow::Result;
use chrono::Utc;

use crate::domain::{CortexPattern, PatternId, ErrorSignature, CortexEvent, WeightIncreaseReason};
use crate::infrastructure::PatternRepository;

/// Event bus trait for publishing domain events
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, event: CortexEvent) -> Result<()>;
}

/// CortexService interface
#[async_trait]
pub trait CortexService: Send + Sync {
    /// Store a new pattern with automatic deduplication
    /// If a similar pattern exists (similarity > 0.95), increments its weight instead
    async fn store_pattern(
        &self,
        execution_id: Option<uuid::Uuid>,
        error_signature: ErrorSignature,
        solution_code: String,
        task_category: String,
        embedding: Vec<f32>,
    ) -> Result<PatternId>;
    
    /// Search for patterns similar to the query
    /// Returns patterns ranked by resonance score (similarity + success + recency)
    async fn search_patterns(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<CortexPattern>>;
    
    /// Update pattern success score from validation results
    /// Integrates with ADR-017 (Gradient Validation System)
    async fn update_pattern_success(
        &self,
        pattern_id: PatternId,
        execution_id: Option<uuid::Uuid>,
        success_score: f64,
    ) -> Result<()>;
    
    /// Apply dopamine (success reinforcement)
    async fn apply_dopamine(&self, pattern_id: PatternId, execution_id: Option<uuid::Uuid>, amount: f64) -> Result<()>;
    
    /// Apply cortisol (failure penalty)
    async fn apply_cortisol(&self, pattern_id: PatternId, penalty: f64) -> Result<()>;
    
    /// Get pattern by ID
    async fn get_pattern(&self, pattern_id: PatternId) -> Result<Option<CortexPattern>>;
    
    /// Prune old/low-weight patterns (sleep cycle - consolidation)
    async fn prune_patterns(&self, min_weight: f64, max_age_days: i64) -> Result<usize>;
}

/// Standard implementation of CortexService
pub struct StandardCortexService {
    pattern_repo: Arc<dyn PatternRepository>,
    event_bus: Arc<dyn EventBus>,
    deduplication_threshold: f64,  // Default: 0.95
}

impl StandardCortexService {
    pub fn new(
        pattern_repo: Arc<dyn PatternRepository>,
        event_bus: Arc<dyn EventBus>,
    ) -> Self {
        Self {
            pattern_repo,
            event_bus,
            deduplication_threshold: 0.95,
        }
    }
    
    pub fn with_deduplication_threshold(mut self, threshold: f64) -> Self {
        self.deduplication_threshold = threshold;
        self
    }
    
    /// Check for duplicate patterns using vector similarity
    async fn check_for_duplicate(
        &self,
        embedding: &[f32],
    ) -> Result<Option<CortexPattern>> {
        let similar = self.pattern_repo.search_similar(embedding.to_vec(), 1).await?;
        
        if let Some((pattern, similarity)) = similar.first() {
            if *similarity > self.deduplication_threshold {
                return Ok(Some(pattern.clone()));
            }
        }
        
        Ok(None)
    }
}

#[async_trait]
impl CortexService for StandardCortexService {
    async fn store_pattern(
        &self,
        execution_id: Option<uuid::Uuid>,
        error_signature: ErrorSignature,
        solution_code: String,
        task_category: String,
        embedding: Vec<f32>,
    ) -> Result<PatternId> {
        // Check for duplicate (ADR-018 deduplication)
        if let Some(mut existing) = self.check_for_duplicate(&embedding).await? {
            // Increment weight instead of storing duplicate (Hebbian learning)
            let old_weight = existing.weight;
            existing.increment_weight(1.0);
            self.pattern_repo.update_pattern(&existing).await?;
            
            self.event_bus.publish(CortexEvent::PatternWeightIncreased {
                pattern_id: existing.id,
                execution_id,
                old_weight,
                new_weight: existing.weight,
                reason: WeightIncreaseReason::Deduplication,
                timestamp: Utc::now(),
            }).await?;
            
            return Ok(existing.id);
        }
        
        // No duplicate, store new pattern
        let pattern = CortexPattern::new(error_signature.clone(), solution_code, task_category);
        let pattern_id = pattern.id;
        
        self.pattern_repo.store_pattern(&pattern, embedding).await?;
        
        self.event_bus.publish(CortexEvent::PatternDiscovered {
            pattern_id,
            execution_id,
            error_signature: format!("{:?}", error_signature),
            task_category: pattern.task_category.clone(),
            timestamp: Utc::now(),
        }).await?;
        
        Ok(pattern_id)
    }
    
    async fn search_patterns(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<CortexPattern>> {
        // Search with higher limit for re-ranking
        let results = self.pattern_repo.search_similar(query_embedding, limit * 5).await?;
        
        // Re-rank by resonance score (ADR-018)
        let mut ranked: Vec<_> = results.into_iter()
            .map(|(pattern, similarity)| {
                let recency_weight = calculate_recency_weight(&pattern.last_verified);
                let resonance = pattern.resonance_score(similarity, recency_weight);
                (pattern, resonance)
            })
            .collect();
        
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        
        Ok(ranked.into_iter()
            .take(limit)
            .map(|(pattern, _)| pattern)
            .collect())
    }
    
    async fn update_pattern_success(
        &self,
        pattern_id: PatternId,
        execution_id: Option<uuid::Uuid>,
        success_score: f64,
    ) -> Result<()> {
        let mut pattern = self.pattern_repo.find_by_id(pattern_id).await?
            .ok_or_else(|| anyhow::anyhow!("Pattern not found"))?;
        
        let old_score = pattern.success_score;
        pattern.update_success_score(success_score);
        
        self.pattern_repo.update_pattern(&pattern).await?;
        
        self.event_bus.publish(CortexEvent::PatternSuccessUpdated {
            pattern_id,
            execution_id,
            old_score,
            new_score: pattern.success_score,
            execution_count: pattern.execution_count,
            timestamp: Utc::now(),
        }).await?;
        
        Ok(())
    }
    
    async fn apply_dopamine(&self, pattern_id: PatternId, execution_id: Option<uuid::Uuid>, amount: f64) -> Result<()> {
        let mut pattern = self.pattern_repo.find_by_id(pattern_id).await?
            .ok_or_else(|| anyhow::anyhow!("Pattern not found"))?;
        
        let old_weight = pattern.weight;
        pattern.increment_weight(amount);
        
        self.pattern_repo.update_pattern(&pattern).await?;
        
        self.event_bus.publish(CortexEvent::PatternWeightIncreased {
            pattern_id,
            execution_id,
            old_weight,
            new_weight: pattern.weight,
            reason: WeightIncreaseReason::Dopamine,
            timestamp: Utc::now(),
        }).await?;
        
        Ok(())
    }
    
    async fn apply_cortisol(&self, pattern_id: PatternId, penalty: f64) -> Result<()> {
        let mut pattern = self.pattern_repo.find_by_id(pattern_id).await?
            .ok_or_else(|| anyhow::anyhow!("Pattern not found"))?;
        
        pattern.weight = (pattern.weight - penalty).max(0.1);  // Minimum 0.1
        
        self.pattern_repo.update_pattern(&pattern).await?;
        
        // Note: We could add a PatternWeightDecreased event if needed
        
        Ok(())
    }
    
    async fn get_pattern(&self, pattern_id: PatternId) -> Result<Option<CortexPattern>> {
        self.pattern_repo.find_by_id(pattern_id).await
    }
    
    async fn prune_patterns(&self, min_weight: f64, max_age_days: i64) -> Result<usize> {
        let all_patterns = self.pattern_repo.get_all_patterns().await?;
        let mut pruned_count = 0;
        
        for pattern in all_patterns {
            if pattern.should_prune(min_weight, max_age_days) {
                let age_days = (Utc::now() - pattern.last_verified).num_days();
                
                self.pattern_repo.delete_pattern(pattern.id).await?;
                
                self.event_bus.publish(CortexEvent::PatternPruned {
                    pattern_id: pattern.id,
                    final_weight: pattern.weight,
                    age_days,
                    reason: format!("weight={}, age_days={}", pattern.weight, age_days),
                    timestamp: Utc::now(),
                }).await?;
                
                pruned_count += 1;
            }
        }
        
        Ok(pruned_count)
    }
}

/// Calculate recency weight based on last verification time
fn calculate_recency_weight(last_verified: &chrono::DateTime<Utc>) -> f64 {
    let days_old = (Utc::now() - *last_verified).num_days();
    let decay_rate = 0.01;  // 1% per day
    (-decay_rate * days_old as f64).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::InMemoryPatternRepository;
    use std::sync::Mutex;
    
    // Mock EventBus for testing
    struct MockEventBus {
        events: Arc<Mutex<Vec<CortexEvent>>>,
    }
    
    impl MockEventBus {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }
        
        fn get_events(&self) -> Vec<CortexEvent> {
            self.events.lock().unwrap().clone()
        }
    }
    
    #[async_trait]
    impl EventBus for MockEventBus {
        async fn publish(&self, event: CortexEvent) -> Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }
    
    #[tokio::test]
    async fn test_store_pattern() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo, event_bus.clone());
        
        let signature = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let embedding = vec![0.1, 0.2, 0.3];
        
        let id = service.store_pattern(
            None,
            signature,
            "pip install requests".to_string(),
            "dependency".to_string(),
            embedding,
        ).await.unwrap();
        
        assert!(id.0.as_u128() > 0);
        
        // Check event was published
        let events = event_bus.get_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), "pattern_discovered");
    }
    
    #[tokio::test]
    async fn test_deduplication() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo.clone(), event_bus.clone());
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let embedding = vec![1.0, 0.0, 0.0];
        
        // Store first pattern
        let id1 = service.store_pattern(
            None,
            signature.clone(),
            "pip install requests".to_string(),
            "dep".to_string(),
            embedding.clone(),
        ).await.unwrap();
        
        // Store duplicate (same embedding)
        let id2 = service.store_pattern(
            None,
            signature.clone(),
            "pip install requests".to_string(),
            "dep".to_string(),
            embedding.clone(),
        ).await.unwrap();
        
        // Should return same ID
        assert_eq!(id1, id2);
        
        // Check weight was incremented
        let pattern = repo.find_by_id(id1).await.unwrap().unwrap();
        assert_eq!(pattern.weight, 2.0);
        
        // Check events
        let events = event_bus.get_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type(), "pattern_discovered");
        assert_eq!(events[1].event_type(), "pattern_weight_increased");
    }
    
    #[tokio::test]
    async fn test_search_patterns() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo, event_bus);
        
        // Store some patterns
        let sig1 = ErrorSignature::new("ImportError".to_string(), "requests");
        service.store_pattern(None, sig1, "pip install requests".to_string(), "dep".to_string(), vec![1.0, 0.0, 0.0]).await.unwrap();
        
        let sig2 = ErrorSignature::new("ImportError".to_string(), "numpy");
        service.store_pattern(None, sig2, "pip install numpy".to_string(), "dep".to_string(), vec![0.0, 1.0, 0.0]).await.unwrap();
        
        // Search
        let results = service.search_patterns(vec![0.9, 0.1, 0.0], 1).await.unwrap();
        
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].solution_code, "pip install requests");
    }
    
    #[tokio::test]
    async fn test_update_success_score() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo.clone(), event_bus.clone());
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let id = service.store_pattern(
            None,
            signature,
            "pip install requests".to_string(),
            "dep".to_string(),
            vec![0.1],
        ).await.unwrap();
        
        // Update success score
        service.update_pattern_success(id, None, 0.9).await.unwrap();
        
        let pattern = repo.find_by_id(id).await.unwrap().unwrap();
        assert_eq!(pattern.success_score, 0.7); // (0.5 * 1 + 0.9) / 2 = 0.7
        
        // Check event
        let events = event_bus.get_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type(), "pattern_success_updated");
    }
    
    #[tokio::test]
    async fn test_apply_dopamine() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo.clone(), event_bus.clone());
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let id = service.store_pattern(
            None,
            signature,
            "pip install requests".to_string(),
            "dep".to_string(),
            vec![0.1],
        ).await.unwrap();
        
        // Apply dopamine
        service.apply_dopamine(id, None, 0.5).await.unwrap();
        
        let pattern = repo.find_by_id(id).await.unwrap().unwrap();
        assert_eq!(pattern.weight, 1.5);
    }
    
    #[tokio::test]
    async fn test_prune_patterns() {
        let repo = Arc::new(InMemoryPatternRepository::new());
        let event_bus = Arc::new(MockEventBus::new());
        let service = StandardCortexService::new(repo.clone(), event_bus);
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let mut pattern = CortexPattern::new(
            signature,
            "pip install requests".to_string(),
            "dep".to_string(),
        );
        
        // Set low weight
        pattern.weight = 0.05;
        repo.store_pattern(&pattern, vec![0.1]).await.unwrap();
        
        // Prune
        let pruned = service.prune_patterns(0.1, 365).await.unwrap();
        assert_eq!(pruned, 1);
        
        // Verify deleted
        let found = repo.find_by_id(pattern.id).await.unwrap();
        assert!(found.is_none());
    }
}
