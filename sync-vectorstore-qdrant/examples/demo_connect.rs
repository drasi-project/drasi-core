// Telling Clippy to ignore "print" errors for this demo file
#![allow(clippy::print_stdout)]
#![allow(clippy::uninlined_format_args)]

use anyhow::Result;
use async_trait::async_trait;
use qdrant_client::qdrant::{CreateCollection, Distance, VectorParams};
use qdrant_client::Qdrant;
use serde_json::json;
use sync_vectorstore_core::{
    Change, ChangeProcessor, EmbeddingService, SyncPointStore, VectorStore,
};
use sync_vectorstore_qdrant::QdrantStore;

// MOCK EMBEDDING SERVICE (For Demo Purposes)
// This allows us to run the example without needing a real OpenAI Key.
// It proves the architecture works.
struct MockEmbeddingService;

#[async_trait]
impl EmbeddingService for MockEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        println!("Generating embedding for: '{}'", text);
        // Return a fixed vector of size 4 (matches our collection config)
        Ok(vec![0.1, 0.2, 0.3, 0.4])
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = "http://localhost:6334".to_string();
    let collection = "demo_rag_collection".to_string();
    let vector_size = 4;

    println!("--- Starting Real-Time RAG Demo ---");

    let client = Qdrant::from_url(&url).build()?;
    if !client.collection_exists(&collection).await? {
        println!("Creating collection '{collection}'...");
        client
            .create_collection(CreateCollection {
                collection_name: collection.clone(),
                vectors_config: Some(
                    qdrant_client::qdrant::vectors_config::Config::Params(VectorParams {
                        size: vector_size,
                        distance: Distance::Cosine.into(),
                        ..Default::default()
                    })
                    .into(),
                ),
                ..Default::default()
            })
            .await?;
    }

    let store = QdrantStore::new(url, collection.clone(), vector_size)?;
    let embedder = MockEmbeddingService;

    let template = "Article Title: {{title}} | Category: {{category}}";
    let processor = ChangeProcessor::new(embedder, store, template);

    println!("Components Initialized Successfully!");

    println!("Processing incoming change batch...");

    let change = Change::Insert {
        id: "article-101".to_string(), // Simple ID (Will be UUID hashed by QdrantStore)
        seq: 500,
        data: json!({
            "title": "Future of Rust",
            "category": "Technology",
            "views": 1000
        }),
    };

    processor.process_batch(vec![change]).await?;
    println!("Batch Processed Successfully!");

    let verify_store =
        QdrantStore::new("http://localhost:6334".to_string(), collection, vector_size)?;
    let loaded_seq = verify_store.load_sync_point().await?;

    println!("Loaded Sync Point from Qdrant: {loaded_seq:?}");

    assert_eq!(loaded_seq, Some(500));
    println!("Verification Complete: Pipeline is working End-to-End!");

    Ok(())
}
