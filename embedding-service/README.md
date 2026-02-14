# Embedding Service for Cortex

Python service using sentence-transformers to generate semantic embeddings for pattern storage.

## Architecture

- **Model**: `all-MiniLM-L6-v2` (384-dim, fast, good quality)
- **Interface**: gRPC for Rust integration
- **Deployment**: Docker container in local stack

## API

```protobuf
service EmbeddingService {
  rpc GenerateEmbedding(EmbeddingRequest) returns (EmbeddingResponse);
  rpc GenerateBatch(BatchEmbeddingRequest) returns (BatchEmbeddingResponse);
}

message EmbeddingRequest {
  string text = 1;
}

message EmbeddingResponse {
  repeated float embedding = 1;
  int32 dimension = 2;
}
```

## Usage

```rust
let embedding = embedding_client
    .generate_embedding(format!("{} {}", error, solution))
    .await?;
```

## Dependencies

- sentence-transformers
- grpcio
- torch (CPU only for local dev)
