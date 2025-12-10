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

//! gRPC Source Plugin for Drasi
//!
//! This plugin exposes a gRPC endpoint for receiving data change events. External systems
//! can stream events to Drasi using the gRPC protocol, which provides efficient binary
//! serialization and bidirectional streaming support.
//!
//! # Service Endpoints
//!
//! The gRPC source implements the following service methods:
//!
//! - **`submit_event`** - Submit a single event (unary RPC)
//! - **`stream_events`** - Stream multiple events (client streaming RPC)
//! - **`request_bootstrap`** - Request initial data for bootstrapping (server streaming RPC)
//! - **`health_check`** - Check service health (unary RPC)
//!
//! # Protocol Buffer Format
//!
//! Events are submitted using the `SourceChange` protobuf message. See the
//! `proto/drasi/v1/source.proto` file for the full schema.
//!
//! ## Insert/Update
//!
//! ```protobuf
//! SourceChange {
//!     type: INSERT or UPDATE
//!     change: Element {
//!         node: Node {
//!             metadata: ElementMetadata { ... }
//!             properties: Struct { ... }
//!         }
//!     }
//! }
//! ```
//!
//! ## Delete
//!
//! ```protobuf
//! SourceChange {
//!     type: DELETE
//!     change: Metadata {
//!         reference: ElementReference { ... }
//!         labels: ["Label1", "Label2"]
//!     }
//! }
//! ```
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `host` | string | `"0.0.0.0"` | Host address to bind to |
//! | `port` | u16 | `50051` | Port to listen on |
//! | `endpoint` | string | None | Optional custom endpoint path |
//! | `timeout_ms` | u64 | `5000` | Request timeout in milliseconds |
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: grpc
//! properties:
//!   host: "0.0.0.0"
//!   port: 50051
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_source_grpc::{GrpcSource, GrpcSourceConfig};
//! use std::sync::Arc;
//!
//! let config = GrpcSourceConfig {
//!     host: "0.0.0.0".to_string(),
//!     port: 50051,
//!     endpoint: None,
//!     timeout_ms: 5000,
//! };
//!
//! let source = Arc::new(GrpcSource::new("my-grpc-source", config)?);
//! drasi.add_source(source).await?;
//! ```
//!
//! # Client Example (using grpcurl)
//!
//! ```bash
//! grpcurl -plaintext -d '{"event": {...}}' localhost:50051 drasi.v1.SourceService/SubmitEvent
//! ```

pub mod config;

#[cfg(test)]
mod tests;
pub use config::GrpcSourceConfig;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};

use drasi_lib::channels::{DispatchMode, *};
use drasi_lib::managers::{log_component_start, log_component_stop};
use drasi_lib::plugin_core::Source;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};

// Include generated protobuf code
// Allow unwrap in generated proto code - tonic generates code with unwrap() for HTTP response building
#[allow(clippy::unwrap_used)]
pub mod proto {
    tonic::include_proto!("drasi.v1");
}

use proto::{
    source_service_server::{SourceService, SourceServiceServer},
    BootstrapRequest as ProtoBootstrapRequest, BootstrapResponse, HealthCheckResponse,
    SourceChange as ProtoSourceChange, StreamEventResponse, SubmitEventRequest,
    SubmitEventResponse,
};

/// gRPC source that exposes a gRPC endpoint to receive SourceChangeEvents.
///
/// This source implements a gRPC service for receiving data change events from external
/// systems. It supports both unary and streaming RPC methods for event submission.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: gRPC-specific configuration (host, port, timeout)
pub struct GrpcSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// gRPC source configuration
    config: GrpcSourceConfig,
}

impl GrpcSource {
    /// Create a builder for GrpcSource
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_grpc::GrpcSource;
    ///
    /// let source = GrpcSource::builder("my-grpc-source")
    ///     .with_host("0.0.0.0")
    ///     .with_port(50051)
    ///     .with_bootstrap_provider(my_provider)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> GrpcSourceBuilder {
        GrpcSourceBuilder::new(id)
    }

    /// Create a new gRPC source.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - gRPC source configuration
    ///
    /// # Returns
    ///
    /// A new `GrpcSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_grpc::{GrpcSource, GrpcSourceConfig};
    ///
    /// let config = GrpcSourceConfig {
    ///     host: "0.0.0.0".to_string(),
    ///     port: 50051,
    ///     endpoint: None,
    ///     timeout_ms: 5000,
    /// };
    ///
    /// let source = GrpcSource::new("my-grpc-source", config)?;
    /// ```
    pub fn new(id: impl Into<String>, config: GrpcSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    /// Create a new gRPC source with custom dispatch settings
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    pub fn with_dispatch(
        id: impl Into<String>,
        config: GrpcSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let id = id.into();
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

#[async_trait]
impl Source for GrpcSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "grpc"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "host".to_string(),
            serde_json::Value::String(self.config.host.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        if let Some(ref endpoint) = self.config.endpoint {
            props.insert(
                "endpoint".to_string(),
                serde_json::Value::String(endpoint.clone()),
            );
        }
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        log_component_start("gRPC Source", &self.base.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting gRPC source".to_string()),
            )
            .await?;

        // Get configuration
        let host = self.config.host.clone();
        let port = self.config.port;

        let addr = format!("{}:{}", host, port).parse()?;

        info!("gRPC source '{}' listening on {}", self.base.id, addr);

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        // Create gRPC service
        let service = GrpcSourceService {
            source_id: self.base.id.clone(),
            dispatchers: self.base.dispatchers.clone(),
        };

        let svc = SourceServiceServer::new(service);

        // Start the gRPC server
        let status = Arc::clone(&self.base.status);
        let source_id = self.base.id.clone();
        let event_tx = self.base.event_tx();

        let task = tokio::spawn(async move {
            *status.write().await = ComponentStatus::Running;

            let running_event = ComponentEvent {
                component_id: source_id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some(format!("gRPC source listening on {}", addr)),
            };

            if let Some(ref tx) = *event_tx.read().await {
                if let Err(e) = tx.send(running_event).await {
                    error!("Failed to send component event: {}", e);
                }
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
        log_component_stop("gRPC Source", &self.base.id);
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// gRPC service implementation for the SourceService RPC methods.
///
/// Handles incoming gRPC requests and dispatches events to registered subscribers.
struct GrpcSourceService {
    /// The source ID used for event attribution
    source_id: String,
    /// Shared dispatchers for sending events to subscribers
    dispatchers: Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
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
                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                    let wrapper = SourceEventWrapper::with_profiling(
                        self.source_id.clone(),
                        SourceEvent::Change(source_change),
                        chrono::Utc::now(),
                        profiling,
                    );

                    debug!("[{}] Processing gRPC event: {:?}", self.source_id, &wrapper);

                    // Dispatch via helper
                    if let Err(e) = SourceBase::dispatch_from_task(
                        self.dispatchers.clone(),
                        wrapper,
                        &self.source_id,
                    )
                    .await
                    {
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
                        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

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
                            debug!("[{}] Failed to dispatch (no subscribers): {}", source_id, e);
                        }

                        events_processed += 1;

                        // Send periodic updates
                        if events_processed.is_multiple_of(100) {
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

/// Convert protobuf SourceChange to Drasi Core SourceChange.
///
/// # Arguments
///
/// * `proto_change` - The protobuf source change message
/// * `source_id` - Source ID to use in element references
///
/// # Returns
///
/// A Drasi Core `SourceChange` or an error if conversion fails.
///
/// # Errors
///
/// Returns an error if:
/// - The change type is invalid
/// - Required fields are missing
/// - Element data cannot be converted
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

/// Convert protobuf Element to Drasi Core Element.
///
/// Handles both Node and Relation element types.
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
                .ok_or_else(|| anyhow::anyhow!(
                    "Validation error: Node element missing required 'metadata' field in gRPC message. \
                     Ensure the gRPC client sends complete node data."
                ))?;

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
                .ok_or_else(|| anyhow::anyhow!(
                    "Validation error: Relation element missing required 'metadata' field in gRPC message. \
                     Ensure the gRPC client sends complete relation data."
                ))?;

            let metadata = convert_proto_metadata_to_core(metadata, source_id)?;
            let properties = convert_proto_properties(&relation.properties)?;

            let in_node = relation.in_node.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Validation error: Relation missing required 'in_node' field. \
                     Relations must specify both source and target nodes."
                )
            })?;
            let out_node = relation.out_node.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Validation error: Relation missing required 'out_node' field. \
                     Relations must specify both source and target nodes."
                )
            })?;

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

/// Builder for gRPC sources.
///
/// Provides a fluent API for constructing gRPC sources
/// with sensible defaults.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_grpc::GrpcSource;
///
/// let source = GrpcSource::builder("my-grpc-source")
///     .with_host("0.0.0.0")
///     .with_port(50051)
///     .build()?;
/// ```
pub struct GrpcSourceBuilder {
    id: String,
    host: String,
    port: u16,
    endpoint: Option<String>,
    timeout_ms: u64,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl GrpcSourceBuilder {
    /// Create a new gRPC source builder with the given ID and default values
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: "0.0.0.0".to_string(),
            port: 50051,
            endpoint: None,
            timeout_ms: 5000,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the gRPC host
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the gRPC port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the optional service endpoint
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set the dispatch mode for this source
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity for this source
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for this source
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start when DrasiLib starts.
    ///
    /// Default is `true`. Set to `false` if this source should only be
    /// started manually via `start_source()`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: GrpcSourceConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.endpoint = config.endpoint;
        self.timeout_ms = config.timeout_ms;
        self
    }

    /// Build the gRPC source
    ///
    /// # Errors
    ///
    /// Returns an error if the source cannot be constructed.
    pub fn build(self) -> Result<GrpcSource> {
        let config = GrpcSourceConfig {
            host: self.host,
            port: self.port,
            endpoint: self.endpoint,
            timeout_ms: self.timeout_ms,
        };

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        Ok(GrpcSource {
            base: SourceBase::new(params)?,
            config,
        })
    }
}
