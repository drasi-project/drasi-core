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
use drasi_lib::reactions::{self, ReactionBase};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use log::{debug, error, info, warn};
use rumqttc::v5::EventLoop as EventLoopV5;
use rumqttc::EventLoop;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

fn json_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    if let Some(param) = h.param(0) {
        let rendered = serde_json::to_string(param.value()).map_err(RenderErrorReason::from)?;
        out.write(&rendered)?;
    } else {
        out.write("null")?;
    }
    Ok(())
}

fn validate_rendered_topic(
    rendered_topic: &str,
    original_template_slash_count: usize,
) -> anyhow::Result<()> {
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
    if rendered_slashes_count > original_template_slash_count {
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

fn is_terminal_v5_disconnect(code: rumqttc::v5::mqttbytes::v5::DisconnectReasonCode) -> bool {
    use rumqttc::v5::mqttbytes::v5::DisconnectReasonCode as C;
    matches!(
        code,
        C::MalformedPacket
            | C::ProtocolError
            | C::ImplementationSpecificError
            | C::NotAuthorized
            | C::TopicFilterInvalid
            | C::TopicNameInvalid
            | C::ReceiveMaximumExceeded
            | C::TopicAliasInvalid
            | C::PacketTooLarge
            | C::PayloadFormatInvalid
            | C::RetainNotSupported
            | C::QoSNotSupported
            | C::SharedSubscriptionNotSupported
            | C::SubscriptionIdentifiersNotSupported
            | C::WildcardSubscriptionsNotSupported
    )
}

fn is_terminal_v4_connack(code: rumqttc::mqttbytes::v4::ConnectReturnCode) -> bool {
    use rumqttc::mqttbytes::v4::ConnectReturnCode as C;
    matches!(
        code,
        C::RefusedProtocolVersion | C::BadClientId | C::BadUserNamePassword | C::NotAuthorized
    )
}

#[derive(Debug, Clone)]
struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    fn new() -> Self {
        Self {
            next: INITIAL_RECONNECT_BACKOFF,
        }
    }

    fn reset(&mut self) {
        self.next = INITIAL_RECONNECT_BACKOFF;
    }

    fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = base.saturating_mul(2).min(MAX_RECONNECT_BACKOFF);
        jittered_backoff(base, reconnect_jitter_seed())
    }
}

fn reconnect_jitter_seed() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn jittered_backoff(base: Duration, seed: u128) -> Duration {
    let max_jitter_millis = (base.as_millis() / 2).max(1);
    let jitter_millis = seed % (max_jitter_millis + 1);
    base + Duration::from_millis(jitter_millis as u64)
}

async fn wait_for_reconnect_backoff(
    reaction_id: &str,
    shutdown_rx: &mut oneshot::Receiver<()>,
    delay: Duration,
) -> bool {
    tokio::select! {
        _ = &mut *shutdown_rx => {
            info!("[{reaction_id}] Received MQTT event loop shutdown signal, exiting event loop");
            true
        }
        _ = tokio::time::sleep(delay) => false,
    }
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
        let (client, event_loop) = if let Some(client_id) = &config.client_id {
            Client::new(client_id.clone(), &config).await?
        } else {
            Client::new(base.id.clone(), &config).await?
        };

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
        let reaction_id = self.base.id.clone();
        Ok(tokio::spawn(async move {
            // Processing loop logic here
            Self::processing(reaction_id, client, priority_queue, shutdown_rx, config).await;
        }))
    }

    async fn processing(
        reaction_id: String,
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
                    // received shutdown signal, exit the loop
                    break;
                }

                result = priority_queue.dequeue() => {
                    let query_result = result.as_ref();

                    if query_result.results.is_empty() {
                        debug!("[{reaction_id}] Skipping control signal for query '{}'", query_result.query_id);
                        continue;
                    } else if let Err(e) = Self::process_results(reaction_id.clone(), client.clone(), query_result, &handlebars, &config).await {
                        warn!("[{reaction_id}] Error processing query result for query '{}': {e}", query_result.query_id);
                    }
                }
            }
        }
    }

    async fn process_results(
        reaction_id: String,
        client: Arc<Client>,
        query_result: &drasi_lib::channels::QueryResult,
        handlebars: &Handlebars<'_>,
        config: &MqttReactionConfig,
    ) -> anyhow::Result<()> {
        let query_id = query_result.query_id.clone();

        for result in &query_result.results {
            if matches!(result, ResultDiff::Noop) {
                // skip noop results
                debug!("[{reaction_id}] Skipping noop result for query '{query_id}'");
                continue;
            }

            match ResultProcessor::process_single_result(
                reaction_id.clone(),
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
                    warn!("Error processing result for query '{query_id}': {e}");
                }
            }
        }

        Ok(())
    }

    async fn process_single_result(
        reaction_id: String,
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
            if let Err(e) = validate_rendered_topic(&topic, spec.extension.slash_count) {
                warn!(
                    "[{reaction_id}] Skipping publish for query '{query_id}' due to invalid rendered topic '{topic}': {e}"
                );
                return Ok(());
            }

            match client
                .publish(
                    topic.clone(),
                    spec.extension.qos,
                    spec.extension.retain,
                    payload.clone(),
                    spec.extension.message_expiry_interval,
                )
                .await
            {
                Ok(_) => {
                    debug!(
                            "[{reaction_id}] Punlished MQTT message for query '{query_id}' on topic '{topic}' with operation '{result_type}' with payload size {} bytes",
                            payload.len()
                        );
                }
                Err(e) => {
                    warn!(
                            "[{reaction_id}] Failed to publish MQTT message for query '{query_id}' on topic '{topic}': {e}"
                        );
                }
            }
        } else {
            warn!(
                "[{reaction_id}] No matching template spec found for query '{query_id}' and operation '{result_type}'"
            );
        }

        Ok(())
    }

    /// start the event loop for processing incoming MQTT messages.
    pub async fn start_event_loop(
        &mut self,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let status_handle = self.base.status_handle();
        match &self.event_loop {
            Some(MqttEventLoop::V5(_)) => {
                self.start_event_loop_v5(shutdown_rx, status_handle).await
            }
            Some(MqttEventLoop::V3_1_1(_)) => {
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
        let reaction_id = self.base.id.clone();
        // take the ownership of the event loop
        let mut event_loop = match self.event_loop.take() {
            Some(MqttEventLoop::V3_1_1(event_loop)) => event_loop,
            _ => {
                return Err(anyhow::anyhow!(
                    "MQTT event loop not initialized properly for v3.1.1"
                ))
            }
        };

        // start polling the event loop
        let handle = tokio::spawn(async move {
            let mut reconnect_backoff = ReconnectBackoff::new();

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("[{reaction_id}] Received MQTT event loop shutdown signal, exiting event loop");
                        break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(event) => {
                                if let rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(connack)) = event {
                                    if is_terminal_v4_connack(connack.code) {
                                        error!("[{reaction_id}] MQTT v3.1.1 terminal CONNACK reason: {:?}", connack.code);
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("[{reaction_id}] MQTT terminal CONNACK reason: {:?}", connack.code))
                                        ).await;
                                        break;
                                    }
                                }
                                reconnect_backoff.reset();
                            }
                            Err(e) => {
                                match &e {
                                    rumqttc::ConnectionError::ConnectionRefused(code) if is_terminal_v4_connack(*code) => {
                                        error!("[{reaction_id}] MQTT event loop terminal error: {e}");
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("[{reaction_id}] MQTT event loop terminal error: {e}"))
                                        ).await;
                                        break;
                                    }
                                    _ => {
                                        let delay = reconnect_backoff.next_delay();
                                        warn!("[{reaction_id}] MQTT event loop transient error: {e}; retrying in {delay:?}");
                                        if wait_for_reconnect_backoff(&reaction_id, &mut shutdown_rx, delay).await {
                                            break;
                                        }
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
        let reaction_id = self.base.id.clone();
        // take the ownership of the event loop
        let mut event_loop = match self.event_loop.take() {
            Some(MqttEventLoop::V5(event_loop)) => event_loop,
            _ => {
                return Err(anyhow::anyhow!(
                    "MQTT event loop not initialized properly for v5"
                ))
            }
        };

        // start polling the event loop
        let handle = tokio::spawn(async move {
            let mut reconnect_backoff = ReconnectBackoff::new();

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                    info!("[{reaction_id}] Received MQTT event loop shutdown signal, exiting event loop");
                    break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(event) => {
                                if let rumqttc::v5::Event::Incoming(rumqttc::v5::mqttbytes::v5::Packet::ConnAck(connack)) = event {
                                    if is_terminal_v5_connack(connack.code) {
                                        error!("[{reaction_id}] MQTT v5 terminal CONNACK reason: {:?}", connack.code);
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("[{reaction_id}] MQTT terminal CONNACK reason: {:?}", connack.code))
                                        ).await;
                                        break;
                                    }
                                }
                                reconnect_backoff.reset();
                            }
                            Err(e) => {
                                match &e {
                                    rumqttc::v5::ConnectionError::ConnectionRefused(code) if is_terminal_v5_connack(*code) => {
                                        error!("[{reaction_id}] MQTT event loop terminal error: {e}");
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("[{reaction_id}] MQTT event loop terminal error: {e}"))
                                        ).await;
                                        break;
                                    }
                                    rumqttc::v5::ConnectionError::MqttState(
                                        rumqttc::v5::StateError::ServerDisconnect {
                                            reason_code,
                                            reason_string,
                                        }
                                    ) if is_terminal_v5_disconnect(*reason_code) => {
                                        error!("[{reaction_id}] MQTT event loop terminal disconnect: {reason_code:?}, reason: {reason_string:?}");
                                        status_handle.set_status(
                                            drasi_lib::ComponentStatus::Error,
                                            Some(format!("[{reaction_id}] MQTT terminal disconnect reason: {reason_code:?}"))
                                        ).await;
                                        break;
                                    }
                                    _ => {
                                        let delay = reconnect_backoff.next_delay();
                                        warn!("[{reaction_id}] MQTT event loop transient error: {e}; retrying in {delay:?}");
                                        if wait_for_reconnect_backoff(&reaction_id, &mut shutdown_rx, delay).await {
                                            break;
                                        }
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
    use super::{
        is_terminal_v4_connack, is_terminal_v5_connack, is_terminal_v5_disconnect,
        jittered_backoff, validate_rendered_topic, wait_for_reconnect_backoff, ReconnectBackoff,
        INITIAL_RECONNECT_BACKOFF, MAX_RECONNECT_BACKOFF,
    };
    use handlebars::Handlebars;
    use rumqttc::mqttbytes::v4::ConnectReturnCode as ConnectCodeV4;
    use rumqttc::v5::mqttbytes::v5::ConnectReturnCode as ConnectCodeV5;
    use rumqttc::v5::mqttbytes::v5::DisconnectReasonCode as DisconnectCode;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn handlebars_with_json_helper() -> Handlebars<'static> {
        let mut handlebars = Handlebars::new();
        handlebars.register_helper("json", Box::new(super::json_helper));
        handlebars
    }

    #[test]
    fn rendered_topic_rejects_wildcards() {
        assert!(validate_rendered_topic("stocks/+/updated", 2).is_err());
        assert!(validate_rendered_topic("stocks/#", 2).is_err());
    }

    #[test]
    fn rendered_topic_rejects_empty_string() {
        assert!(validate_rendered_topic("", 2).is_err());
    }

    #[test]
    fn rendered_topic_rejects_null_character() {
        assert!(validate_rendered_topic("stocks/ab\0cd/updated", 2).is_err());
    }

    #[test]
    fn rendered_topic_rejects_empty_levels() {
        assert!(validate_rendered_topic("stocks//updated", 2).is_err());
    }

    #[test]
    fn rendered_topic_rejects_slash_interpolation() {
        assert!(validate_rendered_topic("stocks/aapl/extra/updated", 2).is_err());
    }

    #[test]
    fn rendered_topic_accepts_valid() {
        assert!(validate_rendered_topic("stocks/AAPL/updated", 2).is_ok());
    }

    #[test]
    fn rendered_topic_rejects_over_mqtt_length_limit() {
        let too_long = "a".repeat(65_536);
        assert!(validate_rendered_topic(&too_long, 2).is_err());
    }

    #[test]
    fn json_helper_renders_full_payload_as_valid_json() {
        let handlebars = handlebars_with_json_helper();
        let data = json!({
            "after": {
                "id": "sensor\"<&>",
                "temp": 22.4,
                "active": true,
                "tags": ["lab", "mqtt"],
                "metadata": { "floor": 3 },
                "optional": null
            }
        });

        let rendered = handlebars.render_template("{{json after}}", &data).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed, data["after"]);
    }

    #[test]
    fn json_helper_keeps_hand_built_json_valid() {
        let handlebars = handlebars_with_json_helper();
        let data = json!({
            "after": {
                "id": "sensor\"<&>",
                "temp": 22.4,
                "optional": null
            }
        });

        let rendered = handlebars
            .render_template(
                r#"{"id": {{json after.id}}, "temp": {{json after.temp}}, "optional": {{json after.optional}}}"#,
                &data,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            parsed,
            json!({
                "id": "sensor\"<&>",
                "temp": 22.4,
                "optional": null
            })
        );
    }

    #[test]
    fn raw_interpolation_still_html_escapes_plain_text() {
        let handlebars = handlebars_with_json_helper();
        let data = json!({
            "after": {
                "id": "sensor\"<&>",
                "temp": 22.4
            }
        });

        let raw = handlebars
            .render_template("{{after.id}} -> {{after.temp}}", &data)
            .unwrap();
        let json = handlebars
            .render_template("{{json after.id}} -> {{json after.temp}}", &data)
            .unwrap();

        assert_eq!(raw, "sensor&quot;&lt;&amp;&gt; -> 22.4");
        assert_eq!(json, r#""sensor\"<&>" -> 22.4"#);
    }

    #[test]
    fn reconnect_backoff_doubles_until_cap() {
        let mut backoff = ReconnectBackoff::new();

        assert!(backoff.next_delay() >= INITIAL_RECONNECT_BACKOFF);
        assert_eq!(backoff.next, INITIAL_RECONNECT_BACKOFF * 2);

        for _ in 0..16 {
            backoff.next_delay();
        }

        assert_eq!(backoff.next, MAX_RECONNECT_BACKOFF);

        backoff.reset();
        assert_eq!(backoff.next, INITIAL_RECONNECT_BACKOFF);
    }

    #[test]
    fn reconnect_backoff_adds_bounded_jitter() {
        let base = Duration::from_secs(10);
        let delay = jittered_backoff(base, 4_999);

        assert!(delay >= base);
        assert!(delay <= base + Duration::from_secs(5));
    }

    #[test]
    fn reconnect_backoff_uses_zero_jitter_for_seed_zero() {
        let base = Duration::from_secs(3);
        let delay = jittered_backoff(base, 0);
        assert_eq!(delay, base);
    }

    #[tokio::test]
    async fn wait_for_reconnect_backoff_returns_false_when_delay_elapses() {
        let (_tx, mut rx) = oneshot::channel();
        let should_stop =
            wait_for_reconnect_backoff("test-rx", &mut rx, Duration::from_millis(1)).await;
        assert!(!should_stop);
    }

    #[tokio::test]
    async fn wait_for_reconnect_backoff_returns_true_on_shutdown_signal() {
        let (tx, mut rx) = oneshot::channel();
        let _ = tx.send(());
        let should_stop =
            wait_for_reconnect_backoff("test-rx", &mut rx, Duration::from_secs(5)).await;
        assert!(should_stop);
    }

    #[test]
    fn mqtt_v5_connack_classification_matches_terminal_reasons() {
        assert!(is_terminal_v5_connack(ConnectCodeV5::BadUserNamePassword));
        assert!(is_terminal_v5_connack(ConnectCodeV5::ProtocolError));
        assert!(is_terminal_v5_connack(ConnectCodeV5::NotAuthorized));

        assert!(!is_terminal_v5_connack(ConnectCodeV5::Success));
        assert!(!is_terminal_v5_connack(ConnectCodeV5::ServerBusy));
        assert!(!is_terminal_v5_connack(ConnectCodeV5::UseAnotherServer));
    }

    #[test]
    fn mqtt_v4_connack_classification_matches_terminal_reasons() {
        assert!(is_terminal_v4_connack(
            ConnectCodeV4::RefusedProtocolVersion
        ));
        assert!(is_terminal_v4_connack(ConnectCodeV4::BadClientId));
        assert!(is_terminal_v4_connack(ConnectCodeV4::BadUserNamePassword));
        assert!(is_terminal_v4_connack(ConnectCodeV4::NotAuthorized));

        assert!(!is_terminal_v4_connack(ConnectCodeV4::Success));
        assert!(!is_terminal_v4_connack(ConnectCodeV4::ServiceUnavailable));
    }

    #[test]
    fn mqtt_v5_disconnect_classification_matches_terminal_reasons() {
        assert!(is_terminal_v5_disconnect(DisconnectCode::NotAuthorized));
        assert!(is_terminal_v5_disconnect(DisconnectCode::ProtocolError));
        assert!(is_terminal_v5_disconnect(DisconnectCode::QoSNotSupported));

        assert!(!is_terminal_v5_disconnect(DisconnectCode::ServerBusy));
        assert!(!is_terminal_v5_disconnect(
            DisconnectCode::ServerShuttingDown
        ));
        assert!(!is_terminal_v5_disconnect(
            DisconnectCode::AdministrativeAction
        ));
        assert!(!is_terminal_v5_disconnect(DisconnectCode::SessionTakenOver));
    }
}
