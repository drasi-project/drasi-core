use anyhow::{Context, Result};
use async_trait::async_trait;
use qdrant_client::qdrant::{
    point_id::PointIdOptions, points_selector, value::Kind, DeletePoints, GetPoints, PointId,
    PointStruct, PointsSelector, UpsertPoints, Value,
};
use qdrant_client::Qdrant;
use std::collections::HashMap;
use sync_vectorstore_core::{SyncPointStore, VectorItem, VectorStore};
use uuid::Uuid;

pub struct QdrantStore {
    client: Qdrant,
    collection_name: String,
    // A fixed UUID reserved for storing the sync checkpoint
    sync_point_id: PointId,
    // Required to generate dummy vectors for checkpoint updates
    vector_size: u64,
}

impl QdrantStore {
    pub fn new(url: String, collection_name: String, vector_size: u64) -> Result<Self> {
        let client = Qdrant::from_url(&url)
            .build()
            .context("Failed to build Qdrant client")?;

        let sync_uuid = Uuid::new_v5(&Uuid::NAMESPACE_DNS, "drasi-sync-point".as_bytes());

        Ok(Self {
            client,
            collection_name,
            sync_point_id: PointId {
                point_id_options: Some(PointIdOptions::Uuid(sync_uuid.to_string())),
            },
            vector_size,
        })
    }
}

#[async_trait]
impl SyncPointStore for QdrantStore {
    async fn load_sync_point(&self) -> Result<Option<u64>> {
        let request = GetPoints {
            collection_name: self.collection_name.clone(),
            ids: vec![self.sync_point_id.clone()],
            with_payload: Some(true.into()),
            with_vectors: Some(false.into()),
            ..Default::default()
        };

        let response = self
            .client
            .get_points(request)
            .await
            .context("Failed to fetch sync point from Qdrant")?;

        if let Some(point) = response.result.first() {
            if let Some(Kind::IntegerValue(seq)) =
                point.payload.get("sequence").and_then(|v| v.kind.as_ref())
            {
                return Ok(Some(*seq as u64));
            }
        }
        Ok(None)
    }

    async fn save_sync_point(&self, sequence: u64) -> Result<()> {
        let mut payload = HashMap::new();
        payload.insert(
            "sequence".to_string(),
            Value {
                kind: Some(Kind::IntegerValue(sequence as i64)),
            },
        );

        let dummy_vector = vec![0.0; self.vector_size as usize];

        let point = PointStruct {
            id: Some(self.sync_point_id.clone()),
            payload,
            vectors: Some(dummy_vector.into()),
        };

        let request = UpsertPoints {
            collection_name: self.collection_name.clone(),
            points: vec![point],
            ..Default::default()
        };

        self.client
            .upsert_points(request)
            .await
            .context("Failed to save sync point to Qdrant")?;

        Ok(())
    }
}

#[async_trait]
impl VectorStore for QdrantStore {
    async fn upsert_vectors(&self, vectors: Vec<VectorItem>) -> Result<()> {
        if vectors.is_empty() {
            return Ok(());
        }

        let points: Vec<PointStruct> = vectors
            .into_iter()
            .map(|v| {
                let uuid_id = Uuid::new_v5(&Uuid::NAMESPACE_DNS, v.id.as_bytes()).to_string();

                let point_id = PointId {
                    point_id_options: Some(PointIdOptions::Uuid(uuid_id)),
                };

                let payload: HashMap<String, Value> =
                    serde_json::from_value(v.payload).unwrap_or_default();

                PointStruct {
                    id: Some(point_id),
                    vectors: Some(v.vector.into()),
                    payload,
                }
            })
            .collect();

        let request = UpsertPoints {
            collection_name: self.collection_name.clone(),
            points,
            ..Default::default()
        };

        self.client
            .upsert_points(request)
            .await
            .context("Failed to upsert vector batch to Qdrant")?;

        Ok(())
    }

    async fn delete_vectors(&self, ids: Vec<String>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        let points_ids: Vec<PointId> = ids
            .into_iter()
            .map(|id| {
                let uuid_id = Uuid::new_v5(&Uuid::NAMESPACE_DNS, id.as_bytes()).to_string();
                PointId {
                    point_id_options: Some(PointIdOptions::Uuid(uuid_id)),
                }
            })
            .collect();

        let request = DeletePoints {
            collection_name: self.collection_name.clone(),
            points: Some(PointsSelector {
                points_selector_one_of: Some(points_selector::PointsSelectorOneOf::Points(
                    qdrant_client::qdrant::PointsIdsList { ids: points_ids },
                )),
            }),
            ..Default::default()
        };

        self.client
            .delete_points(request)
            .await
            .context("Failed to delete vector batch from Qdrant")?;

        Ok(())
    }
}
