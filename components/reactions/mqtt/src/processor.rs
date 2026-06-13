// Copyright 2025 The Drasi Authors.
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

use crate::{client::Client, config::MqttReactionConfig};
use drasi_lib::channels::ResultDiff;
use drasi_lib::component_graph::ComponentStatusHandle;
use drasi_lib::reactions::common::templates::TemplateRouting;
use drasi_lib::reactions::common::OperationType;
use drasi_lib::reactions::ReactionBase;
use handlebars::{Context, Handlebars, Helper, HelperResult, JsonRender, Output, RenderContext};
use log::{debug, error, info, warn};
use rumqttc::v5::EventLoop as EventLoopV5;
use rumqttc::EventLoop;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::sync::oneshot;

fn json_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    if let Some(param) = h.param(0) {
        out.write(&param.value().render())?;
    } else {
        out.write("null")?;
    }
    Ok(())
}

fn validate_rendered_topic(rendered_topic: &str, template_slashes_count: usize) -> anyhow::Result<()> {
    if rendered_topic.is_empty() {
        return Err(anyhow::anyhow!("Rendered topic is empty"));
    }

    if rendered_topic.len() > u16::MAX as usize {
        return Err(anyhow::anyhow!(
            "Rendered topic UTF-8 length exceeds MQTT limit of 65535 bytes"
        ));
    }

    let bytes = rendered_topic.as_bytes();
    for i in 0..bytes.len() {
        let b = bytes[i];
        if b == 0 {
            return Err(anyhow::anyhow!("Rendered topic contains null character"));
        }

        if b == b'+' || b == b'#' {
            return Err(anyhow::anyhow!(
                "Rendered topic contains wildcard characters '+' or '#'"
            ));
        }

        if i != 0 && b == b'/' && bytes[i - 1] == b'/' {
            return Err(anyhow::anyhow!(
                "Rendered topic contains empty levels (consecutive '/')"
            ));
        }
    }

    let rendered_slashes_count = rendered_topic.matches('/').count();
    if rendered_slashes_count > template_slashes_count {
        return Err(anyhow::anyhow!(
            "Rendered topic introduces additional '/' characters from interpolation"
        ));
    }

    Ok(())
}

fn is_terminal_v5_connack(code: rumqttc::v5::mqttbytes::v5::ConnectReturnCode) -> bool {
    use rumqttc::v5::mqttbytes::v5::ConnectReturnCode as C;
    matches!(
        code,
        C::BadAuthenticationMethod
            | C::BadUserNamePassword
            | C::Banned
            | C::MalformedPacket
            | C::NotAuthorized
            | C::ProtocolError
            | C::UnsupportedProtocolVersion
    )
}

fn is_terminal_v4_connack(code: rumqttc::mqttbytes::v4::ConnectReturnCode) -> bool {
    use rumqttc::mqttbytes::v4::ConnectReturnCode as C;
    matches!(
        code,
        C::RefusedProtocolVersion | C::BadClientId | C::BadUserNamePassword | C::NotAuthorized
    )
}

pub enum MqttEventLoop {
    V5(EventLoopV5),
    V3_1_1(EventLoop),
}

pub struct ResultProcessor {
    client: Arc<Client>,
    event_loop: Option<MqttEventLoop>,
    config: MqttReactionConfig,
    base: ReactionBase,
}

impl ResultProcessor {
    pub async fn new(config: MqttReactionConfig, base: ReactionBase) -> anyhow::Result<Self> {
        let (client, event_loop) = Client::new(&config).await?;
        Ok(Self {
            client: Arc::new(client),
            event_loop: Some(event_loop),
            config,
            base,
        })
    }

    /// Start the processing loop for handling incoming MQTT messages and processing them according to the reaction's configuration.
    pub async fn start_processing_loop(
        &mut self,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        // create client arc for publishing
        let priority_queue = self.base.priority_queue.clone();
        let client = self.client.clone();
        let config = self.config.clone();
        Ok(tokio::spawn(async move {
            // Processing loop logic here
            Self::processing(client, priority_queue, shutdown_rx, config).await;
        }))
    }

    async fn processing(
        client: Arc<Client>,
        priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult>,
        mut shutdown_rx: oneshot::Receiver<()>,
        config: MqttReactionConfig,
    ) {
        // create the handlebars
        let mut handlebars = Handlebars::new();
        handlebars.register_helper("json", Box::new(json_helper));

        // start the processing loop
        loop {
            tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    // recieved shutdown signal, exit the loop
                    break;
                }

                result = priority_queue.dequeue() => {
                    let query_result = result.as_ref();

                    if(query_result.results.is_empty()) {
                        debug!("Skipping control signal for query '{}'", query_result.query_id);
                        continue;
                    } else if let Err(e) = Self::process_results(client.clone(), query_result, &handlebars, &config).await {
                        error!("Error processing query result for query '{}': {e}", query_result.query_id);
                    }
                }
            }
        }
    }

    async fn process_results(
        client: Arc<Client>,
        query_result: &drasi_lib::channels::QueryResult,
        handlebars: &Handlebars<'_>,
        config: &MqttReactionConfig,
    ) -> anyhow::Result<()> {
        let query_id = query_result.query_id.clone();

        for result in &query_result.results {
            if matches!(result, ResultDiff::Noop) {
                // skip noop results
                continue;
            }

            match ResultProcessor::process_single_result(
                client.clone(),
                result.clone(),
                handlebars,
                config,
                &query_id,
                query_result.timestamp,
            )
            .await
            {
                Ok(_) => {}
                Err(e) => {
                    error!("Error processing result for query '{query_id}': {e}");
                }
            }
        }

        Ok(())
    }

    async fn process_single_result(
        client: Arc<Client>,
        result: ResultDiff,
        handlebars: &Handlebars<'_>,
        config: &MqttReactionConfig,
        query_id: &str,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        // get the operation, data val and result type
        let (operation, data_value, result_type) = match result {
            ResultDiff::Add { data, .. } => (OperationType::Add, data, "ADD"),
            ResultDiff::Update { data, .. } => (OperationType::Update, data, "UPDATE"),
            ResultDiff::Delete { data, .. } => (OperationType::Delete, data, "DELETE"),
            ResultDiff::Aggregation { before, after, .. } => {
                let aggregation_data = json!({
                    "before": before,
                    "after": after,
                });
                (OperationType::Update, aggregation_data, "AGGREGATION")
            }
            _ => return Ok(()), // unreachable due to the check in process_results.
        };

        // build the context map for template rendering
        let mut context = Map::new();
        match result_type {
            "ADD" => {
                context.insert("after".to_string(), data_value.clone());
            }
            "UPDATE" | "AGGREGATION" => {
                if let Some(obj) = data_value.as_object() {
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    if let Some(data) = obj.get("data") {
                        context.insert("data".to_string(), data.clone());
                    }
                } else {
                    context.insert("after".to_string(), data_value.clone());
                }
            }
            "DELETE" => {
                context.insert("before".to_string(), data_value.clone());
            }
            _ => {
                return Err(anyhow::anyhow!("Unrecognized result type: {result_type}"));
            } // unreachable due to the enum match in process_single_result
        }

        // insert query metadata into the context
        context.insert(
            "query_name".to_string(),
            Value::String(query_id.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(result_type.to_string()),
        );
        context.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_rfc3339()),
        );

        let spec = config.get_template_spec(query_id, operation);

        if let Some(spec) = spec {
            let payload = if spec.extension.empty_payload {
                "".to_string()
            } else if spec.template.is_empty() {
                data_value.to_string()
            } else {
                handlebars.render_template(&spec.template, &context)?
            };

            let topic = handlebars.render_template(&spec.extension.topic, &context)?;
            if let Err(e) = validate_rendered_topic(&topic, spec.extension.slashes_count) {
                warn!(
                    "Skipping publish for query '{query_id}' due to invalid rendered topic '{topic}': {e}"
                );
                return Ok(());
            }

            client
                .publish(topic, spec.extension.qos, spec.extension.retain, payload)
                .await?;
        } else {
            warn!(
                "No matching template spec found for query '{query_id}' and operation '{result_type}'"
            );
        }

        Ok(())
    }

    /// Start the MQTT event loop in a separate task and return the handle
    pub async fn start_event_loop(
        &mut self,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let status_handle = self.base.status_handle();
        match &self.event_loop {
            Some(MqttEventLoop::V5(event_loop)) => {
                self.start_event_loop_v5(shutdown_rx, status_handle).await
            }
            Some(MqttEventLoop::V3_1_1(event_loop)) => {
                self.start_event_loop_v3_1_1(shutdown_rx, status_handle)
                    .await
            }
            None => Err(anyhow::anyhow!("MQTT event loop not initialized")),
        }
    }

    /// Start the event loop for MQTT v3.1.1
    async fn start_event_loop_v3_1_1(
        &mut self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        status_handle: ComponentStatusHandle,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        // take the ownership of the event loop
        let mut event_loop = match self.event_loop.take() {
            Some(MqttEventLoop::V3_1_1(event_loop)) => event_loop,
            _ => {
                return Err(anyhow::anyhow!(
                    "MQTT event loop not initialized properly for v3.1.1"
                ))
            }
        };

        let client_id = self
            .config
            .client_id
            .clone()
            .unwrap_or_else(|| " ".to_string());

        // start polling the event loop
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("[{client_id}] Received MQTT event loop shutdown signal, exiting event loop");
                        break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(connack))) => {
                                if is_terminal_v4_connack(connack.code) {
                                    error!("[{client_id}] MQTT v3.1.1 terminal CONNACK reason: {:?}", connack.code);
                                    status_handle.set_status(
                                        drasi_lib::ComponentStatus::Error,
                                        Some(format!("MQTT terminal CONNACK reason: {:?}", connack.code))
                                    ).await;
                                    break;
                                }
                            }
                            Ok(_) => {},
                            Err(e) => {
                                match &e {
                                    rumqttc::ConnectionError::ConnectionRefused(code) if is_terminal_v4_connack(*code) => {
                                        error!("[{client_id}] MQTT event loop terminal error: {e}");
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("MQTT event loop terminal error: {e}"))
                                        ).await;
                                        break;
                                    }
                                    _ => {
                                        warn!("[{client_id}] MQTT event loop transient error: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Start the event loop for MQTT v5
    async fn start_event_loop_v5(
        &mut self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        status_handle: ComponentStatusHandle,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        // take the ownership of the event loop
        let mut event_loop = match self.event_loop.take() {
            Some(MqttEventLoop::V5(event_loop)) => event_loop,
            _ => {
                return Err(anyhow::anyhow!(
                    "MQTT event loop not initialized properly for v5"
                ))
            }
        };

        let client_id = self
            .config
            .client_id
            .clone()
            .unwrap_or_else(|| " ".to_string());

        // start polling the event loop
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                    info!("[{client_id}] Received MQTT event loop shutdown signal, exiting event loop");
                    break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(rumqttc::v5::Event::Incoming(rumqttc::v5::mqttbytes::v5::Packet::ConnAck(connack))) => {
                                if is_terminal_v5_connack(connack.code) {
                                    error!("[{client_id}] MQTT v5 terminal CONNACK reason: {:?}", connack.code);
                                    status_handle.set_status(
                                        drasi_lib::ComponentStatus::Error,
                                        Some(format!("MQTT terminal CONNACK reason: {:?}", connack.code))
                                    ).await;
                                    break;
                                }
                            }
                            Ok(_) => {},
                            Err(e) => {
                                match &e {
                                    rumqttc::v5::ConnectionError::ConnectionRefused(code) if is_terminal_v5_connack(*code) => {
                                        error!("[{client_id}] MQTT event loop terminal error: {e}");
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("MQTT event loop terminal error: {e}"))
                                        ).await;
                                        break;
                                    }
                                    _ => {
                                        warn!("[{client_id}] MQTT event loop transient error: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::validate_rendered_topic;

    #[test]
    fn rendered_topic_rejects_wildcards() {
        assert!(validate_rendered_topic("stocks/+/updated", 2).is_err());
        assert!(validate_rendered_topic("stocks/#", 1).is_err());
    }

    #[test]
    fn rendered_topic_rejects_empty_levels() {
        assert!(validate_rendered_topic("stocks//updated", 2).is_err());
    }

    #[test]
    fn rendered_topic_rejects_slash_interpolation() {
        let template_slashes = "stocks/{{after.symbol}}/updated".matches('/').count();
        assert!(validate_rendered_topic("stocks/aapl/extra/updated", template_slashes).is_err());
    }

    #[test]
    fn rendered_topic_accepts_valid() {
        let template_slashes = "stocks/{{after.symbol}}/updated".matches('/').count();
        assert!(validate_rendered_topic("stocks/AAPL/updated", template_slashes).is_ok());
    }
}
