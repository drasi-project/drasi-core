# Sync VectorStore Core

Shared utilities and traits for building Drasi Vector Store Reactions. This crate provides the foundational building blocks for creating Real-Time RAG pipelines.

## Features

- **Idempotency:** Built-in `SyncPointStore` trait to track sequence numbers and ensure zero data loss or duplication.
- **Pluggable Embeddings:** `EmbeddingService` trait with out-of-the-box support for:
  - OpenAI (Standard)
  - Azure OpenAI
- **Flexible Templating:** Uses **Handlebars** to transform raw JSON change events into text for embedding generation.
- **Batch Processing:** `ChangeProcessor` handles efficient batching of Insert, Update, and Delete operations.

## Usage

This crate is intended to be used by specific vector store implementations (e.g., Qdrant, Pinecone).