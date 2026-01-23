pub mod openai;

use anyhow::{Context, Result};
use async_trait::async_trait;
use handlebars::Handlebars;
use serde_json::Value;

#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

#[async_trait]
pub trait SyncPointStore: Send + Sync {
    async fn load_sync_point(&self) -> Result<Option<u64>>;
    async fn save_sync_point(&self, sequence: u64) -> Result<()>;
}

#[async_trait]
pub trait VectorStore: SyncPointStore {
    async fn upsert_vectors(&self, vectors: Vec<VectorItem>) -> Result<()>;
    async fn delete_vectors(&self, ids: Vec<String>) -> Result<()>;
}

pub struct VectorItem {
    pub id: String,
    pub vector: Vec<f32>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum Change {
    Insert { id: String, data: Value, seq: u64 },
    Update { id: String, data: Value, seq: u64 },
    Delete { id: String, seq: u64 },
}

pub struct ChangeProcessor<E, S> {
    embedding_service: E,
    store: S,
    registry: Handlebars<'static>,
    template_name: String,
}

impl<E, S> ChangeProcessor<E, S>
where
    E: EmbeddingService,
    S: VectorStore,
{
    pub fn new(embedding_service: E, store: S, template_str: &str) -> Self {
        let mut registry = Handlebars::new();
        registry.set_strict_mode(false);

        registry
            .register_template_string("item_template", template_str)
            .expect("Failed to register Handlebars template. Please check your template syntax.");

        Self {
            embedding_service,
            store,
            registry,
            template_name: "item_template".to_string(),
        }
    }

    pub async fn process_batch(&self, changes: Vec<Change>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }

        let mut upsert_batch = Vec::new();
        let mut delete_batch = Vec::new();
        let mut max_seq = 0;

        for change in changes {
            match change {
                Change::Insert { id, data, seq } | Change::Update { id, data, seq } => {
                    if seq > max_seq {
                        max_seq = seq;
                    }

                    let text = self
                        .registry
                        .render(&self.template_name, &data)
                        .context(format!("Failed to render template for ID: {id}"))?;

                    let vector = self
                        .embedding_service
                        .embed(&text)
                        .await
                        .context(format!("Failed to embed text for ID: {id}"))?;

                    upsert_batch.push(VectorItem {
                        id,
                        vector,
                        payload: data,
                    });
                }
                Change::Delete { id, seq } => {
                    if seq > max_seq {
                        max_seq = seq;
                    }
                    delete_batch.push(id);
                }
            }
        }

        if !upsert_batch.is_empty() {
            self.store.upsert_vectors(upsert_batch).await?;
        }

        if !delete_batch.is_empty() {
            self.store.delete_vectors(delete_batch).await?;
        }

        if max_seq > 0 {
            self.store.save_sync_point(max_seq).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct MockEmbedding;
    #[async_trait]
    impl EmbeddingService for MockEmbedding {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1, 0.2, 0.3])
        }
    }

    struct MockStore {
        saved_vectors: Arc<Mutex<Vec<VectorItem>>>,
        last_seq: Arc<Mutex<u64>>,
    }

    #[async_trait]
    impl SyncPointStore for MockStore {
        async fn load_sync_point(&self) -> Result<Option<u64>> {
            Ok(Some(0))
        }
        async fn save_sync_point(&self, sequence: u64) -> Result<()> {
            *self.last_seq.lock().unwrap() = sequence;
            Ok(())
        }
    }

    #[async_trait]
    impl VectorStore for MockStore {
        async fn upsert_vectors(&self, vectors: Vec<VectorItem>) -> Result<()> {
            let mut store = self.saved_vectors.lock().unwrap();
            store.extend(vectors);
            Ok(())
        }
        async fn delete_vectors(&self, _ids: Vec<String>) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_process_batch_flow() {
        let saved_data = Arc::new(Mutex::new(Vec::new()));
        let last_seq = Arc::new(Mutex::new(0));

        let store = MockStore {
            saved_vectors: saved_data.clone(),
            last_seq: last_seq.clone(),
        };

        let processor = ChangeProcessor::new(MockEmbedding, store, "Description: {{description}}");

        let changes = vec![Change::Insert {
            id: "doc-1".to_string(),
            data: serde_json::json!({ "description": "Hello Rust" }),
            seq: 10,
        }];

        processor.process_batch(changes).await.unwrap();

        let vectors = saved_data.lock().unwrap();
        assert_eq!(vectors.len(), 1, "One vector should have been saved.");
        assert_eq!(vectors[0].id, "doc-1");

        let seq = *last_seq.lock().unwrap();
        assert_eq!(seq, 10, "It should have updated the sequence number.");
    }
}
