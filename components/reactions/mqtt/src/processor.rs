
use super::MqttReactionBuilder;
use crate::{config::{
    MqttAuthMode, MqttCallSpec, MqttQueryConfig, MqttReactionConfig, MqttTransportMode, RetainPolicy
}, mqtt};
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use std::{default, sync::Arc};
use anyhow::Result;
pub struct MqttReaction {
    base: ReactionBase,
    config: MqttReactionConfig,
    adaptive_config: Option<AdaptiveBatchConfig>,
}
use drasi_core::evaluation::variable_value::de;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    managers::log_component_start,
    Reaction,
};
use log::{debug, error, info, warn};

use async_trait::async_trait;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use serde_json::{Map, Value};
use std::{collections::HashMap, os::unix::process, sync::RwLock, time::Duration};

use handlebars::{template, Handlebars};
use rumqttc::v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions};
use super::client::MqttClient;
pub struct Processor {}

impl Processor {


    ////////////////////////////////////////////////////////////////////////////////
    /// MAIN EXECUTION FUNC
    ////////////////////////////////////////////////////////////////////////////////
    
    pub(super) async fn run_internal(
        reaction_name: String,
        base: ReactionBase,
        adaptive_config: Option<AdaptiveBatchConfig>,
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
        let mqtt_client = match MqttClient::new(reaction_name.clone(), &config, event_loop_shutdown_rx).await {
            Ok(client) => client,
            Err(e) => {
                error!("[{reaction_name}] Failed to create MQTT client: {:?}", e);
                return;
            }
        };

        // Main processing loop
        match adaptive_config{
            None => Self::basic_processing(
                mqtt_client,
                priority_queue,
                Arc::clone(&status),
                config,
                shutdown_rx,
                event_loop_shutdown_tx,
            ).await,
            Some(config) => {

            }
        }


        info!("[{reaction_name}] MQTT reaction stopped");
        *status.write().await = ComponentStatus::Stopped;
    }

    ////////////////////////////////////////////////////////////////////////////////
    /// BASIC PROCESSING (NO ADAPTIVE BATCHING)
    ////////////////////////////////////////////////////////////////////////////////
    fn extract_data_and_context(result: &ResultDiff, query_name: &str, query_config: &MqttQueryConfig) -> (Option<Value>, Map<String, Value>) {
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
                        mqtt_client.client_id,
                        type_str,
                        query_name
                    );
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to publish MQTT message for {} operation on query '{}': {}",
                        mqtt_client.client_id,
                        type_str,
                        query_name,
                        e
                    );
                }
            }
        }
        Ok(())
    }

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

            let query_name = &query_result_ref.query_id;

            // Check if we have a route configured for this query
            let route = query_configs.get(query_name).or_else(|| {
                query_configs.get("*").or_else(|| {  // * to handle cases of select-all
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
}


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
fn create_handlebars() -> Handlebars<'static>{
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
