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

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};

use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::{base::SourceBase, Source};
use crate::utils::*;

// Include generated protobuf code
pub mod proto {
    tonic::include_proto!("drasi.v1");
}

use proto::{
    source_service_server::{SourceService, SourceServiceServer},
    BootstrapRequest as ProtoBootstrapRequest, BootstrapResponse, HealthCheckResponse,
    SourceChange as ProtoSourceChange, StreamEventResponse, SubmitEventRequest,
    SubmitEventResponse,
};

/// gRPC source that exposes a gRPC endpoint to receive SourceChangeEvents
pub struct GrpcSource {
    base: SourceBase,
}

impl GrpcSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }
}

#[async_trait]
impl Source for GrpcSource {
    async fn start(&self) -> Result<()> {
        log_component_start("gRPC Source", &self.base.config.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting gRPC source".to_string()),
            )
            .await?;

        // Get configuration
        let port = self
            .base
            .config
            .properties
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(50051) as u16;

        let host = self
            .base
            .config
            .properties
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let addr = format!("{}:{}", host, port).parse()?;

        info!(
            "gRPC source '{}' listening on {}",
            self.base.config.id, addr
        );

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        // Create gRPC service
        let service = GrpcSourceService {
            source_id: self.base.config.id.clone(),
            dispatchers: self.base.dispatchers.clone(),
        };

        let svc = SourceServiceServer::new(service);

        // Start the gRPC server
        let status = Arc::clone(&self.base.status);
        let source_id = self.base.config.id.clone();
        let event_tx = self.base.event_tx.clone();

        let task = tokio::spawn(async move {
            *status.write().await = ComponentStatus::Running;

            let running_event = ComponentEvent {
                component_id: source_id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some(format!("gRPC source listening on {}", addr)),
            };

            if let Err(e) = event_tx.send(running_event).await {
                error!("Failed to send component event: {}", e);
            }

            // Run the server with graceful shutdown
            let server = Server::builder()
                .add_service(svc)
                .serve_with_shutdown(addr, async move {
                    let _ = shutdown_rx.await;
                    debug!("gRPC source received shutdown signal");
                });

            if let Err(e) = server.await {
                error!("gRPC server error: {}", e);
            }

            *status.write().await = ComponentStatus::Stopped;
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("gRPC Source", &self.base.config.id);
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &SourceConfig {
        &self.base.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(
                query_id,
                enable_bootstrap,
                node_labels,
                relation_labels,
                "gRPC",
            )
            .await
    }


    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// gRPC service implementation
struct GrpcSourceService {
    source_id: String,
    dispatchers: Arc<RwLock<Vec<Box<dyn crate::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
}

#[tonic::async_trait]
impl SourceService for GrpcSourceService {
    async fn submit_event(
        &self,
        request: Request<SubmitEventRequest>,
    ) -> Result<Response<SubmitEventResponse>, Status> {
        let event_request = request.into_inner();

        if let Some(proto_change) = event_request.event {
            match convert_proto_to_source_change(&proto_change, &self.source_id) {
                Ok(source_change) => {
                    // Create profiling metadata with timestamps
                    let mut profiling = crate::profiling::ProfilingMetadata::new();
                    profiling.source_send_ns = Some(crate::profiling::timestamp_ns());

                    let wrapper = SourceEventWrapper::with_profiling(
                        self.source_id.clone(),
                        SourceEvent::Change(source_change),
                        chrono::Utc::now(),
                        profiling,
                    );

                    debug!("[{}] Processing gRPC event: {:?}", self.source_id, &wrapper);

                    // Dispatch via helper (using block_on for sync context)
                    if let Err(e) = futures::executor::block_on(SourceBase::dispatch_from_task(
                        self.dispatchers.clone(),
                        wrapper,
                        &self.source_id,
                    )) {
                        debug!(
                            "[{}] Failed to dispatch (no subscribers): {}",
                            self.source_id, e
                        );
                    }

                    debug!("[{}] Successfully processed gRPC event", self.source_id);
                    Ok(Response::new(SubmitEventResponse {
                        success: true,
                        message: "Event processed successfully".to_string(),
                        error: String::new(),
                        event_id: uuid::Uuid::new_v4().to_string(),
                    }))
                }
                Err(e) => {
                    error!("[{}] Invalid event data: {}", self.source_id, e);
                    Ok(Response::new(SubmitEventResponse {
                        success: false,
                        message: "Invalid event data".to_string(),
                        error: e.to_string(),
                        event_id: String::new(),
                    }))
                }
            }
        } else {
            Ok(Response::new(SubmitEventResponse {
                success: false,
                message: "No event provided".to_string(),
                error: "Event is required".to_string(),
                event_id: String::new(),
            }))
        }
    }

    type StreamEventsStream =
        tokio_stream::wrappers::ReceiverStream<Result<StreamEventResponse, Status>>;

    async fn stream_events(
        &self,
        request: Request<tonic::Streaming<ProtoSourceChange>>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        let mut stream = request.into_inner();
        let source_id = self.source_id.clone();
        let dispatchers = self.dispatchers.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(128);

        tokio::spawn(async move {
            let mut events_processed = 0u64;

            while let Ok(Some(proto_change)) = stream.message().await {
                match convert_proto_to_source_change(&proto_change, &source_id) {
                    Ok(source_change) => {
                        // Create profiling metadata with timestamps
                        let mut profiling = crate::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(crate::profiling::timestamp_ns());

                        let wrapper = SourceEventWrapper::with_profiling(
                            source_id.clone(),
                            SourceEvent::Change(source_change),
                            chrono::Utc::now(),
                            profiling,
                        );

                        // Dispatch via helper
                        if let Err(e) = SourceBase::dispatch_from_task(
                            dispatchers.clone(),
                            wrapper.clone(),
                            &source_id,
                        )
                        .await
                        {
                            debug!(
                                "[{}] Failed to dispatch (no subscribers): {}",
                                source_id, e
                            );
                        }

                        events_processed += 1;

                        // Send periodic updates
                        if events_processed % 100 == 0 {
                            let _ = tx
                                .send(Ok(StreamEventResponse {
                                    success: true,
                                    message: format!("Processed {} events", events_processed),
                                    error: String::new(),
                                    events_processed,
                                }))
                                .await;
                        }
                    }
                    Err(e) => {
                        error!("[{}] Invalid event data: {}", source_id, e);
                        let _ = tx
                            .send(Ok(StreamEventResponse {
                                success: false,
                                message: "Invalid event data".to_string(),
                                error: e.to_string(),
                                events_processed,
                            }))
                            .await;
                    }
                }
            }

            // Send final response
            let _ = tx
                .send(Ok(StreamEventResponse {
                    success: true,
                    message: format!("Stream completed. Processed {} events", events_processed),
                    error: String::new(),
                    events_processed,
                }))
                .await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type RequestBootstrapStream =
        tokio_stream::wrappers::ReceiverStream<Result<BootstrapResponse, Status>>;

    async fn request_bootstrap(
        &self,
        _request: Request<ProtoBootstrapRequest>,
    ) -> Result<Response<Self::RequestBootstrapStream>, Status> {
        // For now, return empty stream
        // This could be extended to support bootstrap
        let (tx, rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(async move {
            let _ = tx
                .send(Ok(BootstrapResponse {
                    elements: vec![],
                    total_count: 0,
                }))
                .await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn health_check(
        &self,
        _request: Request<()>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            status: proto::health_check_response::Status::Healthy as i32,
            message: "gRPC source is healthy".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }
}

/// Convert protobuf SourceChange to Drasi Core SourceChange
fn convert_proto_to_source_change(
    proto_change: &ProtoSourceChange,
    source_id: &str,
) -> Result<drasi_core::models::SourceChange> {
    use drasi_core::models::SourceChange;
    use proto::source_change::Change;

    let change_type = proto::ChangeType::try_from(proto_change.r#type)
        .map_err(|_| anyhow::anyhow!("Invalid change type"))?;

    match (change_type, &proto_change.change) {
        (
            proto::ChangeType::Insert | proto::ChangeType::Update,
            Some(Change::Element(proto_element)),
        ) => {
            let element = convert_proto_element_to_core(proto_element, source_id)?;

            if change_type == proto::ChangeType::Insert {
                Ok(SourceChange::Insert { element })
            } else {
                Ok(SourceChange::Update { element })
            }
        }
        (proto::ChangeType::Delete, Some(Change::Metadata(proto_metadata))) => {
            let metadata = convert_proto_metadata_to_core(proto_metadata, source_id)?;
            Ok(SourceChange::Delete { metadata })
        }
        _ => Err(anyhow::anyhow!("Invalid change type or missing data")),
    }
}

/// Convert protobuf Element to Drasi Core Element
fn convert_proto_element_to_core(
    proto_element: &proto::Element,
    source_id: &str,
) -> Result<drasi_core::models::Element> {
    use drasi_core::models::{Element, ElementReference};
    use proto::element::Element as ProtoElementType;

    match &proto_element.element {
        Some(ProtoElementType::Node(node)) => {
            let metadata = node
                .metadata
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Node missing metadata"))?;

            let metadata = convert_proto_metadata_to_core(metadata, source_id)?;
            let properties = convert_proto_properties(&node.properties)?;

            Ok(Element::Node {
                metadata,
                properties,
            })
        }
        Some(ProtoElementType::Relation(relation)) => {
            let metadata = relation
                .metadata
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Relation missing metadata"))?;

            let metadata = convert_proto_metadata_to_core(metadata, source_id)?;
            let properties = convert_proto_properties(&relation.properties)?;

            let in_node = relation
                .in_node
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Relation missing in_node"))?;
            let out_node = relation
                .out_node
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Relation missing out_node"))?;

            Ok(Element::Relation {
                metadata,
                properties,
                in_node: ElementReference {
                    source_id: Arc::from(in_node.source_id.as_str()),
                    element_id: Arc::from(in_node.element_id.as_str()),
                },
                out_node: ElementReference {
                    source_id: Arc::from(out_node.source_id.as_str()),
                    element_id: Arc::from(out_node.element_id.as_str()),
                },
            })
        }
        None => Err(anyhow::anyhow!("Element type not specified")),
    }
}

/// Convert protobuf ElementMetadata to Drasi Core ElementMetadata
fn convert_proto_metadata_to_core(
    proto_metadata: &proto::ElementMetadata,
    source_id: &str,
) -> Result<drasi_core::models::ElementMetadata> {
    use drasi_core::models::{ElementMetadata, ElementReference};

    let reference = proto_metadata
        .reference
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Metadata missing reference"))?;

    Ok(ElementMetadata {
        reference: ElementReference {
            source_id: Arc::from(source_id),
            element_id: Arc::from(reference.element_id.as_str()),
        },
        labels: Arc::from(
            proto_metadata
                .labels
                .iter()
                .map(|s| Arc::from(s.as_str()))
                .collect::<Vec<_>>(),
        ),
        effective_from: proto_metadata.effective_from,
    })
}

/// Convert protobuf Struct to ElementPropertyMap
fn convert_proto_properties(
    props: &Option<prost_types::Struct>,
) -> Result<drasi_core::models::ElementPropertyMap> {
    use drasi_core::models::ElementPropertyMap;

    let mut properties = ElementPropertyMap::new();

    if let Some(struct_props) = props {
        for (key, value) in &struct_props.fields {
            properties.insert(key, convert_proto_value_to_element_value(value)?);
        }
    }

    Ok(properties)
}

/// Convert protobuf Value to ElementValue
fn convert_proto_value_to_element_value(
    value: &prost_types::Value,
) -> Result<drasi_core::models::ElementValue> {
    use drasi_core::models::ElementValue;
    use ordered_float::OrderedFloat;
    use prost_types::value::Kind;

    match &value.kind {
        Some(Kind::NullValue(_)) => Ok(ElementValue::Null),
        Some(Kind::BoolValue(b)) => Ok(ElementValue::Bool(*b)),
        Some(Kind::NumberValue(n)) => {
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Ok(ElementValue::Integer(*n as i64))
            } else {
                Ok(ElementValue::Float(OrderedFloat(*n)))
            }
        }
        Some(Kind::StringValue(s)) => Ok(ElementValue::String(Arc::from(s.as_str()))),
        Some(Kind::ListValue(_)) | Some(Kind::StructValue(_)) => {
            // For complex types, convert to JSON string
            let json_val = proto_value_to_json(value);
            Ok(ElementValue::String(Arc::from(serde_json::to_string(
                &json_val,
            )?)))
        }
        None => Ok(ElementValue::Null),
    }
}

/// Convert protobuf Value to JSON for complex types
fn proto_value_to_json(value: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;

    match &value.kind {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::json!(*n),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            let arr: Vec<serde_json::Value> = list.values.iter().map(proto_value_to_json).collect();
            serde_json::Value::Array(arr)
        }
        Some(Kind::StructValue(s)) => {
            let mut map = serde_json::Map::new();
            for (key, val) in &s.fields {
                map.insert(key.clone(), proto_value_to_json(val));
            }
            serde_json::Value::Object(map)
        }
        None => serde_json::Value::Null,
    }
}
