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

//! Live integration test for AzureOpenAIEmbedder against real Azure OpenAI API.
//!
//! This test is ignored by default and only runs when explicitly requested.
//!
//! # Running the test
//!
//! ```bash
//! export TEST_AZURE_OPENAI_ENDPOINT="https://your-resource.openai.azure.com"
//! export TEST_AZURE_OPENAI_KEY="your-api-key"
//! export TEST_AZURE_OPENAI_MODEL="text-embedding-3-large"
//! cargo test -p drasi-reaction-qdrant --test live_azure_test -- --ignored
//! ```

mod qdrant_helpers;

use drasi_reaction_qdrant::{generate_deterministic_uuid, AzureOpenAIEmbedder, Embedder};
use qdrant_client::qdrant::{
    CreateCollectionBuilder, Distance, PointStruct, UpsertPointsBuilder, Value, VectorParamsBuilder,
};
use qdrant_helpers::{search_points, setup_qdrant};
use std::collections::HashMap;
use std::env;

/// Helper to get required environment variable or panic with clear message
fn require_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| {
        panic!(
            "Environment variable {name} is required for live Azure test. \
             Set it before running: export {name}=\"your-value\""
        )
    })
}

/// Live integration test for AzureOpenAIEmbedder against real Azure OpenAI API.
///
/// This test verifies the full pipeline:
/// 1. Create AzureOpenAIEmbedder with real credentials
/// 2. Setup Qdrant testcontainer
/// 3. Generate embedding for test text
/// 4. Upsert embedding to Qdrant
/// 5. Search to confirm the pipeline works end-to-end
#[tokio::test]
#[ignore] // Only run when explicitly requested with: cargo test -- --ignored
async fn test_azure_openai_embedder_live() {
    env_logger::try_init().ok();

    // 1. Read credentials from environment
    let endpoint = require_env("TEST_AZURE_OPENAI_ENDPOINT");
    let api_key = require_env("TEST_AZURE_OPENAI_KEY");
    let model = require_env("TEST_AZURE_OPENAI_MODEL");

    // 2. Create AzureOpenAIEmbedder
    let dimensions = 3072; // text-embedding-3-large default
    let embedder = AzureOpenAIEmbedder::new(
        endpoint,
        model,
        api_key,
        dimensions,
        "2024-02-01".to_string(),
    );

    assert_eq!(embedder.dimensions(), dimensions);
    assert_eq!(embedder.name(), "azure_openai");

    // 3. Setup Qdrant testcontainer
    let qdrant = setup_qdrant().await;
    let client = qdrant.get_client().expect("Should create Qdrant client");

    // 4. Create collection with correct dimensions
    let collection_name = "live_azure_test";
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name).vectors_config(VectorParamsBuilder::new(
                dimensions as u64,
                Distance::Cosine,
            )),
        )
        .await
        .expect("Should create collection");

    // 5. Generate embedding for test text
    let test_text = "Drasi is a continuous query engine for streaming data".to_string();
    let embeddings = embedder
        .generate(&[test_text.clone()])
        .await
        .expect("Should generate embedding from Azure OpenAI");

    // 6. Verify embedding dimensions
    assert_eq!(embeddings.len(), 1, "Should return one embedding");
    assert!(
        !embeddings[0].is_empty(),
        "Embedding vector should not be empty"
    );
    assert_eq!(
        embeddings[0].len(),
        dimensions,
        "Embedding should have {dimensions} dimensions"
    );

    // Verify embedding values are reasonable (normalized for cosine similarity)
    let magnitude: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (magnitude - 1.0).abs() < 0.1,
        "Embedding should be roughly normalized, got magnitude: {magnitude}"
    );

    // 7. Upsert to Qdrant
    let point_id = generate_deterministic_uuid("drasi-live-test");
    let mut payload: HashMap<String, Value> = HashMap::new();
    payload.insert("text".to_string(), Value::from(test_text.clone()));

    let point = PointStruct::new(point_id.to_string(), embeddings[0].clone(), payload);
    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point]).wait(true))
        .await
        .expect("Should upsert point to Qdrant");

    // 8. Search to confirm full pipeline works
    let search_result = search_points(&client, collection_name, embeddings[0].clone(), 1)
        .await
        .expect("Should search points");

    assert_eq!(
        search_result.result.len(),
        1,
        "Should find one matching point"
    );
    assert!(
        search_result.result[0].score > 0.99,
        "Self-search should have very high score (got {})",
        search_result.result[0].score
    );

    log::info!(
        "Live Azure OpenAI test passed! Generated {} dimension embedding, \
         upserted to Qdrant, and verified with search (score: {:.4})",
        embeddings[0].len(),
        search_result.result[0].score
    );

    // 9. Cleanup
    qdrant.cleanup().await;
}
