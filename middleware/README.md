# Drasi Middleware

## Introduction

Middleware components are pluggable modules that process incoming changes before they are processed by a continuous query. Middlewares can transform, enrich, filter, or route source-changes and they can be chained together in a pipeline.

Learn more about [middlewares in Drasi here](https://drasi.io/concepts/middleware/).

## Using Drasi Middleware

### Feature Flags

All middleware types are **optional** and disabled by default. Enable only the middleware you need:

```toml
[dependencies]
drasi-middleware = { version = "0.3", features = ["jq", "decoder", "map"] }
```

### Available Middleware Features

- **`jq`** - JQ query language transformations (requires system `jq` library)
- **`bundled-jq`** - JQ transformations with bundled jq compiled from source (requires build tools)
- **`decoder`** - Decode encoded strings (base64, hex, URL encoding)
- **`map`** - JSONPath-based property mapping
- **`parse_json`** - Parse JSON strings into structured objects
- **`promote`** - Promote nested properties to top level
- **`relabel`** - Transform element labels
- **`unwind`** - Unwind arrays into multiple elements
- **`all`** - Enable all middleware (convenience feature, includes `bundled-jq`)

### JQ Middleware Options

You can use the JQ middleware in two ways:

#### Option 1: Using System JQ Library (`jq` feature)
Uses your system's installed jq library. Install jq on your system:

**macOS:**
```bash
brew install jq
```

**Ubuntu/Debian:**
```bash
sudo apt-get install libjq-dev
```

Then enable the `jq` feature:
```toml
[dependencies]
drasi-middleware = { version = "0.3", features = ["jq"] }
```

#### Option 2: Bundled JQ (`bundled-jq` feature)
Compiles jq from source during build. Requires build tools:

**macOS:**
```bash
brew install autoconf automake libtool
```

**Ubuntu/Debian:**
```bash
sudo apt-get install autoconf automake libtool flex bison
```

Then enable the `bundled-jq` feature:
```toml
[dependencies]
drasi-middleware = { version = "0.3", features = ["bundled-jq"] }
```

**Note:** If you don't need JQ middleware, you can use other middleware without installing jq or these build tools.

## Middleware Development Guide

This section explains how to develop custom middleware in Drasi core.

## Core Concepts

### SourceMiddleware and SourceMiddlewareFactory

A middleware in Drasi consists of two main components:

1. **SourceMiddleware**: Implements the processing logic to handle and transform `SourceChange` objects
2. **SourceMiddlewareFactory**: Creates instances of middleware configured with specific parameters

## Getting Started

To create a new middleware, you need to:

1. Create a new Rust module under the `middleware` directory
2. Implement both the `SourceMiddleware` trait and a factory that implements the `SourceMiddlewareFactory` trait
3. Write unit tests for your middleware

## Step-by-Step Guide

### 1. Create a Middleware Module

Create a new directory under the `middleware/src` path with a descriptive name for your middleware:
```
middleware/src/my_middleware/
├── mod.rs
└── tests.rs
```

### 2. Implement the SourceMiddleware Trait

In your `mod.rs` file, implement the `SourceMiddleware` trait:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex,
        MiddlewareError,
        MiddlewareSetupError,
        SourceMiddleware,
        SourceMiddlewareFactory
    },
    models::{Element, SourceChange, SourceMiddlewareConfig},
};

pub struct MyMiddleware {
    // Configuration fields for your middleware
}

#[async_trait]
impl SourceMiddleware for MyMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        // Process the source change and return transformed changes
        match source_change {
            SourceChange::Insert { mut element } => {
                // Transform the element being inserted
                Ok(vec![SourceChange::Insert { element }])
            },
            SourceChange::Update { mut element } => {
                // Transform the element being updated
                Ok(vec![SourceChange::Update { element }])
            },
            SourceChange::Delete { metadata } => {
                // Handle deletion
                Ok(vec![SourceChange::Delete { metadata }])
            },
            SourceChange::Future { .. } => {
                // Pass through future events, or transform if needed
                Ok(vec![source_change])
            },
        }
    }
}
```

### 3. Define Configuration Structure
Create a configuration structure for your middleware:

```rust
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MyMiddlewareConfig {
    // Configuration fields with appropriate types
    pub field_one: String,
    #[serde(default)]
    pub optional_field: bool,
}
```

### 4. Implement the SourceMiddlewareFactory
Create a factory that knows how to instantiate your middleware:
```rust
pub struct MyMiddlewareFactory {}

impl MyMiddlewareFactory {
    pub fn new() -> Self {
        MyMiddlewareFactory {}
    }
}

impl Default for MyMiddlewareFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for MyMiddlewareFactory {
    fn name(&self) -> String {
        "my_middleware".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        // Parse configuration
        let my_config: MyMiddlewareConfig = match serde_json::from_value(
            serde_json::Value::Object(config.config.clone())
        ) {
            Ok(cfg) => cfg,
            Err(e) => {
                return Err(MiddlewareSetupError::InvalidConfiguration(
                    format!("Invalid configuration: {}", e)
                ))
            }
        };

        // Validate configuration
        if my_config.field_one.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(
                "field_one cannot be empty".to_string(),
            ));
        }

        // Create middleware instance
        Ok(Arc::new(MyMiddleware {
            // Initialize with parsed config
        }))
    }
}
```

### 5. Write Tests
In your tests.rs file, create unit tests for your middleware:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use drasi_core::{
        in_memory_index::in_memory_element_index::InMemoryElementIndex,
        interface::SourceMiddlewareFactory,
        models::{
            Element, ElementMetadata, ElementReference, SourceChange,
            SourceMiddlewareConfig, ElementValue,
        },
    };
    use serde_json::json;
    use super::*;

    #[tokio::test]
    async fn test_my_middleware_basic() {
        let factory = MyMiddlewareFactory::new();
        let config = json!({
            "field_one": "test_value",
            "optional_field": true
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "my_middleware".into(),
            config: config.as_object().unwrap().clone(),
        };

        let middleware = factory.create(&mw_config).unwrap();

        // Create a test SourceChange
        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "test_property": "test_value"
                }).into(),
            },
        };

        // Process the source change
        let result = middleware.process(source_change, element_index.as_ref()).await.unwrap();
        
        // Assert expected behavior
        assert_eq!(result.len(), 1);
        // Add specific assertions based on your middleware's expected behavior
    }
    
    #[tokio::test]
    async fn test_invalid_config() {
        let factory = MyMiddlewareFactory::new();
        let config = json!({}); // Missing required fields
        
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "my_middleware".into(),
            config: config.as_object().unwrap().clone(),
        };
        
        // Should return an error for invalid config
        assert!(factory.create(&mw_config).is_err());
    }
}
```

## Best Practices

### Error Handling
- Use descriptive error messages in MiddlewareSetupError returns
- Log errors appropriately with information that helps debugging
- Validate configuration before creating middleware instances

```rust
if config_value_invalid {
    log::warn!("Invalid config value: {}", config_value);
    return Err(MiddlewareSetupError::InvalidConfiguration("Detailed error message".to_string()));
}
```

## Configuration Design
- Make your middleware configurable with reasonable defaults
- Use #[serde(default)] for optional fields
- Consider namespacing configuration fields (e.g., with prefixes)
- Document the configuration options clearly

# Example Middlewares

## Map Middleware
The Map middleware transforms data using JSONPath expressions for more advanced mapping.

Key features:
- Selects data using JSONPath expressions
- Maps input properties to output properties
- Supports different mapping operations (insert, update, delete)

## Unwind Middleware
The Unwind middleware flattens arrays into individual elements.

Key features:
- Extracts array elements into separate nodes
- Creates relationships between parent and extracted elements
- Configurable with JSONPath selectors

## Decoder Middleware
The Decoder middleware decodes encoded strings.

Key features:
- Supports decoding from various formats - base64, hex, etc.
- Supports stripping quotes from encoded strings
- Configurable output property prefix