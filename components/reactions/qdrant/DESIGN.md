# Qdrant Reaction - Architecture & Design

This document describes the architecture of the Qdrant reaction. For usage documentation, see [README.md](./README.md).

## Current Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           QdrantReaction                                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌────────────────┐    ┌────────────────┐    ┌────────────────┐         │
│  │    Embedder    │    │   Handlebars   │    │   Document     │         │
│  │     Trait      │    │   Templating   │    │    Builder     │         │
│  └────────────────┘    └────────────────┘    └────────────────┘         │
│         │                      │                      │                 │
│  ┌──────┴───────┐              │               ┌──────┴───────┐         │
│  │ MockEmbedder │              │               │  UUID Gen    │         │
│  │ AzureOpenAI  │              │               │ (RFC 4122)   │         │
│  └──────────────┘              │               └──────────────┘         │
│                                │                                        │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                    Sync Protocol (start())                       │   │
│  │   Subscribe → Snapshot → Batch Materialize → Process Loop        │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                │                                        │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                      Batch Processing                            │   │
│  │   ┌────────────────────┐      ┌────────────────────┐             │   │
│  │   │ Embedding Batches  │      │  Upsert Batches    │             │   │
│  │   │ (configurable size)│  →   │ (configurable size)│             │   │
│  │   └────────────────────┘      └────────────────────┘             │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                │                                        │
│                                ▼                                        │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                       Qdrant Client                              │   │
│  │                 (upsert / delete points)                         │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Implemented Features

### Embedder Trait
- **MockEmbedder**: SHA-256 based deterministic embeddings for testing
- **AzureOpenAIEmbedder**: Production embeddings via Azure OpenAI API
- Extensible trait allows custom embedder implementations

### Batch Processing
- **Embedding Batches**: Multiple documents embedded in single API call
  - Configurable via `BatchConfig.embedding_batch_size` (default: 50)
- **Upsert Batches**: Multiple points written in single Qdrant call
  - Configurable via `BatchConfig.upsert_batch_size` (default: 100)

### Materialized View Sync Protocol
On startup, the reaction follows the drasi-lib synchronization protocol:
1. **Subscribe** - Subscribes to queries, buffering incoming events
2. **Snapshot** - Calls `get_current_results()` to load existing data
3. **Materialize** - Batch upserts snapshot data to Qdrant
4. **Process** - Processes incremental updates from event buffer

This ensures consistent initial state before processing changes.

### Deterministic UUIDs
- RFC 4122 UUID v5 generation from `(collection_name, key_value)` namespace
- Same key always produces same UUID across restarts
- Enables idempotent upsert operations

### Document Building
- Handlebars templating for content generation
- `json` helper for nested object serialization
- Supports nested field paths (e.g., `product.category.name`)

## Known Limitations

### DrasiQuery Dependency for Snapshot Loading

The snapshot loading phase depends on downcasting the query instance to `DrasiQuery`:

```rust
if let Some(drasi_query) = query.as_any().downcast_ref::<DrasiQuery>() {
    let current_results = drasi_query.get_current_results().await;
    // ...
}
```

**Implications:**
- If future versions of Drasi introduce other `Query` implementations (e.g., proxied queries, federated queries), this cast will fail
- When the cast fails, the reaction logs a warning and skips snapshot loading for that query
- The reaction will still function for incremental updates, but will start with an empty initial state

**Mitigation:**
- Consider adding `get_current_results()` to the `Query` trait in drasi-lib
- Alternatively, use a trait object pattern with a `Snapshotable` trait

## Future Improvements

### Embedder Extraction to Shared Crate

Extract the `Embedder` trait and implementations to a shared `drasi-embeddings` crate for reuse across multiple vector store reactions.

**Proposed structure:**
```
components/shared/embeddings/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Public exports
│   ├── traits.rs        # Embedder trait
│   ├── azure_openai.rs  # AzureOpenAIEmbedder
│   ├── openai.rs        # OpenAI (non-Azure)
│   ├── mock.rs          # MockEmbedder
│   └── cache.rs         # Optional caching layer
```

**Benefits:**
- Reuse across Qdrant, Redis, pgvector reactions
- Centralized embedder configuration
- Shared caching and rate limiting
- Consistent API across vector stores

### Additional Embedder Providers
- OpenAI (non-Azure)
- Ollama (local models)
- Hugging Face Inference API
- Cohere embeddings

### Embedding Cache
- Optional caching layer to avoid re-embedding identical content
- LRU cache with configurable size
- Content hash as cache key

### Metadata Filtering
- Selective field storage in Qdrant payload
- Filter expressions for payload fields
- Index configuration for filtered search

### Multi-Collection Support
- Route different queries to different collections
- Per-query collection configuration
- Cross-collection search coordination
