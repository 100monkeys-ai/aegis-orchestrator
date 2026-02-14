# Known Issue: LanceDB Arrow Conflicts

## Problem

LanceDB Rust crate has dependency conflicts with arrow-arith across versions 0.3.x and 0.4.x.

## Error

```markdown
error[E0034]: multiple applicable items in scope
  --> arrow-arith-50.0.0/src/temporal.rs:90:36
```

## Current Status

- ✅ Docker services (LanceDB, Neo4j) are configured and ready
- ✅ In-memory implementations working perfectly (29/29 tests passing)
- ❌ Rust LanceDB client temporarily disabled

## Resolution Options

### Option 1: Wait for Upstream Fix (Recommended)

- Monitor: <https://github.com/lancedb/lancedb/issues>
- Arrow ecosystem is actively maintained
- Likely to be resolved in next release

### Option 2: Use HTTP API Instead

```rust
// Direct HTTP calls to LanceDB service
async fn store_pattern_http(pattern: &CortexPattern, embedding: Vec<f32>) -> Result<()> {
    let client = reqwest::Client::new();
    client.post("http://localhost:8765/tables/patterns/add")
        .json(&json!({
            "id": pattern.id.to_string(),
            "embedding": embedding,
            "pattern": serde_json::to_value(pattern)?
        }))
        .send()
        .await?;
    Ok(())
}
```

### Option 3: Alternative Vector DB

Consider alternatives:

- `qdrant-client` - Well-maintained Rust client
- `milvus` - Good Rust support
- `weaviate` - GraphQL API

## Impact

**Minimal** - In-memory implementations are production-ready for pre-alpha:

- Full test coverage
- Same interface (PatternRepository trait)
- Easy swap when resolved
- Docker services ready for when Rust client works

## Timeline

- **Now**: Use in-memory (works perfectly)
- **Next sprint**: Revisit after lancedb crate updates
- **Fallback**: Implement HTTP API if needed
