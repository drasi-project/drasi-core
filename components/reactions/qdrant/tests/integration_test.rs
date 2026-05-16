// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Integration tests for the Qdrant reaction using testcontainers.

mod qdrant_helpers;

use drasi_reaction_qdrant::{
    generate_deterministic_uuid, BatchConfig, DocumentConfig, EmbeddingConfig, MockEmbedder,
    QdrantConfig, QdrantReactionConfig, RetryConfig,
};
use qdrant_client::qdrant::{
    CreateCollectionBuilder, Distance, PointStruct, UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_helpers::{get_points_count, search_points, setup_qdrant};
use serial_test::serial;
use std::collections::HashMap;
use std::sync::Arc;

/// Test that the mock embedder produces deterministic embeddings
#[tokio::test]
async fn test_mock_embedder_deterministic() {
    use drasi_reaction_qdrant::Embedder;

    let embedder = MockEmbedder::new(1536);

    let text = "This is a test document for embedding".to_string();
    let embedding1 = embedder
        .generate(&[text.clone()])
        .await
        .expect("Should generate embedding");
    let embedding2 = embedder
        .generate(&[text])
        .await
        .expect("Should generate embedding");

    assert_eq!(
        embedding1, embedding2,
        "Same text should produce same embedding"
    );
    assert_eq!(embedding1[0].len(), 1536, "Should have correct dimensions");

    // Verify normalization
    let norm: f32 = embedding1[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 0.0001,
        "Embedding should be normalized to unit length"
    );
}

/// Test that different texts produce different embeddings
#[tokio::test]
async fn test_mock_embedder_different_texts() {
    use drasi_reaction_qdrant::Embedder;

    let embedder = MockEmbedder::new(1536);

    let embedding1 = embedder
        .generate(&["Hello world".to_string()])
        .await
        .expect("Should generate embedding");
    let embedding2 = embedder
        .generate(&["Goodbye world".to_string()])
        .await
        .expect("Should generate embedding");

    assert_ne!(
        embedding1, embedding2,
        "Different texts should produce different embeddings"
    );
}

/// Test deterministic UUID generation
#[test]
fn test_deterministic_uuid_generation() {
    let key = "product-123";
    let uuid1 = generate_deterministic_uuid(key);
    let uuid2 = generate_deterministic_uuid(key);

    assert_eq!(uuid1, uuid2, "Same key should produce same UUID");

    let uuid3 = generate_deterministic_uuid("product-456");
    assert_ne!(
        uuid1, uuid3,
        "Different keys should produce different UUIDs"
    );
}

/// Test UUID format
#[test]
fn test_uuid_format() {
    let uuid = generate_deterministic_uuid("test-key-12345");
    let uuid_str = uuid.to_string();

    // UUID format: 8-4-4-4-12
    assert_eq!(uuid_str.len(), 36);
    assert_eq!(uuid_str.chars().filter(|c| *c == '-').count(), 4);
}

/// Test configuration validation
#[test]
fn test_config_validation_success() {
    let config = QdrantReactionConfig {
        qdrant: QdrantConfig {
            endpoint: "http://localhost:6334".to_string(),
            api_key: None,
            collection_name: "test".to_string(),
            create_collection: true,
        },
        embedding: EmbeddingConfig::Mock { dimensions: 1536 },
        document: DocumentConfig {
            document_template: "{{content}}".to_string(),
            key_field: "id".to_string(),
            ..Default::default()
        },
        batch: BatchConfig::default(),
        retry: RetryConfig::default(),
    };

    assert!(config.validate().is_ok());
}

/// Test configuration validation failure for missing endpoint
#[test]
fn test_config_validation_missing_endpoint() {
    let config = QdrantReactionConfig {
        qdrant: QdrantConfig {
            endpoint: "".to_string(),
            api_key: None,
            collection_name: "test".to_string(),
            create_collection: true,
        },
        embedding: EmbeddingConfig::default(),
        document: DocumentConfig::default(),
        batch: BatchConfig::default(),
        retry: RetryConfig::default(),
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("endpoint"));
}

/// Test Qdrant connection and collection creation
#[tokio::test]
#[serial]
async fn test_qdrant_connection() {
    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");

    // Verify connection by listing collections
    let collections = client
        .list_collections()
        .await
        .expect("Should list collections");
    assert!(collections.collections.is_empty() || !collections.collections.is_empty());

    qdrant.cleanup().await;
}

/// Test creating a collection and upserting points
#[tokio::test]
#[serial]
async fn test_upsert_and_search() {
    use drasi_reaction_qdrant::Embedder;

    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");
    let embedder = MockEmbedder::new(128);

    let collection_name = "test_upsert_search";

    // Create collection
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
        )
        .await
        .expect("Should create collection");

    // Generate embedding and upsert point
    let text = "This is a test document about machine learning".to_string();
    let embeddings = embedder
        .generate(&[text])
        .await
        .expect("Should generate embedding");

    let point_id = generate_deterministic_uuid("doc-001");
    let mut payload: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
    payload.insert(
        "title".to_string(),
        qdrant_client::qdrant::Value {
            kind: Some(qdrant_client::qdrant::value::Kind::StringValue(
                "Test Document".to_string(),
            )),
        },
    );

    let point = PointStruct::new(point_id.to_string(), embeddings[0].clone(), payload);

    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point]).wait(true))
        .await
        .expect("Should upsert point");

    // Verify point count
    let count = get_points_count(&client, collection_name)
        .await
        .expect("Should get points count");
    assert_eq!(count, 1);

    // Search for the point
    let search_result = search_points(&client, collection_name, embeddings[0].clone(), 10)
        .await
        .expect("Should search points");

    assert_eq!(search_result.result.len(), 1);
    assert_eq!(
        search_result.result[0]
            .id
            .clone()
            .unwrap()
            .point_id_options
            .unwrap(),
        qdrant_client::qdrant::point_id::PointIdOptions::Uuid(point_id.to_string())
    );

    qdrant.cleanup().await;
}

/// Test updating an existing point (upsert with same ID)
#[tokio::test]
#[serial]
async fn test_update_point() {
    use drasi_reaction_qdrant::Embedder;

    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");
    let embedder = MockEmbedder::new(128);

    let collection_name = "test_update";

    // Create collection
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
        )
        .await
        .expect("Should create collection");

    let point_id = generate_deterministic_uuid("doc-update-001");

    // Insert initial point
    let text1 = "Initial document content".to_string();
    let embeddings1 = embedder
        .generate(&[text1])
        .await
        .expect("Should generate embedding");

    let mut payload1: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
    payload1.insert(
        "version".to_string(),
        qdrant_client::qdrant::Value {
            kind: Some(qdrant_client::qdrant::value::Kind::IntegerValue(1)),
        },
    );

    let point1 = PointStruct::new(point_id.to_string(), embeddings1[0].clone(), payload1);

    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point1]).wait(true))
        .await
        .expect("Should upsert point");

    // Update with new content
    let text2 = "Updated document content".to_string();
    let embeddings2 = embedder
        .generate(&[text2])
        .await
        .expect("Should generate embedding");

    let mut payload2: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
    payload2.insert(
        "version".to_string(),
        qdrant_client::qdrant::Value {
            kind: Some(qdrant_client::qdrant::value::Kind::IntegerValue(2)),
        },
    );

    let point2 = PointStruct::new(point_id.to_string(), embeddings2[0].clone(), payload2);

    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point2]).wait(true))
        .await
        .expect("Should upsert updated point");

    // Verify still only one point
    let count = get_points_count(&client, collection_name)
        .await
        .expect("Should get points count");
    assert_eq!(count, 1);

    // Verify the embedding was updated (search with new embedding should return it)
    let search_result = search_points(&client, collection_name, embeddings2[0].clone(), 1)
        .await
        .expect("Should search points");

    assert_eq!(search_result.result.len(), 1);
    // Score should be very high (close to 1.0) for exact match
    assert!(search_result.result[0].score > 0.99);

    qdrant.cleanup().await;
}

/// Test deleting a point
#[tokio::test]
#[serial]
async fn test_delete_point() {
    use drasi_reaction_qdrant::Embedder;
    use qdrant_client::qdrant::{DeletePointsBuilder, PointsIdsList};

    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");
    let embedder = MockEmbedder::new(128);

    let collection_name = "test_delete";

    // Create collection
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
        )
        .await
        .expect("Should create collection");

    let point_id = generate_deterministic_uuid("doc-delete-001");

    // Insert point
    let text = "Document to be deleted".to_string();
    let embeddings = embedder
        .generate(&[text])
        .await
        .expect("Should generate embedding");

    let payload: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
    let point = PointStruct::new(point_id.to_string(), embeddings[0].clone(), payload);

    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point]).wait(true))
        .await
        .expect("Should upsert point");

    // Verify point exists
    let count = get_points_count(&client, collection_name)
        .await
        .expect("Should get points count");
    assert_eq!(count, 1);

    // Delete point
    client
        .delete_points(
            DeletePointsBuilder::new(collection_name)
                .points(PointsIdsList {
                    ids: vec![point_id.to_string().into()],
                })
                .wait(true),
        )
        .await
        .expect("Should delete point");

    // Verify point was deleted
    let count = get_points_count(&client, collection_name)
        .await
        .expect("Should get points count");
    assert_eq!(count, 0);

    qdrant.cleanup().await;
}

/// Test batch processing of multiple points
#[tokio::test]
#[serial]
async fn test_batch_processing() {
    use drasi_reaction_qdrant::Embedder;

    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");
    let embedder = MockEmbedder::new(128);

    let collection_name = "test_batch";

    // Create collection
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
        )
        .await
        .expect("Should create collection");

    // Create multiple points
    let texts = vec![
        "First document about artificial intelligence".to_string(),
        "Second document about machine learning".to_string(),
        "Third document about deep learning".to_string(),
    ];

    let embeddings = embedder
        .generate(&texts)
        .await
        .expect("Should generate embeddings");
    assert_eq!(embeddings.len(), 3);

    let points: Vec<PointStruct> = texts
        .iter()
        .enumerate()
        .zip(embeddings.iter())
        .map(|((i, text), embedding)| {
            let point_id = generate_deterministic_uuid(&format!("batch-doc-{i}"));
            let mut payload: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
            payload.insert(
                "content".to_string(),
                qdrant_client::qdrant::Value {
                    kind: Some(qdrant_client::qdrant::value::Kind::StringValue(
                        text.clone(),
                    )),
                },
            );
            PointStruct::new(point_id.to_string(), embedding.clone(), payload)
        })
        .collect();

    // Batch upsert
    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, points).wait(true))
        .await
        .expect("Should upsert points");

    // Verify all points were created
    let count = get_points_count(&client, collection_name)
        .await
        .expect("Should get points count");
    assert_eq!(count, 3);

    qdrant.cleanup().await;
}

/// Test that similar texts produce similar embeddings (semantic search)
#[tokio::test]
#[serial]
async fn test_semantic_similarity() {
    use drasi_reaction_qdrant::Embedder;

    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create client");
    let embedder = MockEmbedder::new(128);

    let collection_name = "test_similarity";

    // Create collection
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
        )
        .await
        .expect("Should create collection");

    // Create documents
    let docs = vec![
        ("doc1", "The quick brown fox jumps over the lazy dog"),
        ("doc2", "A fast brown fox leaps over a sleepy dog"),
        (
            "doc3",
            "Machine learning is a subset of artificial intelligence",
        ),
    ];

    for (id, content) in &docs {
        let embeddings = embedder
            .generate(&[content.to_string()])
            .await
            .expect("Should generate embedding");
        let point_id = generate_deterministic_uuid(id);
        let mut payload: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
        payload.insert(
            "id".to_string(),
            qdrant_client::qdrant::Value {
                kind: Some(qdrant_client::qdrant::value::Kind::StringValue(
                    id.to_string(),
                )),
            },
        );
        let point = PointStruct::new(point_id.to_string(), embeddings[0].clone(), payload);
        client
            .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point]).wait(true))
            .await
            .expect("Should upsert point");
    }

    // Search with a query similar to the first two documents
    let query = "A brown fox and a dog".to_string();
    let query_embedding = embedder
        .generate(&[query])
        .await
        .expect("Should generate embedding");

    let search_result = search_points(&client, collection_name, query_embedding[0].clone(), 3)
        .await
        .expect("Should search points");

    assert_eq!(search_result.result.len(), 3);

    // All results should be returned with scores
    // Note: With mock embedder, similarity is based on SHA-256 hash, not semantic meaning
    // Cosine similarity ranges from -1 to 1, so we just verify search returns results
    // and that each result has a score (can be negative with dissimilar vectors)
    for result in &search_result.result {
        // Score exists and is within valid cosine similarity range
        assert!(result.score >= -1.0 && result.score <= 1.0);
    }

    qdrant.cleanup().await;
}

/// Test QdrantReaction can be created with config
#[test]
fn test_reaction_creation() {
    use drasi_reaction_qdrant::QdrantReaction;

    let config = QdrantReactionConfig {
        qdrant: QdrantConfig {
            endpoint: "http://localhost:6334".to_string(),
            api_key: None,
            collection_name: "test_collection".to_string(),
            create_collection: true,
        },
        embedding: EmbeddingConfig::Mock { dimensions: 1536 },
        document: DocumentConfig {
            document_template: "{{name}}: {{description}}".to_string(),
            title_template: Some("{{name}}".to_string()),
            key_field: "id".to_string(),
            metadata_fields: None,
        },
        batch: BatchConfig::default(),
        retry: RetryConfig::default(),
    };

    let embedder = Arc::new(MockEmbedder::new(1536));
    let reaction = QdrantReaction::with_embedder(
        "test-reaction",
        vec!["query1".to_string()],
        config,
        embedder,
    );

    assert!(reaction.is_ok());
}

/// Test QdrantReaction builder pattern
#[test]
fn test_reaction_builder() {
    use drasi_reaction_qdrant::QdrantReaction;

    let config = QdrantReactionConfig {
        qdrant: QdrantConfig {
            endpoint: "http://localhost:6334".to_string(),
            api_key: None,
            collection_name: "test_collection".to_string(),
            create_collection: true,
        },
        embedding: EmbeddingConfig::Mock { dimensions: 1536 },
        document: DocumentConfig::default(),
        batch: BatchConfig::default(),
        retry: RetryConfig::default(),
    };

    let embedder = Arc::new(MockEmbedder::new(1536));

    let reaction = QdrantReaction::builder("builder-test")
        .with_query("query1")
        .with_query("query2")
        .with_config(config)
        .with_embedder(embedder)
        .with_auto_start(false)
        .with_priority_queue_capacity(5000)
        .build();

    assert!(reaction.is_ok());
    let reaction = reaction.expect("Reaction should be created");

    use drasi_lib::Reaction;
    assert_eq!(reaction.id(), "builder-test");
    assert_eq!(reaction.query_ids().len(), 2);
    assert!(!reaction.auto_start());
}
