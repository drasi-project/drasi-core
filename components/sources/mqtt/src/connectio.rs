use core::error;

use crate::SourceChangeEvent;
use crate::{
    config::MQTTConnectionConfig,
    model::{convert_mqtt_to_source_change, map_json_to_mqtt_source_change, MqttSourceChange},
    MqttAppState,
};
use anyhow::Result;
use log::{debug, error, info, trace, warn};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, QoS};
pub struct MQTTConnectionWrapper {
    client: Box<AsyncClient>,
    id: String,
    eventloop: Box<EventLoop>,
    options: Box<MqttOptions>,
    state: MqttAppState,
    qos: QoS,
    channel_capacity: usize,
    timeout_ms: u64,
    topic: String,
}

impl MQTTConnectionWrapper {
    pub fn new(config: MQTTConnectionConfig, state: MqttAppState) -> Self {
        let (client, eventloop) = AsyncClient::new(config.options.clone(), config.channel_capacity);
        Self {
            client: Box::new(client),
            eventloop: Box::new(eventloop),
            options: Box::new(config.options),
            state,
            qos: config.qos,
            channel_capacity: config.channel_capacity,
            timeout_ms: config.timeout_ms,
            topic: config.topic,
            id: config.id,
        }
    }

    pub fn client(&self) -> &AsyncClient {
        &self.client
    }

    pub fn eventloop(&self) -> &EventLoop {
        &self.eventloop
    }

    pub fn options(&self) -> &MqttOptions {
        &self.options
    }

    pub async fn start(&mut self, error_tx: tokio::sync::oneshot::Sender<String>) {
        match self.client.subscribe(self.topic.clone(), self.qos).await {
            Ok(_) => {
                info!("[{}] Successfully subscribed to topic '{}'", self.id, self.topic);
            }
            Err(e) => {
                error!("[{}] Failed to subscribe to topic '{}': {:?}", self.id, self.topic, e);
                let _ = error_tx.send(format!("{:?}", e));
                return;
            }
        }
        loop {
            let event = match self.eventloop.poll().await {
                Ok(event) => event,
                Err(e) => {
                    error!("[{}] MQTT event loop error: {:?}", self.id, e);
                    let _ = error_tx.send(format!("{:?}", e));
                    break;
                }
            };
            match event {
                Event::Incoming(incoming) => match incoming {
                    Incoming::Publish(publish) => {
                        match map_json_to_mqtt_source_change(&String::from_utf8_lossy(
                            &publish.payload,
                        )) {
                            Ok(event) => {
                                Self::process_events(&self.id, &self.state, vec![event]).await;
                            }
                            Err(e) => {
                                error!("[{}] Failed to map MQTT payload to source change: {:?}", self.id, e);
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    async fn process_events(source_id: &str, state: &MqttAppState, events: Vec<MqttSourceChange>) {
        trace!("[{}] Processing MQTT event", source_id);

        let mut success_count = 0;
        let mut error_count = 0;

        for (idx, event) in events.iter().enumerate() {
            match convert_mqtt_to_source_change(&event, source_id) {
                Ok(source_change) => {
                    let change_event = SourceChangeEvent {
                        source_id: source_id.to_string(),
                        change: source_change,
                        timestamp: chrono::Utc::now(),
                    };

                    if let Err(e) = state.batch_tx.send(change_event).await {
                        error!(
                                "[{}] Failed to send SourceChangeEvent (event {}) to batch channel: {:?}",
                                source_id, idx + 1, e
                            );
                        error_count += 1;
                    } else {
                        debug!(
                            "[{}] Successfully sent SourceChangeEvent (event {}) to batch channel",
                            source_id,
                            idx + 1
                        );
                        success_count += 1;
                    }
                }
                Err(e) => {
                    error_count += 1;
                }
            }
        }
        trace!(
            "[{}] Finished processing MQTT events: {} successful, {} errors",
            source_id,
            success_count,
            error_count
        );
    }
}
