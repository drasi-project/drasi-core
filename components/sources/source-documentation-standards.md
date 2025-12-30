# Source Plugin Documentation Standards

This document defines the documentation standards for all source plugin implementations in drasi-core.

## Reference Implementations

The following plugins serve as reference implementations for different aspects:

- **`plugin-platform`**: Reference for module-level documentation, builder pattern, and test organization
- **`plugin-http`**: Reference for comprehensive data format examples and adaptive batching
- **`plugin-grpc`**: Reference for protocol buffer integration and proto conversion

When creating a new source plugin, use these as templates for documentation style and code organization.

## Required Documentation Sections

### 1. Module-Level Documentation (`lib.rs`)

Each source plugin's `lib.rs` should include a module-level doc comment at the top explaining:

```rust
//! Brief one-line description of the source.
//!
//! Detailed description of what this source does, its purpose,
//! and primary use cases.
//!
//! # Configuration
//!
//! The source is configured via [`SourceConfig`]. Key configuration options:
//!
//! - `field_name`: Description of the field and valid values
//! - `optional_field`: (Optional) Description with default value
//!
//! # Data Format
//!
//! Description of the expected inbound data format. Include JSON examples
//! where applicable:
//!
//! ```json
//! {
//!     "op": "i",
//!     "payload": { ... }
//! }
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! let config = SourceConfig { ... };
//! let source = Source::new("my-source", config)?;
//! source.start().await?;
//! ```
```

### 2. Struct Documentation

All public structs must have doc comments:

```rust
/// Brief description of the struct.
///
/// Longer description explaining the purpose, ownership model,
/// and key behaviors.
pub struct MySource {
    /// Description of the base field
    base: SourceBase,
    /// Description of the config field
    config: MySourceConfig,
}
```

### 3. Configuration Struct Documentation

Configuration structs must document all fields:

```rust
/// Configuration for the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySourceConfig {
    /// Description of required field. Example: "localhost"
    pub host: String,

    /// Description of port. Default: 8080
    #[serde(default = "default_port")]
    pub port: u16,

    /// (Optional) Description of optional field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional_field: Option<String>,
}
```

### 4. Method Documentation

All public methods must have doc comments:

```rust
/// Brief description of what the method does.
///
/// Longer description if needed, explaining:
/// - Parameters and their purpose
/// - Return value semantics
/// - Any side effects
/// - Error conditions
///
/// # Arguments
///
/// * `id` - The unique identifier for this source instance
/// * `config` - Configuration for the source
///
/// # Returns
///
/// A new source instance ready to be started.
///
/// # Errors
///
/// Returns an error if configuration validation fails.
pub fn new(id: impl Into<String>, config: MySourceConfig) -> Result<Self> {
    // ...
}
```

### 5. Inline Comments for Complex Logic

Add inline comments for non-obvious code:

```rust
// Extract element type from payload.source.table
let element_type = payload["source"]["table"]
    .as_str()
    .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'payload.source.table' field"))?;

// Handle delete operation (no properties needed)
if op == "d" {
    // Deletes only need metadata, not full element data
    source_changes.push(SourceChange::Delete { metadata });
    continue;
}
```

## Data Format Documentation

Each source should document:

1. **Input Format**: What data format the source expects (JSON, protobuf, SQL, etc.)
2. **Field Mapping**: How input fields map to SourceChange operations
3. **Examples**: JSON/YAML examples for each operation type

### Example Data Format Documentation

```rust
//! # Data Format
//!
//! The HTTP source accepts JSON events in the following format:
//!
//! ## Insert Operation
//!
//! ```json
//! {
//!     "type": "insert",
//!     "element": {
//!         "type": "node",
//!         "id": "node-123",
//!         "labels": ["Person"],
//!         "properties": { "name": "Alice" }
//!     },
//!     "timestamp": 1699900000000000000
//! }
//! ```
//!
//! ## Update Operation
//!
//! ```json
//! {
//!     "type": "update",
//!     "element": { ... }
//! }
//! ```
//!
//! ## Delete Operation
//!
//! ```json
//! {
//!     "type": "delete",
//!     "id": "node-123",
//!     "labels": ["Person"]
//! }
//! ```
```

## Configuration Documentation

Each config.rs should document:

1. **Required vs Optional Fields**: Use serde attributes and document defaults
2. **Valid Values**: Document acceptable ranges and formats
3. **Default Values**: Document what defaults are used

### Example Configuration Documentation

```rust
//! Configuration types for HTTP source.
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: http
//! properties:
//!   host: "0.0.0.0"
//!   port: 8080
//!   timeout_ms: 10000
//! ```

/// HTTP source configuration.
///
/// Configures the HTTP server that receives events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSourceConfig {
    /// Host address to bind to. Default: "0.0.0.0"
    #[serde(default = "default_host")]
    pub host: String,

    /// Port to listen on. Default: 8080. Valid range: 1-65535
    #[serde(default = "default_port")]
    pub port: u16,

    /// Request timeout in milliseconds. Default: 10000 (10 seconds)
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}
```

## Unit Test Requirements

Each source should have tests for:

1. **Construction**: Test `new()` and `with_dispatch()` with valid and invalid configs
2. **Basic Properties**: Test `id()`, `type_name()`, `properties()`
3. **Lifecycle**: Test initial status, status transitions
4. **Builder**: Test builder defaults and all options (if builder exists)
5. **Config**: Test serialization, deserialization with defaults, validation
6. **Event Transformation**: Test conversion from input format to SourceChange
7. **Error Handling**: Test error cases and error messages

### Test Organization

Tests should be organized into modules for clarity and maintainability.
Each module focuses on a specific concern:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================================
    // Construction Tests
    // ============================================================================

    mod construction {
        use super::*;
        use drasi_lib::Source;

        #[test]
        fn test_new_with_valid_config() {
            let config = SourceBuilder::new()
                .with_required_field("value")
                .build();
            let source = MySource::new("test-source", config);
            assert!(source.is_ok());
        }

        #[test]
        fn test_new_with_custom_config() {
            let config = SourceConfig { /* custom values */ };
            let source = MySource::new("custom-source", config).unwrap();
            assert_eq!(source.id(), "custom-source");
        }

        #[test]
        fn test_with_dispatch_creates_source() {
            let config = SourceBuilder::new().build();
            let source = MySource::with_dispatch(
                "dispatch-source",
                config,
                Some(DispatchMode::Channel),
                Some(1000),
            );
            assert!(source.is_ok());
        }
    }

    // ============================================================================
    // Properties Tests
    // ============================================================================

    mod properties {
        use super::*;
        use drasi_lib::Source;

        #[test]
        fn test_id_returns_correct_value() {
            let source = create_test_source("my-source");
            assert_eq!(source.id(), "my-source");
        }

        #[test]
        fn test_type_name_returns_source_type() {
            let source = create_test_source("test");
            assert_eq!(source.type_name(), "my_source");
        }

        #[test]
        fn test_properties_contains_expected_fields() {
            let source = create_test_source("test");
            let props = source.properties();
            assert!(props.contains_key("field_name"));
        }

        #[test]
        fn test_properties_excludes_sensitive_fields() {
            // Verify passwords/secrets are not exposed
            let source = create_test_source("test");
            let props = source.properties();
            assert!(props.get("password").is_none());
        }
    }

    // ============================================================================
    // Lifecycle Tests
    // ============================================================================

    mod lifecycle {
        use super::*;
        use drasi_lib::channels::ComponentStatus;
        use drasi_lib::Source;

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = create_test_source("test");
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }

        #[tokio::test]
        async fn test_status_transitions() {
            let source = create_test_source("test");

            // Initial
            assert_eq!(source.status().await, ComponentStatus::Stopped);

            // Start
            source.start().await.unwrap();
            assert_eq!(source.status().await, ComponentStatus::Running);

            // Stop
            source.stop().await.unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    // ============================================================================
    // Builder Tests
    // ============================================================================

    mod builder {
        use super::*;

        #[test]
        fn test_builder_defaults() {
            let config = SourceBuilder::new().build();
            assert_eq!(config.field, "default_value");
        }

        #[test]
        fn test_builder_with_all_options() {
            let config = SourceBuilder::new()
                .with_field1("value1")
                .with_field2(42)
                .build();
            assert_eq!(config.field1, "value1");
            assert_eq!(config.field2, 42);
        }

        #[test]
        fn test_builder_chaining() {
            let config = SourceBuilder::new()
                .with_field("first")
                .with_field("second") // Override
                .build();
            assert_eq!(config.field, "second");
        }

        #[test]
        fn test_builder_default_trait() {
            let builder1 = SourceBuilder::new();
            let builder2 = SourceBuilder::default();
            // Both should produce equivalent results
        }
    }

    // ============================================================================
    // Config Tests
    // ============================================================================

    mod config {
        use super::*;

        #[test]
        fn test_config_serialization() {
            let config = SourceConfig { /* values */ };
            let json = serde_json::to_string(&config).unwrap();
            let deserialized: SourceConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config, deserialized);
        }

        #[test]
        fn test_config_deserialization_with_defaults() {
            let json = r#"{"required_field": "value"}"#;
            let config: SourceConfig = serde_json::from_str(json).unwrap();
            assert_eq!(config.optional_field, "default_value");
        }

        #[test]
        fn test_config_validation_empty_required_field() {
            let config = SourceConfig {
                required_field: "".to_string(),
                // ...
            };
            let result = config.validate();
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("cannot be empty"));
        }
    }

    // ============================================================================
    // Event Transformation Tests
    // ============================================================================

    mod event_transformation {
        use super::*;
        use serde_json::json;

        #[test]
        fn test_insert_event_transformation() {
            let input = json!({ "op": "i", /* ... */ });
            let result = transform_event(input, "source-id").unwrap();
            assert!(matches!(result[0].source_change, SourceChange::Insert { .. }));
        }

        #[test]
        fn test_update_event_transformation() {
            let input = json!({ "op": "u", /* ... */ });
            let result = transform_event(input, "source-id").unwrap();
            assert!(matches!(result[0].source_change, SourceChange::Update { .. }));
        }

        #[test]
        fn test_delete_event_transformation() {
            let input = json!({ "op": "d", /* ... */ });
            let result = transform_event(input, "source-id").unwrap();
            assert!(matches!(result[0].source_change, SourceChange::Delete { .. }));
        }

        #[test]
        fn test_batch_transformation() {
            // Test multiple events in a batch
        }

        #[test]
        fn test_property_type_conversion() {
            // Test string, int, float, bool, null, array, object
        }
    }

    // ============================================================================
    // Error Handling Tests
    // ============================================================================

    mod error_handling {
        use super::*;

        #[test]
        fn test_transform_missing_required_field() {
            let input = json!({ /* missing op */ });
            let result = transform_event(input, "source-id");
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Missing"));
        }

        #[test]
        fn test_transform_invalid_operation_type() {
            let input = json!({ "op": "invalid" });
            let result = transform_event(input, "source-id");
            assert!(result.is_err());
        }
    }
}
```

### Test Helper Functions

Create test helpers to reduce boilerplate:

```rust
#[cfg(test)]
fn create_test_source(id: &str) -> MySource {
    let config = SourceBuilder::new()
        .with_required_field("test-value")
        .build();
    MySource::new(id, config).unwrap()
}
```

## SourceBase Usage Patterns

All sources should use `SourceBase` for common functionality. This ensures consistent behavior
across all source implementations.

### Standard Constructor Pattern

```rust
impl MySource {
    /// Create a new source with the given ID and configuration.
    pub fn new(id: impl Into<String>, config: MySourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    /// Create a new source with custom dispatch settings.
    pub fn with_dispatch(
        id: impl Into<String>,
        config: MySourceConfig,
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
```

### Lifecycle Delegation

The `Source` trait methods should delegate to `SourceBase`:

```rust
#[async_trait]
impl Source for MySource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "my_source"
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        // 1. Set status to Starting
        self.base.set_status(ComponentStatus::Starting).await;

        // 2. Send component event
        self.base.send_component_event(ComponentStatus::Starting, None).await?;

        // 3. Spawn your task
        let task = tokio::spawn(async move { /* ... */ });
        *self.base.task_handle.write().await = Some(task);

        // 4. Set status to Running
        self.base.set_status(ComponentStatus::Running).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use common stop helper
        self.base.stop_common().await
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        // Delegate to base for bootstrap handling
        self.base.subscribe_with_bootstrap(
            query_id, enable_bootstrap, node_labels, relation_labels, "MySource"
        ).await
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
```

### Dispatching Events

From within spawned tasks, use the static dispatch helper:

```rust
SourceBase::dispatch_from_task(
    dispatchers.clone(),
    wrapper,
    &source_id,
).await?;
```

## Review Checklist

When reviewing a source plugin, verify:

- [ ] Module-level documentation exists and includes purpose, configuration, data format, and example
- [ ] All public structs have doc comments
- [ ] All configuration fields are documented with descriptions and defaults
- [ ] All public methods have doc comments
- [ ] Complex logic has inline comments
- [ ] Data format is documented with JSON/YAML examples
- [ ] Unit tests exist for construction, properties, lifecycle, and transformations
- [ ] Tests pass with `cargo test -p drasi-plugin-{name}`
- [ ] No clippy warnings with `cargo clippy -p drasi-plugin-{name}`
- [ ] Code is formatted with `cargo fmt`
