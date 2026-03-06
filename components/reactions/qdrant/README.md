# Qdrant Reaction

A vector database reaction that synchronizes Drasi query results to Qdrant for semantic search and retrieval-augmented generation (RAG) applications.

## Overview

The Qdrant Reaction generates embeddings from query result data and stores them as searchable vector points in a Qdrant collection. It enables semantic search over your continuous query results, making it ideal for building AI-powered search, recommendation systems, and RAG pipelines.

**Output Method**: Uses gRPC to communicate with Qdrant, supporting both local instances and Qdrant Cloud.

### Key Capabilities

- **Automatic Embedding Generation**: Generates embeddings using Azure OpenAI or mock embedders
- **Template-Based Content**: Uses Handlebars templates to construct document content from query results
- **Deterministic Point IDs**: RFC 4122 UUID v5 ensures consistent point IDs for upsert/delete operations
- **Collection Management**: Optionally creates collections with correct vector dimensions
- **Full Lifecycle Support**: Handles ADD, UPDATE, and DELETE operations from continuous queries

### Use Cases

- **Semantic Search**: Enable natural language search over structured data
- **RAG Applications**: Build retrieval-augmented generation pipelines
- **Recommendation Systems**: Find similar items based on content
- **Knowledge Bases**: Create searchable document repositories from query results
- **Content Discovery**: Surface related content based on semantic similarity

**Best for**: Applications requiring semantic search over continuously updated data.

**Not recommended for**: Scenarios where exact keyword matching is sufficient.

### Synchronization Protocol

The reaction follows the drasi-lib materialized view protocol:

1. **Subscribe** - Subscribes to configured queries, buffering incoming events
2. **Snapshot** - Loads existing query results via `get_current_results()`
3. **Materialize** - Batch upserts snapshot data to Qdrant collection
4. **Process** - Processes incremental updates from the event buffer

This ensures the reaction starts with a consistent view of all existing data before processing incremental changes. Progress is logged for large datasets (> 1000 items).

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent, type-safe API for creating QdrantReaction instances.

#### With MockEmbedder (Testing)

```rust
use drasi_reaction_qdrant::{
    QdrantReaction, QdrantReactionConfig, QdrantConfig,
    EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, MockEmbedder,
};
use std::sync::Arc;

let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "http://localhost:6334".to_string(),
        api_key: None,
        collection_name: "my_documents".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::Mock { dimensions: 1536 },
    document: DocumentConfig {
        document_template: "{{name}}: {{description}}".to_string(),
        key_field: "id".to_string(),
        ..Default::default()
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};

let embedder = Arc::new(MockEmbedder::new(1536));
let reaction = QdrantReaction::with_embedder(
    "my-qdrant-reaction",
    vec!["product-query".to_string()],
    config,
    embedder,
)?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

#### With AzureOpenAIEmbedder (Production)

```rust
use drasi_reaction_qdrant::{
    QdrantReaction, QdrantReactionConfig, QdrantConfig,
    EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, AzureOpenAIEmbedder,
};
use std::sync::Arc;

let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "http://localhost:6334".to_string(),
        api_key: None,
        collection_name: "products".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::AzureOpenai {
        endpoint: "https://myresource.openai.azure.com".to_string(),
        model: "text-embedding-3-large".to_string(),
        api_key: "your-api-key".to_string(),
        dimensions: 3072,
        api_version: "2024-02-01".to_string(),
    },
    document: DocumentConfig {
        document_template: "Product: {{name}}. Description: {{description}}. Category: {{category}}".to_string(),
        title_template: Some("{{name}}".to_string()),
        key_field: "product_id".to_string(),
        metadata_fields: None,
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};

let embedder = Arc::new(AzureOpenAIEmbedder::new(
    "https://myresource.openai.azure.com".to_string(),
    "text-embedding-3-large".to_string(),
    "your-api-key".to_string(),
    3072,
    "2024-02-01".to_string(),
));

let reaction = QdrantReaction::with_embedder(
    "product-search",
    vec!["product-updates".to_string()],
    config,
    embedder,
)?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Config Struct Approach

For programmatic configuration or deserialization scenarios:

```rust
use drasi_reaction_qdrant::{
    QdrantReaction, QdrantReactionConfig, QdrantConfig,
    EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, MockEmbedder,
};
use std::sync::Arc;

let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "http://localhost:6334".to_string(),
        api_key: None,
        collection_name: "test_collection".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::Mock { dimensions: 768 },
    document: DocumentConfig {
        document_template: "{{content}}".to_string(),
        title_template: None,
        key_field: "id".to_string(),
        metadata_fields: None,
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};

let embedder = Arc::new(MockEmbedder::new(768));
let reaction = QdrantReaction::with_embedder(
    "test-reaction",
    vec!["test-query".to_string()],
    config,
    embedder,
)?;
```

## Configuration Options

### Core Options

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `id` | Unique identifier for the reaction | `String` | **(Required)** |
| `queries` | Query IDs to subscribe to | `Vec<String>` | **(Required)** |
| `auto_start` | Start automatically when added | `bool` | `true` |
| `priority_queue_capacity` | Maximum events in queue | `usize` | `10000` |

### Qdrant Options (`QdrantConfig`)

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `endpoint` | Qdrant gRPC endpoint URL | `String` | **(Required)** |
| `api_key` | API key for Qdrant Cloud | `Option<String>` | `None` |
| `collection_name` | Target collection name | `String` | **(Required)** |
| `create_collection` | Create collection if missing | `bool` | `true` |

### Embedding Options (`EmbeddingConfig`)

`EmbeddingConfig` is a tagged enum with variants for each embedding provider. Each variant contains only the fields relevant to that provider, providing compile-time enforcement of required fields.

#### Mock Variant

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `dimensions` | Embedding vector dimensions | `usize` | `3072` |

#### AzureOpenai Variant

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `endpoint` | Azure OpenAI endpoint | `String` | **(Required)** |
| `model` | Model deployment name | `String` | **(Required)** |
| `api_key` | Azure OpenAI API key | `String` | **(Required)** |
| `dimensions` | Embedding vector dimensions | `usize` | `3072` |
| `api_version` | Azure OpenAI API version | `String` | `"2024-02-01"` |

### Document Options (`DocumentConfig`)

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `document_template` | Handlebars template for content | `String` | **(Required)** |
| `title_template` | Optional title template | `Option<String>` | `None` |
| `key_field` | Field path for point ID generation | `String` | **(Required)** |
| `metadata_fields` | Fields to include in payload | `Option<Vec<String>>` | `None` (all fields) |

### Batch Options (`BatchConfig`)

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `embedding_batch_size` | Max documents per embedding API call | `usize` | `50` |
| `upsert_batch_size` | Max points per Qdrant upsert call | `usize` | `100` |

### Retry Options (`RetryConfig`)

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `max_retries` | Maximum retry attempts before failure | `u32` | `3` |
| `initial_delay_ms` | Initial delay between retries (ms) | `u64` | `100` |
| `max_delay_ms` | Maximum delay between retries (ms) | `u64` | `5000` |

Retry uses exponential backoff with jitter. On persistent failure:
- **Snapshot loading**: Fails startup (prevents inconsistent state)
- **Stream processing**: Panics (forces pod restart for zero data loss)

## Template Variables

Document templates have access to all fields from the query result data.

### Available Variables

For ADD and UPDATE events:
- All fields from `after` data (e.g., `{{name}}`, `{{description}}`, `{{price}}`)
- Nested fields using dot notation (e.g., `{{product.category.name}}`)

For DELETE events:
- All fields from `before` data

### Template Helpers

The `json` helper converts values to JSON strings:

```handlebars
Full object: {{json this}}
Nested field: {{json metadata}}
```

### Example Templates

**Simple product:**
```handlebars
{{name}}: {{description}}
```

**Rich product with metadata:**
```handlebars
Product: {{name}}
Category: {{category}}
Description: {{description}}
Price: ${{price}}
Tags: {{json tags}}
```

**Article with structured content:**
```handlebars
Title: {{title}}
Author: {{author.name}}
Summary: {{summary}}
Full Content: {{body}}
```

## Usage Examples

### Example 1: Basic Testing Setup

Simple setup with mock embeddings for development:

```rust
use drasi_reaction_qdrant::{QdrantReaction, QdrantReactionConfig, QdrantConfig, EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, MockEmbedder};
use std::sync::Arc;

let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "http://localhost:6334".to_string(),
        api_key: None,
        collection_name: "dev_collection".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::Mock { dimensions: 384 },
    document: DocumentConfig {
        document_template: "{{title}}: {{content}}".to_string(),
        key_field: "id".to_string(),
        ..Default::default()
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};

let embedder = Arc::new(MockEmbedder::new(384));
let reaction = QdrantReaction::with_embedder(
    "dev-search",
    vec!["articles".to_string()],
    config,
    embedder,
)?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Example 2: Production Azure OpenAI

Full production setup with Azure OpenAI embeddings:

```rust
use drasi_reaction_qdrant::{QdrantReaction, QdrantReactionConfig, QdrantConfig, EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, AzureOpenAIEmbedder};
use std::sync::Arc;

let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "https://my-qdrant.cloud:6333".to_string(),
        api_key: Some("qdrant-api-key".to_string()),
        collection_name: "production_docs".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::AzureOpenai {
        endpoint: "https://myresource.openai.azure.com".to_string(),
        model: "text-embedding-3-large".to_string(),
        api_key: "azure-api-key".to_string(),
        dimensions: 3072,
        api_version: "2024-02-01".to_string(),
    },
    document: DocumentConfig {
        document_template: "{{title}}\n\n{{summary}}\n\n{{body}}".to_string(),
        title_template: Some("{{title}}".to_string()),
        key_field: "document_id".to_string(),
        metadata_fields: None,
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};

let embedder = Arc::new(AzureOpenAIEmbedder::new(
    "https://myresource.openai.azure.com".to_string(),
    "text-embedding-3-large".to_string(),
    "azure-api-key".to_string(),
    3072,
    "2024-02-01".to_string(),
));

let reaction = QdrantReaction::with_embedder(
    "doc-search",
    vec!["document-updates".to_string()],
    config,
    embedder,
)?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Example 3: E-commerce Product Search

Optimized for product catalog search:

```rust
let config = QdrantReactionConfig {
    qdrant: QdrantConfig {
        endpoint: "http://localhost:6334".to_string(),
        api_key: None,
        collection_name: "products".to_string(),
        create_collection: true,
    },
    embedding: EmbeddingConfig::AzureOpenai {
        endpoint: "https://myresource.openai.azure.com".to_string(),
        model: "text-embedding-ada-002".to_string(),
        api_key: "your-key".to_string(),
        dimensions: 1536,
        api_version: "2024-02-01".to_string(),
    },
    document: DocumentConfig {
        // Rich product description for semantic search
        document_template: r#"
Product: {{name}}
Brand: {{brand}}
Category: {{category}} > {{subcategory}}
Description: {{description}}
Features: {{json features}}
Price: ${{price}}
        "#.trim().to_string(),
        title_template: Some("{{name}} by {{brand}}".to_string()),
        key_field: "sku".to_string(),
        metadata_fields: None,
    },
    batch: BatchConfig::default(),
    retry: RetryConfig::default(),
};
```

### Example 4: Multi-Query Monitoring

Subscribe to multiple queries:

```rust
let reaction = QdrantReaction::with_embedder(
    "unified-search",
    vec![
        "product-updates".to_string(),
        "article-updates".to_string(),
        "faq-updates".to_string(),
    ],
    config,
    embedder,
)?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Example 5: Custom Embedder Implementation

Implement the `Embedder` trait for custom providers:

```rust
use drasi_reaction_qdrant::Embedder;
use anyhow::Result;
use async_trait::async_trait;

pub struct MyCustomEmbedder {
    dimensions: usize,
    // ... your configuration
}

#[async_trait]
impl Embedder for MyCustomEmbedder {
    async fn generate(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // Call your embedding service here
        let embeddings = texts.iter()
            .map(|text| {
                // Generate embedding for text
                vec![0.0f32; self.dimensions] // placeholder
            })
            .collect();
        Ok(embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "my_custom_embedder"
    }
}

// Use with QdrantReaction
let embedder = Arc::new(MyCustomEmbedder { dimensions: 768 });
let reaction = QdrantReaction::with_embedder(
    "custom-search",
    vec!["my-query".to_string()],
    config,
    embedder,
)?;
```

## Embedder Implementations

### MockEmbedder

Deterministic SHA-256 based embeddings for testing. Produces consistent embeddings without external API calls.

```rust
use drasi_reaction_qdrant::MockEmbedder;

let embedder = MockEmbedder::new(1536); // 1536 dimensions

// Same text always produces same embedding
let emb1 = embedder.generate(&["hello".to_string()]).await?;
let emb2 = embedder.generate(&["hello".to_string()]).await?;
assert_eq!(emb1, emb2);
```

**Use for**: Unit tests, integration tests, development, demos.

### AzureOpenAIEmbedder

Production embeddings via Azure OpenAI API.

```rust
use drasi_reaction_qdrant::AzureOpenAIEmbedder;

let embedder = AzureOpenAIEmbedder::new(
    "https://myresource.openai.azure.com".to_string(),
    "text-embedding-3-large".to_string(),  // deployment name
    "your-api-key".to_string(),
    3072,  // dimensions for text-embedding-3-large
    "2024-02-01".to_string(),  // API version
);
```

**Supported models**:
- `text-embedding-3-large` (3072 dimensions)
- `text-embedding-3-small` (1536 dimensions)
- `text-embedding-ada-002` (1536 dimensions)

**Use for**: Production deployments with Azure OpenAI.

## Performance Considerations

### Embedding Latency

| Embedder | Latency | Notes |
|----------|---------|-------|
| MockEmbedder | < 1ms | In-process, no network |
| AzureOpenAIEmbedder | 50-200ms | Network dependent |

### Throughput Guidelines

| Scenario | Events/Sec | Recommendation |
|----------|-----------|----------------|
| Low Volume | < 10 | Default configuration |
| Medium Volume | 10-50 | Increase queue capacity |
| High Volume | > 50 | Consider batching, use dedicated embedder instances |

### Memory Usage

- **Priority Queue**: Buffers up to `priority_queue_capacity` events
- **Embedding Cache**: No caching (each document generates new embedding)
- **Qdrant Client**: Maintains persistent gRPC connection

### Tuning Tips

1. **Queue Capacity**: Increase `priority_queue_capacity` for bursty workloads
2. **Embedding Dimensions**: Use smaller models (ada-002) for lower latency
3. **Document Templates**: Keep templates concise to reduce embedding costs
4. **Collection Indexing**: Let Qdrant optimize with `create_collection: true`

## Troubleshooting

### No Points Appearing in Qdrant

**Symptoms**: Reaction runs but collection remains empty

**Solutions**:
1. Verify query is producing results
2. Check reaction is subscribed to correct query IDs
3. Verify Qdrant endpoint is reachable
4. Check collection exists or `create_collection: true`
5. Enable debug logging: `RUST_LOG=debug`

### Template Rendering Errors

**Symptoms**: Points have empty content or errors in logs

**Solutions**:
1. Verify template syntax (Handlebars format)
2. Check field names match query result structure
3. Use `{{json this}}` to inspect available fields
4. Test templates with simple expressions first

### Azure OpenAI Connection Issues

**Symptoms**: "Azure OpenAI API error" in logs

**Solutions**:
1. Verify endpoint URL format: `https://{resource}.openai.azure.com`
2. Check API key is valid and not expired
3. Verify deployment name matches Azure configuration
4. Check API version is supported (try `2024-02-01`)
5. Ensure network allows outbound HTTPS

### Qdrant Connection Failures

**Symptoms**: "Failed to connect to Qdrant" errors

**Solutions**:
1. Verify Qdrant is running: `curl http://localhost:6333/health`
2. Check endpoint URL includes correct port (6334 for gRPC)
3. For Qdrant Cloud, verify API key is set
4. Check firewall/network allows gRPC traffic

## Limitations

1. **Single Embedder**: One embedder per reaction (no per-query embedders)
2. **Cosine Distance Only**: Collections created with cosine similarity
3. **No Metadata Filtering**: All fields stored, no selective filtering yet

For very high-throughput scenarios, consider:
- Multiple reaction instances with different query subscriptions
- Adjusting batch sizes via `BatchConfig` for your workload
- Pre-computed embeddings stored in source data

## License

Copyright 2026 The Drasi Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
