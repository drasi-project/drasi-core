
use super::MqttReactionBuilder;
use crate::config::{
    MqttAuthMode, MqttQueryConfig, MqttReactionConfig, MqttTransportMode,
    RetainPolicy, MqttCallSpec
};
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

        // Task to handle MQTT connection events
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

        // // Main processing loop
        // match adaptive_config{
        //     None => Self::basic_processing(
        //         reaction_name.clone(),
        //         mqtt_client,
        //         priority_queue,
        //         Arc::clone(&status),
        //         config,
        //         shutdown_rx,
        //         event_loop_shutdown_tx,
        //     ).await,
        //     Some(config) => {

        //     }
        // }


        info!("[{reaction_name}] MQTT reaction stopped");
        *status.write().await = ComponentStatus::Stopped;
    }

    ////////////////////////////////////////////////////////////////////////////////
    /// BASIC PROCESSING (NO ADAPTIVE BATCHING)
    ////////////////////////////////////////////////////////////////////////////////
    
    #[allow(clippy::too_many_arguments)]
    async fn process_result(
        id: impl Into<String>,
        mqtt_client: &AsyncClient,
        handlebars: &Handlebars<'_>,
        result: &ResultDiff,
        query_name: &str,
        query_config: &MqttQueryConfig,
    ) -> Result<()> {
        match result {
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                debug!(
                    "[{}] Received aggregation or noop result, skipping MQTT call",
                    id.into()
                );
                return Ok(());
            }
            _ => {}
        }

        let mut type_str = "";
        let mut context: Map<String, Value> = Map::new();
        let mut data = match result {
            ResultDiff::Add { data } => {
                if let Some(spec) = query_config.added.as_ref() {
                    context.insert("after".to_string(), data.clone());
                    type_str = "ADD";
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
                    type_str = "UPDATE";
                    Some(data)
                } else {
                    None
                }
            }
            ResultDiff::Delete { data } => {
                if let Some(spec) = query_config.deleted.as_ref() {
                    context.insert("before".to_string(), data.clone());
                    type_str = "DELETE";
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
            let topic = call_spec.topic.clone();
            let qos = match call_spec.qos {
                crate::config::QualityOfService::AtMostOnce => QoS::AtMostOnce,
                crate::config::QualityOfService::AtLeastOnce => QoS::AtLeastOnce,
                crate::config::QualityOfService::ExactlyOnce => QoS::ExactlyOnce,
            };

            let retain = match call_spec.retain {
                RetainPolicy::Retain => true,
                RetainPolicy::NoRetain => false,
            };

            match mqtt_client.publish(topic, qos, retain, body).await {
                Ok(_) => {
                    debug!(
                        "[{}] Published MQTT message for {} operation on query '{}'",
                        id.into(),
                        type_str,
                        query_name
                    );
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to publish MQTT message for {} operation on query '{}': {}",
                        id.into(),
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
        reaction_name: String,
        mqtt_client: AsyncClient,
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
                query_configs.get("*").or_else(|| {
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
                    reaction_name.clone(),
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

    ////////////////////////////////////////////////////////////////////////////////
    /// PRIVATE HELPER METHODS
    ////////////////////////////////////////////////////////////////////////////////
    

    fn build_mqtt_options(reaction_name: impl Into<String>, config: &MqttReactionConfig) -> MqttOptions {
        let broker_addr = config.broker_addr.clone();
        let port = config.port;
        let transport_mode = config.transport_mode.clone();
        let keep_alive = config.keep_alive;
        let clean_session = config.clean_session;
        let auth_mode = config.auth_mode.clone();
        let request_channel_capacity = config.request_channel_capacity;
        let pending_throttle = config.pending_throttle;
        let max_packet_size = config.max_packet_size;
        let max_inflight = config.max_inflight;
        let connection_timeout = config.connection_timeout;

        let mut options = MqttOptions::new(reaction_name.into(), broker_addr.clone(), port);

        match transport_mode {
            MqttTransportMode::TCP => {
                options.set_transport(rumqttc::Transport::Tcp);
            }
            MqttTransportMode::TLS => {
                // TODO: Add TLS configuration options to MqttReactionConfig and set them here
            }
        }

        if let MqttAuthMode::UsernamePassword { username, password } = auth_mode {
            options.set_credentials(username, password);
        }

        options.set_outgoing_inflight_upper_limit(max_inflight);
        options.set_keep_alive(Duration::from_secs(keep_alive));
        options.set_clean_start(clean_session);
        options.set_max_packet_size(Some(max_packet_size));
        options.set_pending_throttle(Duration::from_micros(pending_throttle));
        options.set_request_channel_capacity(request_channel_capacity);
        options.set_connection_timeout(connection_timeout);

        options
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
