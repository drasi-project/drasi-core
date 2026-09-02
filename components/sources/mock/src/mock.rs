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

use super::config::{DataType, MockSourceConfig};
use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::schema::{NodeSchema, PropertySchema, PropertyType, RelationSchema, SourceSchema};
use log::{debug, info};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::*;
use drasi_lib::identity::{CredentialContext, IdentityProvider};
use drasi_lib::managers::{log_component_start, log_component_stop};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use tracing::Instrument;

/// Mock source that generates synthetic data for testing and development.
///
/// This source runs an internal tokio task that generates data at configurable
/// intervals. It supports different data types (Counter, SensorReading, Generic)
/// to simulate various real-world scenarios.
///
/// # Event Generation Behavior
///
/// - **Counter**: Always emits INSERT events with sequential IDs
/// - **SensorReading**: Emits INSERT for first reading per sensor, UPDATE thereafter
/// - **Generic**: Always emits INSERT events with sequential IDs
///
/// # Thread Safety
///
/// This type is `Send + Sync` and can be safely shared across threads.
/// Internal state is protected by `RwLock`.
pub struct MockSource {
    /// Base source implementation providing dispatchers, status tracking, and lifecycle management.
    pub(crate) base: SourceBase,

    /// Configuration specifying data type and generation interval.
    config: MockSourceConfig,

    /// Tracks sensor IDs that have been seen for INSERT vs UPDATE logic.
    /// Only used when `config.data_type` is `SensorReading`.
    seen_sensors: Arc<RwLock<HashSet<u32>>>,

    /// Live mesh of CONNECTED_TO edges when SensorReading.mesh is enabled.
    mesh_state: Arc<RwLock<MeshState>>,
}

impl MockSource {
    /// Creates a new `MockSource` with the given ID and configuration.
    ///
    /// The source is created in a stopped state. Call [`start()`](Self::start) to begin
    /// generating events, or add it to DrasiLib which will start it automatically
    /// (unless `auto_start` is disabled via the builder).
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance. Must be unique within a DrasiLib instance.
    /// * `config` - Configuration specifying data type and generation interval.
    ///
    /// # Returns
    ///
    /// A new `MockSource` instance, or an error if validation fails.
    ///
    /// # Errors
    ///
    /// Returns [`anyhow::Error`] if:
    /// - `config.interval_ms` is 0 (would cause spin loop)
    /// - `config.data_type` is `SensorReading` with `sensor_count` of 0
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_mock::{MockSource, MockSourceConfig, DataType};
    ///
    /// let config = MockSourceConfig {
    ///     data_type: DataType::sensor_reading(10),
    ///     interval_ms: 1000,
    /// };
    ///
    /// let source = MockSource::new("my-mock-source", config)?;
    /// ```
    pub fn new(id: impl Into<String>, config: MockSourceConfig) -> Result<Self> {
        config.validate()?;
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
            mesh_state: Arc::new(RwLock::new(MeshState::default())),
        })
    }

    /// Creates a new `MockSource` with custom dispatch settings.
    ///
    /// This is a lower-level constructor for advanced use cases where you need
    /// control over event dispatching. For most cases, prefer [`MockSource::builder()`].
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance.
    /// * `config` - Configuration specifying data type and generation interval.
    /// * `dispatch_mode` - Optional dispatch mode (`Channel` or `Broadcast`).
    /// * `dispatch_buffer_capacity` - Optional buffer size for dispatch channels.
    ///
    /// # Errors
    ///
    /// Returns [`anyhow::Error`] if:
    /// - `config.interval_ms` is 0
    /// - `config.data_type` is `SensorReading` with `sensor_count` of 0
    pub fn with_dispatch(
        id: impl Into<String>,
        config: MockSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        config.validate()?;
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
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
            mesh_state: Arc::new(RwLock::new(MeshState::default())),
        })
    }
}

#[async_trait]
impl Source for MockSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mock"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        // Serialize through the DTO to get camelCase naming and structured output
        // matching the creation schema and config file format
        use crate::descriptor::{DataTypeDto, MockSourceConfigDto};
        use drasi_plugin_sdk::ConfigValue;

        let data_type_dto = match &self.config.data_type {
            DataType::Counter => DataTypeDto::Counter,
            DataType::SensorReading { sensor_count, mesh } => DataTypeDto::SensorReading {
                sensor_count: *sensor_count,
                mesh: *mesh,
            },
            DataType::Generic => DataTypeDto::Generic,
        };

        let dto = MockSourceConfigDto {
            data_type: data_type_dto,
            interval_ms: ConfigValue::Static(self.config.interval_ms),
        };

        self.base.properties_or_serialize(&dto)
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        let (label, properties) = match &self.config.data_type {
            DataType::Counter => (
                "Counter",
                vec![
                    PropertySchema {
                        name: "value".to_string(),
                        data_type: Some(PropertyType::Integer),
                        description: None,
                    },
                    PropertySchema {
                        name: "timestamp".to_string(),
                        data_type: Some(PropertyType::Timestamp),
                        description: None,
                    },
                ],
            ),
            DataType::SensorReading { .. } => (
                "SensorReading",
                vec![
                    PropertySchema {
                        name: "sensor_id".to_string(),
                        data_type: Some(PropertyType::String),
                        description: None,
                    },
                    PropertySchema {
                        name: "temperature".to_string(),
                        data_type: Some(PropertyType::Float),
                        description: None,
                    },
                    PropertySchema {
                        name: "humidity".to_string(),
                        data_type: Some(PropertyType::Float),
                        description: None,
                    },
                    PropertySchema {
                        name: "timestamp".to_string(),
                        data_type: Some(PropertyType::Timestamp),
                        description: None,
                    },
                ],
            ),
            DataType::Generic => (
                "Generic",
                vec![
                    PropertySchema {
                        name: "value".to_string(),
                        data_type: Some(PropertyType::Integer),
                        description: None,
                    },
                    PropertySchema {
                        name: "message".to_string(),
                        data_type: Some(PropertyType::String),
                        description: None,
                    },
                    PropertySchema {
                        name: "timestamp".to_string(),
                        data_type: Some(PropertyType::Timestamp),
                        description: None,
                    },
                ],
            ),
        };

        let relations = match &self.config.data_type {
            DataType::SensorReading { mesh: true, .. } => vec![RelationSchema {
                label: "CONNECTED_TO".to_string(),
                from: Some("SensorReading".to_string()),
                to: Some("SensorReading".to_string()),
                properties: vec![PropertySchema {
                    name: "strength".to_string(),
                    data_type: Some(PropertyType::Float),
                    description: Some("Link strength in the sensor mesh".to_string()),
                }],
            }],
            _ => Vec::new(),
        };

        Some(SourceSchema {
            nodes: vec![NodeSchema {
                label: label.to_string(),
                properties,
            }],
            relations,
        })
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Mock Source", &self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting mock source".to_string()),
            )
            .await;

        // Get broadcast_tx for publishing
        let base = self.base.clone_shared();
        let source_id = self.base.id.clone();

        // Get configuration
        let data_type = self.config.data_type.clone();
        let interval_ms = self.config.interval_ms;

        // Clone seen_sensors / mesh state for the task
        let seen_sensors = Arc::clone(&self.seen_sensors);
        let mesh_state = Arc::clone(&self.mesh_state);

        // Get instance_id from context for log routing isolation
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        // Start the data generation task with component span for proper log routing
        let status_handle = self.base.status_handle();
        let source_name = self.base.id.clone();
        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "mock_source_task",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );
        let task = tokio::spawn(
            async move {
                // Set Running status inside the task to avoid a race condition where
                // the loop checks status before the caller sets it after spawn.
                status_handle
                    .set_status(
                        ComponentStatus::Running,
                        Some("Mock source started successfully".to_string()),
                    )
                    .await;

                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
                let mut seq = 0u64;

                loop {
                    interval.tick().await;

                    // Check if we should stop
                    if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
                        break;
                    }

                    seq += 1;

                    // Generate data based on type
                    let source_change = match &data_type {
                        DataType::Counter => {
                            let element_id = format!("counter_{seq}");
                            let reference = ElementReference::new(&source_name, &element_id);

                            let mut property_map = ElementPropertyMap::new();
                            property_map.insert(
                                "value",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::Number(seq.into()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "timestamp",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::String(chrono::Utc::now().to_rfc3339()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );

                            let metadata = ElementMetadata {
                                reference,
                                labels: Arc::from(vec![Arc::from("Counter")]),
                                effective_from: crate::time::get_system_time_millis()
                                    .unwrap_or_else(|e| {
                                        log::warn!("Failed to get timestamp for mock counter: {e}");
                                        chrono::Utc::now().timestamp_millis() as u64
                                    }),
                            };

                            let element = Element::Node {
                                metadata,
                                properties: property_map,
                            };

                            SourceChange::Insert { element }
                        }
                        DataType::SensorReading { sensor_count, mesh } => {
                            // When mesh is on, insert sensors 0..n-1 first so the graph
                            // can appear as soon as every node exists.
                            let sensor_id = if *mesh {
                                let seen = seen_sensors.read().await;
                                (0..*sensor_count)
                                    .find(|id| !seen.contains(id))
                                    .unwrap_or_else(|| rand::random::<u32>() % *sensor_count)
                            } else {
                                rand::random::<u32>() % *sensor_count
                            };
                            // Use sensor_id as the element_id for stable identity
                            let element_id = format!("sensor_{sensor_id}");
                            let reference = ElementReference::new(&source_name, &element_id);

                            let mut property_map = ElementPropertyMap::new();
                            property_map.insert(
                                "sensor_id",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::String(format!("sensor_{sensor_id}")),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "temperature",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::Number(
                                        serde_json::Number::from_f64(
                                            20.0 + rand::random::<f64>() * 10.0,
                                        )
                                        .unwrap_or(serde_json::Number::from(25)),
                                    ),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "humidity",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::Number(
                                        serde_json::Number::from_f64(
                                            40.0 + rand::random::<f64>() * 20.0,
                                        )
                                        .unwrap_or(serde_json::Number::from(50)),
                                    ),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "timestamp",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::String(chrono::Utc::now().to_rfc3339()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );

                            let metadata = ElementMetadata {
                                reference,
                                labels: Arc::from(vec![Arc::from("SensorReading")]),
                                effective_from: crate::time::get_system_time_millis()
                                    .unwrap_or_else(|e| {
                                        log::warn!("Failed to get timestamp for mock sensor: {e}");
                                        chrono::Utc::now().timestamp_millis() as u64
                                    }),
                            };

                            let element = Element::Node {
                                metadata,
                                properties: property_map,
                            };

                            // Determine if this is a new sensor (Insert) or an update (Update)
                            let is_new = {
                                let mut seen = seen_sensors.write().await;
                                seen.insert(sensor_id)
                            };

                            if is_new {
                                SourceChange::Insert { element }
                            } else {
                                SourceChange::Update { element }
                            }
                        }
                        DataType::Generic => {
                            // Generic data
                            let element_id = format!("generic_{seq}");
                            let reference = ElementReference::new(&source_name, &element_id);

                            let mut property_map = ElementPropertyMap::new();
                            property_map.insert(
                                "value",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::Number(rand::random::<i32>().into()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "message",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::String("Generic mock data".to_string()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );
                            property_map.insert(
                                "timestamp",
                                crate::conversion::json_to_element_value_or_default(
                                    &Value::String(chrono::Utc::now().to_rfc3339()),
                                    drasi_core::models::ElementValue::Null,
                                ),
                            );

                            let metadata = ElementMetadata {
                                reference,
                                labels: Arc::from(vec![Arc::from("Generic")]),
                                effective_from: crate::time::get_system_time_millis()
                                    .unwrap_or_else(|e| {
                                        log::warn!("Failed to get timestamp for mock generic: {e}");
                                        chrono::Utc::now().timestamp_millis() as u64
                                    }),
                            };

                            let element = Element::Node {
                                metadata,
                                properties: property_map,
                            };

                            SourceChange::Insert { element }
                        }
                    };

                    // Create profiling metadata with timestamps
                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                    let wrapper = SourceEventDraft::with_profiling(
                        source_id.clone(),
                        SourceEvent::Change(source_change),
                        chrono::Utc::now(),
                        profiling,
                    );

                    // Dispatch to all subscribers via helper
                    if let Err(e) = base.dispatch_event(wrapper).await {
                        debug!("Failed to dispatch change: {e}");
                    }

                    if let DataType::SensorReading {
                        sensor_count,
                        mesh: true,
                    } = &data_type
                    {
                        emit_mesh_tick(
                            Arc::clone(&mesh_state),
                            Arc::clone(&seen_sensors),
                            *sensor_count,
                            &source_name,
                            &source_id,
                            seq,
                            base.clone_shared(),
                        )
                        .await;
                    }
                }

                info!("Mock source task completed");
            }
            .instrument(span),
        );

        *self.base.task_handle.write().await = Some(task);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Mock Source", &self.base.id);

        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping mock source".to_string()),
            )
            .await;

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Mock source stopped successfully".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "Mock").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        // Test-only: exercise the FFI identity-provider clone/drop path across the plugin
        // boundary. Gated entirely behind the `DRASI_MOCK_IDENTITY_CLONE_STRESS` environment
        // variable (read only here) so no test-only knob leaks into the public config schema.
        // When set to N > 0 and an identity provider was injected, clone it N times and fetch
        // credentials on each clone. Each `clone_box` invokes the FFI vtable `clone_fn` (the
        // path that previously leaked a vtable struct), and each `get_credentials` round-trips
        // through the host back into the identity-provider plugin.
        if let Some(provider) = context.identity_provider.clone() {
            let clone_stress = std::env::var("DRASI_MOCK_IDENTITY_CLONE_STRESS")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            if clone_stress > 0 {
                let ctx = CredentialContext::default();
                for _ in 0..clone_stress {
                    let cloned: Box<dyn IdentityProvider> = provider.clone_box();
                    let _ = cloned.get_credentials(&ctx).await;
                }
            }
        }
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

impl MockSource {
    /// Injects a custom event into the source for testing purposes.
    ///
    /// This allows tests to send specific [`SourceChange`] events (INSERT, UPDATE, DELETE)
    /// without waiting for automatic generation. Useful for deterministic testing scenarios.
    ///
    /// The source does not need to be started to inject events.
    ///
    /// # Arguments
    ///
    /// * `change` - The [`SourceChange`] to inject (e.g., `SourceChange::Insert { element }`)
    ///
    /// # Errors
    ///
    /// Returns [`anyhow::Error`] if dispatching fails (e.g., all receivers have been dropped).
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        self.base.dispatch_source_change(change).await
    }

    /// Creates a test subscription to receive events from this source.
    ///
    /// This bypasses DrasiLib's subscription mechanism and directly subscribes to
    /// the source's event dispatcher. Useful for unit testing the source in isolation.
    ///
    /// # Returns
    ///
    /// A boxed receiver that yields [`SourceEventDraft`](drasi_lib::channels::SourceEventDraft)
    /// for each event generated or injected.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = MockSource::new("test", config)?;
    /// let mut rx = source.test_subscribe().await;
    ///
    /// source.start().await?;
    ///
    /// // Receive events
    /// while let Some(event) = rx.recv().await {
    ///     println!("Received: {:?}", event);
    /// }
    /// ```
    pub async fn test_subscribe(
        &self,
    ) -> Box<dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::StampedSourceEvent>> {
        self.base.test_subscribe().await
    }
}

/// Builder for [`MockSource`] instances.
///
/// Provides a fluent API for constructing mock sources with sensible defaults.
/// This is the recommended way to create a `MockSource`.
///
/// # Defaults
///
/// | Option | Default |
/// |--------|---------|
/// | `data_type` | [`DataType::Generic`] |
/// | `interval_ms` | 5000 |
/// | `dispatch_mode` | `Channel` |
/// | `dispatch_buffer_capacity` | 1000 |
/// | `auto_start` | `true` |
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_mock::{MockSource, DataType};
///
/// let source = MockSource::builder("my-source")
///     .with_data_type(DataType::sensor_reading(10))
///     .with_interval_ms(1000)
///     .with_auto_start(false)  // Don't start automatically
///     .build()?;
/// ```
pub struct MockSourceBuilder {
    id: String,
    data_type: DataType,
    interval_ms: u64,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MockSourceBuilder {
    /// Create a new builder with the given source ID.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            data_type: DataType::Generic,
            interval_ms: 5000,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the data type to generate.
    ///
    /// # Arguments
    ///
    /// * `data_type` - One of: `DataType::Counter`, `DataType::SensorReading { sensor_count }`, or `DataType::Generic` (default)
    ///
    /// For SensorReading, use `DataType::sensor_reading(count)` helper method.
    pub fn with_data_type(mut self, data_type: DataType) -> Self {
        self.data_type = data_type;
        self
    }

    /// Set the generation interval in milliseconds.
    ///
    /// # Arguments
    ///
    /// * `interval_ms` - Interval between data generation (default: 5000)
    pub fn with_interval_ms(mut self, interval_ms: u64) -> Self {
        self.interval_ms = interval_ms;
        self
    }

    /// Set the dispatch mode for event routing.
    ///
    /// # Arguments
    ///
    /// * `mode` - `Channel` (default, with backpressure) or `Broadcast`
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Buffer size for dispatch channels (default: 1000)
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for initial data delivery.
    ///
    /// # Arguments
    ///
    /// * `provider` - Bootstrap provider implementation
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

    /// Build the MockSource instance.
    ///
    /// # Returns
    ///
    /// A fully constructed `MockSource`, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The base source cannot be initialized
    /// - The configuration is invalid (e.g., interval_ms is 0, sensor_count is 0)
    pub fn build(self) -> Result<MockSource> {
        let config = MockSourceConfig {
            data_type: self.data_type,
            interval_ms: self.interval_ms,
        };

        config.validate()?;

        // Build SourceBaseParams with all settings
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

        Ok(MockSource {
            base: SourceBase::new(params)?,
            config,
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
            mesh_state: Arc::new(RwLock::new(MeshState::default())),
        })
    }
}

impl MockSource {
    /// Create a builder for MockSource with the given ID.
    ///
    /// This is the recommended way to construct a MockSource.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = MockSource::builder("my-source")
    ///     .with_data_type(DataType::sensor_reading(10))
    ///     .with_interval_ms(1000)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> MockSourceBuilder {
        MockSourceBuilder::new(id)
    }
}

#[derive(Debug, Clone)]
struct MeshEdge {
    from: u32,
    to: u32,
    id: String,
    is_chord: bool,
}

#[derive(Debug, Default)]
struct MeshState {
    seeded: bool,
    edges: Vec<MeshEdge>,
    next_chord: u32,
}

fn random_strength() -> f64 {
    0.3 + rand::random::<f64>() * 0.7
}

fn mesh_timestamp_millis() -> u64 {
    crate::time::get_system_time_millis().unwrap_or_else(|e| {
        log::warn!("Failed to get timestamp for mesh edge: {e}");
        chrono::Utc::now().timestamp_millis() as u64
    })
}

fn pick_replacement_chord(
    existing: &HashSet<(u32, u32)>,
    sensor_count: u32,
    next_chord: u32,
) -> Option<MeshEdge> {
    if sensor_count < 2 {
        return None;
    }
    for _ in 0..16 {
        let from = rand::random::<u32>() % sensor_count;
        let to = rand::random::<u32>() % sensor_count;
        if from == to || existing.contains(&(from, to)) {
            continue;
        }
        return Some(make_chord(from, to, next_chord));
    }
    for from in 0..sensor_count {
        for to in 0..sensor_count {
            if from != to && !existing.contains(&(from, to)) {
                return Some(make_chord(from, to, next_chord));
            }
        }
    }
    None
}

fn make_chord(from: u32, to: u32, next_chord: u32) -> MeshEdge {
    MeshEdge {
        from,
        to,
        id: format!("mesh_chord_{next_chord}"),
        is_chord: true,
    }
}

fn initial_mesh_edges(sensor_count: u32) -> Vec<MeshEdge> {
    let mut edges = Vec::new();
    if sensor_count < 2 {
        return edges;
    }

    for i in 0..sensor_count {
        let to = (i + 1) % sensor_count;
        edges.push(MeshEdge {
            from: i,
            to,
            id: format!("mesh_ring_{i}"),
            is_chord: false,
        });
    }

    let chord_count = ((sensor_count / 3).max(1)).min(sensor_count.saturating_sub(1));
    for k in 0..chord_count {
        let from = (k * 2) % sensor_count;
        let span = (sensor_count / 2).max(2);
        let to = (from + span) % sensor_count;
        if from == to {
            continue;
        }
        if to == (from + 1) % sensor_count || from == (to + 1) % sensor_count {
            continue;
        }
        edges.push(MeshEdge {
            from,
            to,
            id: format!("mesh_chord_{k}"),
            is_chord: true,
        });
    }
    edges
}

fn connected_to_element(source_name: &str, edge: &MeshEdge, strength: f64) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert(
        "strength",
        crate::conversion::json_to_element_value_or_default(
            &Value::Number(
                serde_json::Number::from_f64(strength).unwrap_or(serde_json::Number::from(1)),
            ),
            drasi_core::models::ElementValue::Null,
        ),
    );

    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_name, &edge.id),
            labels: Arc::from(vec![Arc::from("CONNECTED_TO")]),
            effective_from: mesh_timestamp_millis(),
        },
        properties,
        in_node: ElementReference::new(source_name, &format!("sensor_{}", edge.from)),
        out_node: ElementReference::new(source_name, &format!("sensor_{}", edge.to)),
    }
}

async fn dispatch_generated_change(base: SourceBase, source_id: &str, source_change: SourceChange) {
    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());
    let wrapper = SourceEventDraft::with_profiling(
        source_id.to_string(),
        SourceEvent::Change(source_change),
        chrono::Utc::now(),
        profiling,
    );
    if let Err(e) = base.dispatch_event(wrapper).await {
        debug!("Failed to dispatch mesh change: {e}");
    }
}

async fn emit_mesh_tick(
    mesh_state: Arc<RwLock<MeshState>>,
    seen_sensors: Arc<RwLock<HashSet<u32>>>,
    sensor_count: u32,
    source_name: &str,
    source_id: &str,
    seq: u64,
    base: SourceBase,
) {
    let seen_count = seen_sensors.read().await.len() as u32;
    if seen_count < sensor_count || sensor_count < 2 {
        return;
    }

    let mut state = mesh_state.write().await;
    if !state.seeded {
        state.edges = initial_mesh_edges(sensor_count);
        state.next_chord = state.edges.iter().filter(|e| e.is_chord).count() as u32;
        let edges = state.edges.clone();
        state.seeded = true;
        drop(state);
        for edge in edges {
            dispatch_generated_change(
                base.clone_shared(),
                source_id,
                SourceChange::Insert {
                    element: connected_to_element(source_name, &edge, random_strength()),
                },
            )
            .await;
        }
        return;
    }

    if state.edges.is_empty() {
        return;
    }

    if seq.is_multiple_of(7) {
        if let Some(idx) = state
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_chord)
            .map(|(i, _)| i)
            .next()
        {
            let existing: HashSet<(u32, u32)> =
                state.edges.iter().map(|e| (e.from, e.to)).collect();
            if let Some(edge) = pick_replacement_chord(&existing, sensor_count, state.next_chord) {
                let old = state.edges.remove(idx);
                state.next_chord += 1;
                drop(state);

                dispatch_generated_change(
                    base.clone_shared(),
                    source_id,
                    SourceChange::Delete {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(source_name, &old.id),
                            labels: Arc::from(vec![Arc::from("CONNECTED_TO")]),
                            effective_from: mesh_timestamp_millis(),
                        },
                    },
                )
                .await;

                {
                    let mut state = mesh_state.write().await;
                    state.edges.push(edge.clone());
                }
                dispatch_generated_change(
                    base.clone_shared(),
                    source_id,
                    SourceChange::Insert {
                        element: connected_to_element(source_name, &edge, random_strength()),
                    },
                )
                .await;
            }
            return;
        }
    }

    if seq.is_multiple_of(3) {
        let idx = (seq as usize) % state.edges.len();
        let edge = state.edges[idx].clone();
        drop(state);
        dispatch_generated_change(
            base.clone_shared(),
            source_id,
            SourceChange::Update {
                element: connected_to_element(source_name, &edge, random_strength()),
            },
        )
        .await;
    }
}

#[cfg(test)]
mod mesh_unit_tests {
    use super::*;

    #[test]
    fn initial_mesh_edges_empty_for_tiny_counts() {
        assert!(initial_mesh_edges(0).is_empty());
        assert!(initial_mesh_edges(1).is_empty());
    }

    #[test]
    fn initial_mesh_edges_is_a_ring_for_two_sensors() {
        let edges = initial_mesh_edges(2);
        let ring: Vec<_> = edges.iter().filter(|e| !e.is_chord).collect();
        assert_eq!(ring.len(), 2);
        assert_eq!((ring[0].from, ring[0].to), (0, 1));
        assert_eq!((ring[1].from, ring[1].to), (1, 0));
    }

    #[test]
    fn initial_mesh_edges_has_unique_pairs_and_chords() {
        let edges = initial_mesh_edges(10);
        assert_eq!(edges.iter().filter(|e| !e.is_chord).count(), 10);
        assert!(edges.iter().any(|e| e.is_chord));
        let pairs: HashSet<_> = edges.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(pairs.len(), edges.len());
    }

    #[test]
    fn pick_replacement_chord_none_when_complete() {
        let existing: HashSet<(u32, u32)> = [(0, 1), (1, 0)].into_iter().collect();
        assert!(pick_replacement_chord(&existing, 2, 0).is_none());
    }

    #[test]
    fn pick_replacement_chord_finds_missing_pair() {
        let existing: HashSet<(u32, u32)> = [(0, 1)].into_iter().collect();
        let edge = pick_replacement_chord(&existing, 2, 5).expect("should find (1, 0)");
        assert_eq!((edge.from, edge.to), (1, 0));
        assert_eq!(edge.id, "mesh_chord_5");
        assert!(edge.is_chord);
    }
}
