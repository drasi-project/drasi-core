use super::MqttReactionBuilder;
use crate::adaptive_batcher::AdaptiveBatchConfig;
use crate::{
    adaptive_batcher::AdaptiveBatcher,
    config::{
        MqttAuthMode, MqttCallSpec, MqttQueryConfig, MqttReactionConfig, MqttTransportMode,
        RetainPolicy,
    },
    mqtt,
};
use anyhow::Result;
use drasi_core::evaluation::variable_value::de;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    managers::log_component_start,
    Reaction,
};
use log::{debug, error, info, warn};
use std::{default, sync::Arc};

use async_trait::async_trait;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use serde_json::{Map, Value};
use std::{collections::HashMap, os::unix::process, sync::RwLock, time::Duration};

use super::client::MqttClient;
use handlebars::{template, Handlebars};
use rumqttc::{
    tokio_rustls::rustls::crypto::hash::Hash,
    v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions},
};
use tokio::sync::mpsc;
pub struct Processor {}

impl Processor {
    //==============================================================================
    // MAIN EXECUTION FUNC
    //==============================================================================
    /// Entry point for the MQTT reaction's processing logic.
    pub(super) async fn run_internal(
        reaction_name: String,
        base: ReactionBase,
        adaptive_config: AdaptiveBatchConfig,
        config: MqttReactionConfig,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let reaction_name = base.id.clone();
        let priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult> =
            base.priority_queue.clone();
        let status = base.status.clone();

        let query_configs = config.query_configs.clone();
        let default_topic: String = config.default_topic.clone();

        // Channels to handle MQTT connection events
        let (mut event_loop_shutdown_tx, mut event_loop_shutdown_rx) =
            tokio::sync::mpsc::channel(1);

        // Create MQTT client
        let mqtt_client =
            match MqttClient::new(reaction_name.clone(), &config, event_loop_shutdown_rx).await {
                Ok(client) => client,
                Err(e) => {
                    error!("[{reaction_name}] Failed to create MQTT client: {e:?}");
                    return;
                }
            };

        // Main processing loop
        match adaptive_config.adaptive_enabled {
            false => {
                Self::basic_processing(
                    mqtt_client,
                    priority_queue,
                    Arc::clone(&status),
                    config,
                    shutdown_rx,
                    event_loop_shutdown_tx,
                )
                .await
            }
            true => {
                Self::adaptive_batch_processing(
                    mqtt_client,
                    priority_queue,
                    Arc::clone(&status),
                    config,
                    adaptive_config,
                    shutdown_rx,
                    event_loop_shutdown_tx,
                )
                .await
            }
        }

        info!("[{reaction_name}] MQTT reaction stopped");
        *status.write().await = ComponentStatus::Stopped;
    }

    //==============================================================================
    // ADAPTIVE BATCHING PROCESSING
    //==============================================================================
    async fn adaptive_batch_processing(
        mqtt_client: MqttClient,
        priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult>,
        status: Arc<tokio::sync::RwLock<ComponentStatus>>,
        config: MqttReactionConfig,
        adaptive_config: AdaptiveBatchConfig,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        mut event_loop_shutdown_tx: tokio::sync::mpsc::Sender<()>,
    ) {
        // Create channel for batching with capacity based on batch configuration
        let batch_channel_capacity = adaptive_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);

        debug!(
            "[{}] MqttAdaptiveReaction using batch channel capacity: {} (max_batch_size: {} × 5)",
            mqtt_client.client_id, batch_channel_capacity, adaptive_config.max_batch_size
        );

        // Set up Handlebars with json helper
        let handlebars = create_handlebars();

        let reaction_name = mqtt_client.client_id.clone();

        // Spawn adaptive batcher task
        let batcher_handle = tokio::spawn({
            let reaction_name = mqtt_client.client_id.clone();
            async move {
                let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config);

                let mut total_batches = 0u64;
                let mut total_results = 0u64;

                info!("[{reaction_name}] Adaptive MQTT batcher started");

                while let Some(batch) = batcher.next_batch().await {
                    if batch.is_empty() {
                        continue;
                    }

                    // get the total number of results in the batch for all queries.
                    let batch_size = batch
                        .iter()
                        .map(|(_, v): &(String, Vec<ResultDiff>)| v.len())
                        .sum::<usize>();

                    total_batches += 1;
                    total_results += batch_size as u64;

                    debug!("[{reaction_name}] Processing adaptive batch of {batch_size} results");

                    Self::process_batch(batch, &mqtt_client, &config, &handlebars).await;

                    if total_batches.is_multiple_of(100) {
                        info!(
                                "[{}] Adaptive MQTT metrics - Batches: {}, Results: {}, Avg batch size: {:.1}",
                                reaction_name,
                                total_batches,
                                total_results,
                                total_results as f64 / total_batches as f64
                            );
                    }
                }
                info!(
                        "[{reaction_name}] Adaptive MQTT batcher stopped - Total batches: {total_batches}, Total results: {total_results}"
                    );
            }
        });

        // Main processing loop for incoming results to be batched
        loop {
            // Use select to wait for either a result OR shutdown signal
            let query_result_arc = tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                    break;
                }

                result = priority_queue.dequeue() => result,
            };
            let query_result = query_result_arc.as_ref();

            if !matches!(*status.read().await, ComponentStatus::Running) {
                info!("[{reaction_name}] Reaction status changed to non-running, exiting");
                break;
            }

            if query_result.results.is_empty() {
                continue;
            }

            // Send to batcher
            if batch_tx
                .send((query_result.query_id.clone(), query_result.results.clone()))
                .await
                .is_err()
            {
                error!("[{reaction_name}] Failed to send to batch channel");
                break;
            }
        }

        // Close the batch channel
        drop(batch_tx);

        // Wait for batcher to complete
        let _ = tokio::time::timeout(Duration::from_secs(5), batcher_handle).await;

        info!("[{reaction_name}] Adaptive HTTP reaction completed");
    }

    // Process a batch of results, final process is done by the process_result function which is shared with basic processing.
    async fn process_batch(
        batch: Vec<(String, Vec<ResultDiff>)>,
        mqtt_client: &MqttClient,
        config: &MqttReactionConfig,
        handlebars: &Handlebars<'_>,
    ) {
        // Group by query_id for batch processing
        let mut batch_by_query: HashMap<String, Vec<ResultDiff>> = HashMap::new();
        for (query_id, results) in batch {
            batch_by_query.entry(query_id).or_default().extend(results);
        }

        for (query_id, results) in batch_by_query {
            // Check if we have a route configured for this query
            let route = config.query_configs.get(&query_id).or_else(|| {
                config.query_configs.get("*").or_else(|| {
                    // * to handle cases of select-all
                    query_id
                        .split('.')
                        .next_back()
                        .and_then(|name| config.query_configs.get(name))
                })
            });

            let query_config = match route {
                Some(config) => config.clone(),
                None => {
                    debug!(
                        "[{}] No configuration for query '{}', using default",
                        mqtt_client.client_id, query_id
                    );
                    default_config_for_query(&config.default_topic)
                }
            };

            debug!(
                "[{}] Processing {} results from query '{}'",
                mqtt_client.client_id,
                results.len(),
                query_id
            );

            // Process each result
            for result in &results {
                if let Err(e) =
                    Self::process_result(mqtt_client, handlebars, result, &query_id, &query_config)
                        .await
                {
                    error!(
                        "[{}] Failed to process result: {}",
                        mqtt_client.client_id, e
                    );
                }
            }
        }
    }

    //==============================================================================
    // BASIC PROCESSING (NO ADAPTIVE BATCHING)
    //==============================================================================

    async fn basic_processing(
        mqtt_client: MqttClient,
        priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult>,
        status: Arc<tokio::sync::RwLock<ComponentStatus>>,
        config: MqttReactionConfig,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        mut event_loop_shutdown_tx: tokio::sync::mpsc::Sender<()>,
    ) {
        // Set up Handlebars with json helper
        let handlebars = create_handlebars();

        let default_topic = config.default_topic;
        let query_configs = config.query_configs;
        let reaction_name = mqtt_client.client_id.clone();

        loop {
            let query_result_arc = tokio::select! {
                biased; // give priority to shutdown

                _ = &mut shutdown_rx => {
                    info!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                    event_loop_shutdown_tx.send(()).await;
                    break;
                },

                result = priority_queue.dequeue() => result,
            };

            if !matches!(*status.read().await, ComponentStatus::Running) {
                info!("[{reaction_name}] Status is not running, exiting processing loop");
                event_loop_shutdown_tx.send(()).await;
                break;
            }

            let query_result_ref = query_result_arc.as_ref();
            if query_result_ref.results.is_empty() {
                debug!("[{reaction_name}] Recieved empty result set from query");
                continue;
            }

            let query_name: &String = &query_result_ref.query_id;

            // Check if we have a route configured for this query
            let route = query_configs.get(query_name).or_else(|| {
                query_configs.get("*").or_else(|| {
                    // * to handle cases of select-all
                    query_name
                        .split('.')
                        .next_back()
                        .and_then(|name| query_configs.get(name))
                })
            });

            let query_config = match route {
                Some(config) => config.clone(),
                None => {
                    debug!(
                        "[{reaction_name}] No configuration for query '{query_name}', using default"
                    );
                    default_config_for_query(&default_topic)
                }
            };

            debug!(
                "[{}] Processing {} results from query '{}'",
                reaction_name,
                query_result_ref.results.len(),
                query_name
            );

            // Process each result
            for result in &query_result_ref.results {
                if let Err(e) = Self::process_result(
                    &mqtt_client,
                    &handlebars,
                    result,
                    query_name,
                    &query_config,
                )
                .await
                {
                    error!("[{reaction_name}] Failed to process result: {e}");
                }
            }
        }
    }

    //========================================================
    // HELPER FUNCTIONS
    //========================================================

    #[allow(clippy::too_many_arguments)]
    async fn process_result(
        mqtt_client: &MqttClient,
        handlebars: &Handlebars<'_>,
        result: &ResultDiff,
        query_name: &str,
        query_config: &MqttQueryConfig,
    ) -> Result<()> {
        match result {
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                debug!(
                    "[{}] Received aggregation or noop result, skipping MQTT call",
                    mqtt_client.client_id,
                );
                return Ok(());
            }
            _ => {}
        }

        let (data, context) = Self::extract_data_and_context(result, query_name, query_config);

        let type_str = result_type(result);

        let call_spec_result = match type_str {
            "ADD" => query_config.added.as_ref(),
            "UPDATE" => query_config.updated.as_ref(),
            "DELETE" => query_config.deleted.as_ref(),
            _ => None,
        };

        if let Some(call_spec) = call_spec_result {
            let body = if !call_spec.body.is_empty() {
                handlebars.render_template(&call_spec.body, &context)?
            } else {
                serde_json::to_string(&data)?
            };
            match mqtt_client.publish_from_call_spec(call_spec, body).await {
                Ok(_) => {
                    debug!(
                        "[{}] Published MQTT message for {} operation on query '{}'",
                        mqtt_client.client_id, type_str, query_name
                    );
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to publish MQTT message for {} operation on query '{}': {}",
                        mqtt_client.client_id, type_str, query_name, e
                    );
                }
            }
        }
        Ok(())
    }

    fn extract_data_and_context(
        result: &ResultDiff,
        query_name: &str,
        query_config: &MqttQueryConfig,
    ) -> (Option<Value>, Map<String, Value>) {
        let mut context: Map<String, Value> = Map::new();
        let mut data = match result {
            ResultDiff::Add { data } => {
                if let Some(spec) = query_config.added.as_ref() {
                    context.insert("after".to_string(), data.clone());
                    Some(data.clone())
                } else {
                    None
                }
            }
            ResultDiff::Update { .. } => {
                if let Some(spec) = query_config.updated.as_ref() {
                    let data_to_process = serde_json::to_value(result)
                        .expect("ResultDiff serialization should succeed");
                    let data = data_to_process.clone();
                    if let Some(obj) = data_to_process.as_object() {
                        if let Some(before) = obj.get("before") {
                            context.insert("before".to_string(), before.clone());
                        }
                        if let Some(after) = obj.get("after") {
                            context.insert("after".to_string(), after.clone());
                        }
                        if let Some(data_field) = obj.get("data") {
                            context.insert("data".to_string(), data_field.clone());
                        }
                    } else {
                        context.insert("after".to_string(), data_to_process);
                    }
                    Some(data)
                } else {
                    None
                }
            }
            ResultDiff::Delete { data } => {
                if let Some(spec) = query_config.deleted.as_ref() {
                    context.insert("before".to_string(), data.clone());
                    Some(data.clone())
                } else {
                    None
                }
            }
            _ => {
                // This should not happen due to the early check.
                None
            }
        };

        context.insert(
            "query_name".to_string(),
            Value::String(query_name.to_string()),
        );

        let type_str = result_type(result);
        context.insert("operation".to_string(), Value::String(type_str.to_string()));
        (data, context)
    }

    fn extract_query_name_and_config() {}
}

//=================================================================
// HELPER INLINE FUNCTIONS
//=================================================================
#[inline]
fn result_type(result_diff: &ResultDiff) -> &'static str {
    match result_diff {
        ResultDiff::Add { .. } => "ADD",
        ResultDiff::Update { .. } => "UPDATE",
        ResultDiff::Delete { .. } => "DELETE",
        _ => "",
    }
}

#[inline]
fn default_config_for_query(default_topic: &str) -> MqttQueryConfig {
    MqttQueryConfig {
        added: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
        updated: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
        deleted: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
    }
}

#[inline]
fn create_handlebars() -> Handlebars<'static> {
    // Set up Handlebars with json helper
    let mut handlebars = Handlebars::new();
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    let json_str = serde_json::to_string(&value.value())
                        .unwrap_or_else(|_| "null".to_string());
                    out.write(&json_str)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}
