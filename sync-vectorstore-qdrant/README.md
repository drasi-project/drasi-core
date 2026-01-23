# Qdrant Vector Store Reaction

A Drasi reaction implementation that synchronizes change streams to a Qdrant Vector Database in real-time.

## Features

- **Deterministic IDs:** Automatically hashes arbitrary Drasi string IDs into UUID v5 to comply with Qdrant's strict ID requirements.
- **Strict Vector Sizing:** Handles checkpointing by managing dummy vectors to comply with collection configuration.
- **Resiliency:** Automatically recovers from the last successfully synced sequence number on startup.
- **Performance:** Uses gRPC (port 6334) for high-performance data transfer.

## Configuration

When using this reaction, ensure your Qdrant instance is running and accessible.

- **URL:** The gRPC endpoint (e.g., `http://localhost:6334`).
- **Collection:** The name of the target collection.
- **Vector Size:** The dimension of the embeddings (must match your model).

## Development

Run the integration demo (requires Docker):

```bash
cargo run -p sync-vectorstore-qdrant --example demo_connect