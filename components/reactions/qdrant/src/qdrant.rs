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

//! Qdrant vector database reaction implementation.
//!
//! This module provides the main `QdrantReaction` that synchronizes Drasi query
//! results to a Qdrant vector database by generating embeddings and storing them
//! as searchable vector points.

use crate::config::{EmbeddingConfig, QdrantReactionConfig, RetryConfig};
use crate::document::{
    extract_field, generate_deterministic_uuid, setup_handlebars, Document, DocumentBuilder,
};
use crate::embedder::{AzureOpenAIEmbedder, Embedder, MockEmbedder};
use anyhow::{Context, Result};
use async_trait::async_trait;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use qdrant_client::qdrant::{
    CreateCollectionBuilder, DeletePointsBuilder, Distance, PointStruct, PointsIdsList,
    UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_client::Qdrant;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::queries::manager::DrasiQuery;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

/// Error handler callback type for handling failures after retry exhaustion.
///
/// The handler receives an `anyhow::Error` describing the failure.
/// By default (if no handler is set), errors are logged and processing continues.
pub type ErrorHandler = Arc<dyn Fn(anyhow::Error) + Send + Sync>;

/// Qdrant vector database reaction.
///
/// Synchronizes Drasi query results to Qdrant by:
/// 1. Generating document content from query results using Handlebars templates
/// 2. Creating embeddings for the document content
/// 3. Storing the embeddings as vector points in Qdrant
///
/// # Example
///
/// ```rust,no_run
/// use drasi_reaction_qdrant::{
///     QdrantReaction, QdrantReactionConfig, QdrantConfig,
///     EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, MockEmbedder,
/// };
/// use std::sync::Arc;
///
/// fn main() -> anyhow::Result<()> {
///     let config = QdrantReactionConfig {
///         qdrant: QdrantConfig {
///             endpoint: "http://localhost:6334".to_string(),
///             api_key: None,
///             collection_name: "my_collection".to_string(),
///             create_collection: true,
///         },
///         embedding: EmbeddingConfig::Mock { dimensions: 1536 },
///         document: DocumentConfig::default(),
///         batch: BatchConfig::default(),
///         retry: RetryConfig::default(),
///     };
///     let embedder = Arc::new(MockEmbedder::new(1536));
///
///     let reaction = QdrantReaction::with_embedder(
///         "my-reaction",
///         vec!["my-query".to_string()],
///         config,
///         embedder,
///     )?;
///     Ok(())
/// }
/// ```
pub struct QdrantReaction {
    base: ReactionBase,
    config: QdrantReactionConfig,
    embedder: Arc<dyn Embedder>,
    qdrant_client: Arc<Mutex<Option<Qdrant>>>,
    handlebars: Handlebars<'static>,
    error_handler: Option<ErrorHandler>,
}

impl QdrantReaction {
    /// Create a new Qdrant reaction with the default embedder based on configuration.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the reaction
    /// * `queries` - Query IDs to subscribe to
    /// * `config` - Configuration for Qdrant, embedding, and document generation
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Configuration validation fails
    /// - Template compilation fails
    /// - Azure OpenAI is configured but credentials are missing
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: QdrantReactionConfig,
    ) -> Result<Self> {
        config.validate()?;

        // Create embedder based on configuration
        let embedder: Arc<dyn Embedder> = match &config.embedding {
            EmbeddingConfig::Mock { dimensions } => Arc::new(MockEmbedder::new(*dimensions)),
            EmbeddingConfig::AzureOpenai {
                endpoint,
                model,
                api_key,
                dimensions,
                api_version,
            } => Arc::new(AzureOpenAIEmbedder::new(
                endpoint.clone(),
                model.clone(),
                api_key.clone(),
                *dimensions,
                api_version.clone(),
            )),
        };

        Self::with_embedder(id, queries, config, embedder)
    }

    /// Create a new Qdrant reaction with a custom embedder.
    ///
    /// This is useful for testing with mock embedders or using custom embedding
    /// implementations.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the reaction
    /// * `queries` - Query IDs to subscribe to
    /// * `config` - Configuration for Qdrant and document generation
    /// * `embedder` - Custom embedder implementation
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Configuration validation fails
    /// - Template compilation fails
    pub fn with_embedder(
        id: impl Into<String>,
        queries: Vec<String>,
        config: QdrantReactionConfig,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self> {
        let id = id.into();

        // Validate configuration
        config.validate()?;

        // Setup Handlebars
        let handlebars = setup_handlebars(
            &config.document.document_template,
            config.document.title_template.as_deref(),
        )?;

        let params = ReactionBaseParams::new(id, queries);

        Ok(Self {
            base: ReactionBase::new(params),
            config,
            embedder,
            qdrant_client: Arc::new(Mutex::new(None)),
            handlebars,
            error_handler: None,
        })
    }

    /// Create a builder for constructing a QdrantReaction.
    pub fn builder(id: impl Into<String>) -> QdrantReactionBuilder {
        QdrantReactionBuilder::new(id)
    }

    /// Initialize the Qdrant client connection.
    async fn initialize_qdrant_client(&self) -> Result<Qdrant> {
        let mut builder = Qdrant::from_url(&self.config.qdrant.endpoint);

        if let Some(ref api_key) = self.config.qdrant.api_key {
            builder = builder.api_key(api_key.clone());
        }

        let client = builder.build().context("Failed to create Qdrant client")?;

        info!(
            "[{}] Connected to Qdrant at {}",
            self.base.id, self.config.qdrant.endpoint
        );

        Ok(client)
    }

    /// Ensure the collection exists, creating it if necessary.
    async fn ensure_collection(&self, client: &Qdrant) -> Result<()> {
        let collection_name = &self.config.qdrant.collection_name;

        // Check if collection exists
        let exists = client
            .collection_exists(collection_name)
            .await
            .context("Failed to check collection existence")?;

        if exists {
            info!(
                "[{}] Collection '{}' already exists",
                self.base.id, collection_name
            );
            return Ok(());
        }

        if !self.config.qdrant.create_collection {
            anyhow::bail!(
                "Collection '{collection_name}' does not exist and create_collection is false"
            );
        }

        // Create collection with the correct dimensions
        let dimensions = self.embedder.dimensions() as u64;

        client
            .create_collection(
                CreateCollectionBuilder::new(collection_name)
                    .vectors_config(VectorParamsBuilder::new(dimensions, Distance::Cosine)),
            )
            .await
            .context("Failed to create collection")?;

        info!(
            "[{}] Created collection '{}' with {} dimensions",
            self.base.id, collection_name, dimensions
        );

        Ok(())
    }

    /// Upsert a point to Qdrant.
    async fn upsert_point(
        &self,
        client: &Qdrant,
        point_id: uuid::Uuid,
        embedding: Vec<f32>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let collection_name = &self.config.qdrant.collection_name;

        // Convert payload to Qdrant format
        let qdrant_payload: HashMap<String, qdrant_client::qdrant::Value> =
            convert_json_to_qdrant_payload(&payload);

        let point = PointStruct::new(point_id.to_string(), embedding, qdrant_payload);

        client
            .upsert_points(UpsertPointsBuilder::new(collection_name, vec![point]).wait(true))
            .await
            .context("Failed to upsert point to Qdrant")?;

        debug!(
            "[{}] Upserted point {} to collection '{}'",
            self.base.id, point_id, collection_name
        );

        Ok(())
    }

    /// Delete a point from Qdrant.
    async fn delete_point(&self, client: &Qdrant, point_id: uuid::Uuid) -> Result<()> {
        let collection_name = &self.config.qdrant.collection_name;

        client
            .delete_points(
                DeletePointsBuilder::new(collection_name)
                    .points(PointsIdsList {
                        ids: vec![point_id.to_string().into()],
                    })
                    .wait(true),
            )
            .await
            .context("Failed to delete point from Qdrant")?;

        debug!(
            "[{}] Deleted point {} from collection '{}'",
            self.base.id, point_id, collection_name
        );

        Ok(())
    }
}

/// Retry an async operation with exponential backoff and jitter.
///
/// # Arguments
///
/// * `operation_name` - Name of the operation for logging
/// * `config` - Retry configuration
/// * `reaction_id` - Reaction ID for logging
/// * `operation` - Async closure to retry
///
/// # Returns
///
/// Returns the result of the operation if successful within max_retries attempts,
/// otherwise returns the last error encountered.
async fn retry_with_backoff<T, F, Fut>(
    operation_name: &str,
    config: &RetryConfig,
    reaction_id: &str,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0;
    let mut delay_ms = config.initial_delay_ms;

    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt < config.max_retries => {
                attempt += 1;
                warn!(
                    "[{reaction_id}] {operation_name} failed (attempt {}/{}): {e}. Retrying in {delay_ms}ms...",
                    attempt,
                    config.max_retries + 1
                );

                // Add jitter (±25% randomization to prevent thundering herd)
                let jitter_factor = rand::thread_rng().gen_range(0.75..1.25);
                let actual_delay = (delay_ms as f64 * jitter_factor) as u64;

                tokio::time::sleep(Duration::from_millis(actual_delay)).await;

                // Exponential backoff capped at max_delay_ms
                delay_ms = (delay_ms * 2).min(config.max_delay_ms);
            }
            Err(e) => {
                error!(
                    "[{reaction_id}] {operation_name} failed after {} attempts. Final error: {e:#}",
                    attempt + 1
                );
                return Err(e);
            }
        }
    }
}

/// Process a batch of documents: build docs, generate embeddings, upsert to Qdrant.
///
/// This function handles the complete pipeline for a batch of data items:
/// 1. Builds Document structs from data using templates
/// 2. Generates embeddings in sub-batches (respecting API limits)
/// 3. Creates Qdrant PointStructs with embeddings and metadata
/// 4. Performs batch upsert to Qdrant
///
/// Returns the number of successfully upserted points.
async fn process_upsert_batch(
    data_items: &[serde_json::Value],
    document_builder: &DocumentBuilder<'_>,
    embedder: &Arc<dyn Embedder>,
    client: &Qdrant,
    collection_name: &str,
    embedding_batch_size: usize,
    reaction_id: &str,
) -> Result<usize> {
    // 1. Build all documents (filter failures, log errors)
    let docs: Vec<Document> = data_items
        .iter()
        .filter_map(|data| match document_builder.build(data) {
            Ok(d) => Some(d),
            Err(e) => {
                error!("[{reaction_id}] Failed to build document: {e}");
                None
            }
        })
        .collect();

    if docs.is_empty() {
        return Ok(0);
    }

    // 2. Extract content strings for embedding
    let contents: Vec<String> = docs.iter().map(|d| d.content.clone()).collect();

    // 3. Generate embeddings in sub-batches (respect API limits)
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(contents.len());
    for chunk in contents.chunks(embedding_batch_size) {
        let chunk_vec: Vec<String> = chunk.to_vec();
        let embeddings = embedder
            .generate(&chunk_vec)
            .await
            .context("Failed to generate embeddings for batch")?;
        all_embeddings.extend(embeddings);
    }

    // Verify we got the right number of embeddings
    if all_embeddings.len() != docs.len() {
        anyhow::bail!(
            "Embedding count mismatch: expected {}, got {}",
            docs.len(),
            all_embeddings.len()
        );
    }

    // 4. Build PointStructs
    let points: Vec<PointStruct> = docs
        .iter()
        .zip(all_embeddings.iter())
        .map(|(doc, embedding)| {
            let payload = convert_json_to_qdrant_payload(&doc.metadata);
            PointStruct::new(doc.point_id.to_string(), embedding.clone(), payload)
        })
        .collect();

    // 5. Batch upsert to Qdrant
    let count = points.len();
    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, points).wait(true))
        .await
        .context("Failed to batch upsert points to Qdrant")?;

    debug!("[{reaction_id}] Batch upserted {count} points to '{collection_name}'");
    Ok(count)
}

/// Builder for QdrantReaction
pub struct QdrantReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: Option<QdrantReactionConfig>,
    embedder: Option<Arc<dyn Embedder>>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    error_handler: Option<ErrorHandler>,
}

impl QdrantReactionBuilder {
    /// Create a new builder with the given reaction ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: None,
            embedder: None,
            priority_queue_capacity: None,
            auto_start: true,
            error_handler: None,
        }
    }

    /// Set the query IDs to subscribe to.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Connect this reaction to receive results from a query.
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: QdrantReactionConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Set a custom embedder (overrides config-based embedder).
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Set custom priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set an error handler for failures after retry exhaustion.
    ///
    /// If not set, errors are logged and processing continues.
    pub fn with_error_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(anyhow::Error) + Send + Sync + 'static,
    {
        self.error_handler = Some(Arc::new(handler));
        self
    }

    /// Build the QdrantReaction.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Configuration is not set
    /// - Configuration validation fails
    /// - Template compilation fails
    pub fn build(self) -> Result<QdrantReaction> {
        let config = self.config.context("Configuration is required")?;
        config.validate()?;

        // Use provided embedder or create from config
        let embedder: Arc<dyn Embedder> = if let Some(e) = self.embedder {
            e
        } else {
            match &config.embedding {
                EmbeddingConfig::Mock { dimensions } => Arc::new(MockEmbedder::new(*dimensions)),
                EmbeddingConfig::AzureOpenai {
                    endpoint,
                    model,
                    api_key,
                    dimensions,
                    api_version,
                } => Arc::new(AzureOpenAIEmbedder::new(
                    endpoint.clone(),
                    model.clone(),
                    api_key.clone(),
                    *dimensions,
                    api_version.clone(),
                )),
            }
        };

        let handlebars = setup_handlebars(
            &config.document.document_template,
            config.document.title_template.as_deref(),
        )?;

        let mut params =
            ReactionBaseParams::new(self.id, self.queries).with_auto_start(self.auto_start);
        if let Some(capacity) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Ok(QdrantReaction {
            base: ReactionBase::new(params),
            config,
            embedder,
            qdrant_client: Arc::new(Mutex::new(None)),
            handlebars,
            error_handler: self.error_handler,
        })
    }
}

#[async_trait]
impl Reaction for QdrantReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "qdrant"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "qdrant_endpoint".to_string(),
            serde_json::Value::String(self.config.qdrant.endpoint.clone()),
        );
        props.insert(
            "collection_name".to_string(),
            serde_json::Value::String(self.config.qdrant.collection_name.clone()),
        );
        props.insert(
            "embedding_service".to_string(),
            serde_json::Value::String(self.embedder.name().to_string()),
        );
        props.insert(
            "embedding_dimensions".to_string(),
            serde_json::Value::Number(self.embedder.dimensions().into()),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting Qdrant reaction".to_string()),
            )
            .await?;

        // Initialize Qdrant client
        let client = self.initialize_qdrant_client().await?;

        // Ensure collection exists
        self.ensure_collection(&client).await?;

        // Store client
        {
            let mut client_guard = self.qdrant_client.lock().await;
            *client_guard = Some(client.clone());
        }

        // ============================================================
        // SYNC PROTOCOL: Subscribe -> Snapshot -> Materialize -> Process
        // ============================================================

        // 1. SUBSCRIBE - Subscribe first so events start buffering
        self.base.subscribe_to_queries().await?;

        // 2. SNAPSHOT & MATERIALIZE - Load existing query results into Qdrant
        // Access QueryProvider via context
        let context = self.base.context().await.ok_or_else(|| {
            anyhow::anyhow!("Context not initialized - was reaction added to DrasiLib?")
        })?;
        let query_provider = context.query_provider;

        // Setup document builder for snapshot processing
        let document_builder = DocumentBuilder::new(
            &self.handlebars,
            "document",
            if self.config.document.title_template.is_some() {
                Some("title")
            } else {
                None
            },
            &self.config.document.key_field,
        );

        for query_id in &self.base.queries {
            let query = query_provider.get_query_instance(query_id).await?;

            // Cast to DrasiQuery to access get_current_results()
            // NOTE: UpsertPointsBuilder uses upsert (overwrite) semantics,
            // so idempotency is guaranteed by UUID generation
            if let Some(drasi_query) = query.as_any().downcast_ref::<DrasiQuery>() {
                let current_results = drasi_query.get_current_results().await;

                if !current_results.is_empty() {
                    let total = current_results.len();
                    info!(
                        "[{}] Loading {} existing results from query '{}'",
                        self.base.id, total, query_id
                    );

                    // Process snapshot in batches with retry and progress logging
                    let mut processed = 0;
                    for chunk in current_results.chunks(self.config.batch.upsert_batch_size) {
                        // Clone data for the retry closure
                        let chunk_vec: Vec<serde_json::Value> = chunk.to_vec();

                        // Retry with exponential backoff - fail startup on persistent error
                        let count = retry_with_backoff(
                            "Snapshot batch upsert",
                            &self.config.retry,
                            &self.base.id,
                            || {
                                let chunk_ref = &chunk_vec;
                                let doc_builder = &document_builder;
                                let emb = &self.embedder;
                                let cli = &client;
                                let coll = &self.config.qdrant.collection_name;
                                let emb_batch = self.config.batch.embedding_batch_size;
                                let rid = &self.base.id;
                                async move {
                                    process_upsert_batch(
                                        chunk_ref,
                                        doc_builder,
                                        emb,
                                        cli,
                                        coll,
                                        emb_batch,
                                        rid,
                                    )
                                    .await
                                }
                            },
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Snapshot loading failed for query '{}' after {} retries. \
                                 Cannot start in inconsistent state.",
                                query_id, self.config.retry.max_retries
                            )
                        })?;

                        processed += count;

                        // Progress logging for large datasets
                        if total > 1000 && processed % 1000 < self.config.batch.upsert_batch_size {
                            info!(
                                "[{}] Snapshot progress: {}/{} ({:.1}%)",
                                self.base.id,
                                processed,
                                total,
                                (processed as f64 / total as f64) * 100.0
                            );
                        }
                    }

                    info!(
                        "[{}] Snapshot loading complete for query '{}' ({} items)",
                        self.base.id, query_id, processed
                    );
                }
            } else {
                warn!(
                    "[{}] Query '{}' is not a DrasiQuery, skipping snapshot",
                    self.base.id, query_id
                );
            }
        }

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Qdrant reaction started".to_string()),
            )
            .await?;

        info!(
            "[{}] Started - syncing to Qdrant collection '{}' from queries: {:?}",
            self.base.id, self.config.qdrant.collection_name, self.base.queries
        );

        // 3. PROCESS - Spawn processing task for incremental updates
        let priority_queue = self.base.priority_queue.clone();
        let reaction_id = self.base.id.clone();
        let qdrant_client = self.qdrant_client.clone();
        let embedder = self.embedder.clone();
        let handlebars = self.handlebars.clone();
        let config = self.config.clone();
        let error_handler = self.error_handler.clone();

        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let processing_task = tokio::spawn(async move {
            let document_builder = DocumentBuilder::new(
                &handlebars,
                "document",
                if config.document.title_template.is_some() {
                    Some("title")
                } else {
                    None
                },
                &config.document.key_field,
            );

            loop {
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                let query_result = (*query_result_arc).clone();

                if query_result.results.is_empty() {
                    debug!("[{reaction_id}] Received empty result set from query");
                    continue;
                }

                debug!(
                    "[{reaction_id}] Processing {} results from query '{}'",
                    query_result.results.len(),
                    query_result.query_id
                );

                // Get Qdrant client
                let client = {
                    let guard = qdrant_client.lock().await;
                    match guard.as_ref() {
                        Some(c) => c.clone(),
                        None => {
                            error!("[{reaction_id}] Qdrant client not initialized");
                            continue;
                        }
                    }
                };

                // Separate adds/updates from deletes for batch processing
                let mut upsert_data: Vec<serde_json::Value> = Vec::new();
                let mut delete_ids: Vec<uuid::Uuid> = Vec::new();

                for result in &query_result.results {
                    match result {
                        ResultDiff::Add { data } | ResultDiff::Update { after: data, .. } => {
                            upsert_data.push(data.clone());
                        }
                        ResultDiff::Delete { data } => {
                            match extract_field(data, &config.document.key_field) {
                                Ok(key) => {
                                    delete_ids.push(generate_deterministic_uuid(&key));
                                }
                                Err(e) => {
                                    error!("[{reaction_id}] Failed to extract key for delete: {e}");
                                }
                            }
                        }
                        ResultDiff::Aggregation { .. } => {
                            warn!(
                                "[{reaction_id}] Aggregation results not supported for vector storage"
                            );
                        }
                        ResultDiff::Noop => {
                            // Nothing to do
                        }
                    }
                }

                // Batch process upserts with retry
                if !upsert_data.is_empty() {
                    for chunk in upsert_data.chunks(config.batch.upsert_batch_size) {
                        let chunk_vec: Vec<serde_json::Value> = chunk.to_vec();
                        let chunk_len = chunk_vec.len();

                        // Retry with exponential backoff - invoke error handler on persistent failure
                        if let Err(e) =
                            retry_with_backoff("Batch upsert", &config.retry, &reaction_id, || {
                                let chunk_ref = &chunk_vec;
                                let doc_builder = &document_builder;
                                let emb = &embedder;
                                let cli = &client;
                                let coll = &config.qdrant.collection_name;
                                let emb_batch = config.batch.embedding_batch_size;
                                let rid = &reaction_id;
                                async move {
                                    process_upsert_batch(
                                        chunk_ref,
                                        doc_builder,
                                        emb,
                                        cli,
                                        coll,
                                        emb_batch,
                                        rid,
                                    )
                                    .await
                                }
                            })
                            .await
                        {
                            let err = anyhow::anyhow!(
                                "Batch upsert failed after {} retries. {} items dropped. Error: {e:#}",
                                config.retry.max_retries,
                                chunk_len
                            );
                            error!("[{reaction_id}] {err}");
                            if let Some(ref handler) = error_handler {
                                handler(err);
                            }
                        }
                    }
                }

                // Batch process deletes with retry
                if !delete_ids.is_empty() {
                    let collection_name = config.qdrant.collection_name.clone();
                    let delete_count = delete_ids.len();
                    let ids: Vec<qdrant_client::qdrant::PointId> =
                        delete_ids.iter().map(|id| id.to_string().into()).collect();

                    // Retry with exponential backoff - invoke error handler on persistent failure
                    if let Err(e) =
                        retry_with_backoff("Batch delete", &config.retry, &reaction_id, || {
                            let cli = &client;
                            let coll = &collection_name;
                            let ids_clone = ids.clone();
                            async move {
                                cli.delete_points(
                                    DeletePointsBuilder::new(coll)
                                        .points(PointsIdsList { ids: ids_clone })
                                        .wait(true),
                                )
                                .await
                                .context("Failed to delete points from Qdrant")?;
                                Ok(())
                            }
                        })
                        .await
                    {
                        let err = anyhow::anyhow!(
                            "Batch delete failed after {} retries. {} deletes dropped. Error: {e:#}",
                            config.retry.max_retries,
                            delete_count
                        );
                        error!("[{reaction_id}] {err}");
                        if let Some(ref handler) = error_handler {
                            handler(err);
                        }
                    }

                    debug!(
                        "[{reaction_id}] Deleted {delete_count} points from '{collection_name}'"
                    );
                }
            }
        });

        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Stop processing
        self.base.stop_common().await?;

        // Clear Qdrant client
        {
            let mut client_guard = self.qdrant_client.lock().await;
            *client_guard = None;
        }

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Qdrant reaction stopped".to_string()),
            )
            .await?;

        info!("[{}] Stopped Qdrant reaction", self.base.id);

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

/// Convert JSON value to Qdrant payload format.
fn convert_json_to_qdrant_payload(
    value: &serde_json::Value,
) -> HashMap<String, qdrant_client::qdrant::Value> {
    let mut payload = HashMap::new();

    if let serde_json::Value::Object(map) = value {
        for (key, val) in map {
            payload.insert(key.clone(), json_to_qdrant_value(val));
        }
    }

    payload
}

/// Convert a single JSON value to Qdrant value.
fn json_to_qdrant_value(value: &serde_json::Value) -> qdrant_client::qdrant::Value {
    use qdrant_client::qdrant::value::Kind;
    use qdrant_client::qdrant::Value as QdrantValue;

    match value {
        serde_json::Value::Null => QdrantValue {
            kind: Some(Kind::NullValue(0)),
        },
        serde_json::Value::Bool(b) => QdrantValue {
            kind: Some(Kind::BoolValue(*b)),
        },
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                QdrantValue {
                    kind: Some(Kind::IntegerValue(i)),
                }
            } else if let Some(f) = n.as_f64() {
                QdrantValue {
                    kind: Some(Kind::DoubleValue(f)),
                }
            } else {
                QdrantValue {
                    kind: Some(Kind::StringValue(n.to_string())),
                }
            }
        }
        serde_json::Value::String(s) => QdrantValue {
            kind: Some(Kind::StringValue(s.clone())),
        },
        serde_json::Value::Array(arr) => {
            use qdrant_client::qdrant::ListValue;
            let list = ListValue {
                values: arr.iter().map(json_to_qdrant_value).collect(),
            };
            QdrantValue {
                kind: Some(Kind::ListValue(list)),
            }
        }
        serde_json::Value::Object(obj) => {
            use qdrant_client::qdrant::Struct;
            let fields: HashMap<String, QdrantValue> = obj
                .iter()
                .map(|(k, v)| (k.clone(), json_to_qdrant_value(v)))
                .collect();
            QdrantValue {
                kind: Some(Kind::StructValue(Struct { fields })),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BatchConfig, DocumentConfig, QdrantConfig, RetryConfig};

    fn create_test_config() -> QdrantReactionConfig {
        QdrantReactionConfig {
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
        }
    }

    #[test]
    fn test_create_reaction_with_mock_embedder() {
        let config = create_test_config();
        let embedder = Arc::new(MockEmbedder::new(1536));

        let reaction = QdrantReaction::with_embedder(
            "test-reaction",
            vec!["query1".to_string()],
            config,
            embedder,
        );

        assert!(reaction.is_ok());
        let reaction = reaction.expect("Reaction should be created");
        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.type_name(), "qdrant");
    }

    #[test]
    fn test_reaction_properties() {
        let config = create_test_config();
        let embedder = Arc::new(MockEmbedder::new(1536));

        let reaction = QdrantReaction::with_embedder(
            "test-reaction",
            vec!["query1".to_string()],
            config,
            embedder,
        )
        .expect("Reaction should be created");

        let props = reaction.properties();
        assert_eq!(
            props.get("qdrant_endpoint"),
            Some(&serde_json::Value::String(
                "http://localhost:6334".to_string()
            ))
        );
        assert_eq!(
            props.get("collection_name"),
            Some(&serde_json::Value::String("test_collection".to_string()))
        );
        assert_eq!(
            props.get("embedding_service"),
            Some(&serde_json::Value::String("mock".to_string()))
        );
    }

    #[test]
    fn test_builder_pattern() {
        let config = create_test_config();
        let embedder = Arc::new(MockEmbedder::new(1536));

        let reaction = QdrantReaction::builder("builder-test")
            .with_query("query1")
            .with_query("query2")
            .with_config(config)
            .with_embedder(embedder)
            .with_auto_start(false)
            .build();

        assert!(reaction.is_ok());
        let reaction = reaction.expect("Reaction should be created");
        assert_eq!(reaction.query_ids().len(), 2);
        assert!(!reaction.auto_start());
    }

    #[test]
    fn test_json_to_qdrant_payload() {
        let json = serde_json::json!({
            "string": "hello",
            "number": 42,
            "float": 3.15,
            "bool": true,
            "null": null,
            "array": [1, 2, 3],
            "nested": {
                "key": "value"
            }
        });

        let payload = convert_json_to_qdrant_payload(&json);

        assert!(payload.contains_key("string"));
        assert!(payload.contains_key("number"));
        assert!(payload.contains_key("float"));
        assert!(payload.contains_key("bool"));
        assert!(payload.contains_key("null"));
        assert!(payload.contains_key("array"));
        assert!(payload.contains_key("nested"));
    }
}
