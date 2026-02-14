# Cortex Docker Stack

## Services

### 1. Embedding Service

- **Port**: 50052 (gRPC)
- **Model**: all-MiniLM-L6-v2 (384-dim)
- **Purpose**: Generate semantic embeddings for pattern storage

### 2. LanceDB

- **Port**: 8765
- **Purpose**: Vector store for pattern similarity search
- **Storage**: `/data` volume

### 3. Neo4j

- **Ports**:
  - 7474 (HTTP/UI)
  - 7687 (Bolt)
- **Purpose**: Knowledge graph for structural reasoning
- **Auth**: neo4j / aegis_dev_password
- **UI**: <http://localhost:7474>

## Usage

```bash
# Start all services
cd docker
docker-compose up -d

# View logs
docker-compose logs -f embedding-service
docker-compose logs -f lancedb
docker-compose logs -f neo4j

# Stop services
docker-compose down

# Reset data (WARNING: deletes all data)
docker-compose down -v
```

## Health Checks

```bash
# Embedding service
grpcurl -plaintext localhost:50052 embedding.EmbeddingService/HealthCheck

# LanceDB
curl http://localhost:8765/health

# Neo4j
cypher-shell -u neo4j -p aegis_dev_password "RETURN 1"
```

## Configuration

### Embedding Service

- `MODEL_NAME`: Sentence transformer model (default: all-MiniLM-L6-v2)

### LanceDB

- `LANCE_DB_PATH`: Data directory (default: /data)

### Neo4j

- `NEO4J_AUTH`: Username/password
- `NEO4J_PLUGINS`: Enabled plugins (APOC for graph algorithms)
- Heap size: 512MB initial, 2GB max
