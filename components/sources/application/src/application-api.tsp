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

/**
 * Application Source API Types
 *
 * The Application source is a special in-process source type that allows
 * applications embedding DrasiServerCore to directly publish changes via
 * Rust API calls rather than external protocols like HTTP or gRPC.
 *
 * This file documents the conceptual data structures. The actual API is
 * implemented in Rust and uses the internal `SourceChange` types directly.
 *
 * Source: server-core/src/sources/application/
 *
 * IMPORTANT: This is a Rust-only API, not an HTTP/JSON API.
 * TypeSpec is used here primarily for documentation purposes.
 * For the actual Rust API, refer to:
 * - drasi_server_core::sources::application::ApplicationSourceHandle
 * - drasi_server_core::sources::application::ApplicationSource
 */

import "@typespec/json-schema";
import "../../../../typespec/core-types.tsp";

using TypeSpec.JsonSchema;
using Drasi.Sources;

namespace Drasi.Sources.Application;

/**
 * Application Source Handle
 *
 * Conceptual representation of the Rust ApplicationSourceHandle trait.
 * Applications implement this trait to receive change events from queries
 * and to publish changes to the Drasi core.
 *
 * Rust Implementation:
 * ```rust
 * #[async_trait]
 * pub trait ApplicationSourceHandle: Send + Sync {
 *     async fn send_change(&self, change: SourceChange) -> Result<()>;
 *     // ... other methods
 * }
 * ```
 */
@jsonSchema
model ApplicationSourceHandle {
  /** Documentation placeholder for Rust trait */
  @extension("x-rust-trait", "drasi_server_core::sources::application::ApplicationSourceHandle")
  description: "This is a Rust trait, not a JSON/HTTP API. See Rust documentation for usage.";
}

/**
 * Conceptual usage example in Rust:
 *
 * ```rust
 * use drasi_server_core::sources::application::ApplicationSourceHandle;
 * use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
 * use std::sync::Arc;
 *
 * // Get handle to application source
 * let handle = core.get_application_source_handle("my-app-source").await?;
 *
 * // Create a node
 * let node = Element::Node {
 *     metadata: ElementMetadata {
 *         reference: ElementReference::new("my-app-source", "user_123"),
 *         labels: Arc::new([Arc::from("User"), Arc::from("Person")]),
 *         effective_from: timestamp_ns(),
 *     },
 *     properties: {
 *         let mut props = ElementPropertyMap::new();
 *         props.insert("name", ElementValue::String(Arc::from("John Doe")));
 *         props.insert("age", ElementValue::Integer(30));
 *         props
 *     },
 * };
 *
 * // Send insert change
 * handle.send_change(SourceChange::Insert { element: node }).await?;
 *
 * // Create a relation
 * let relation = Element::Relation {
 *     metadata: ElementMetadata {
 *         reference: ElementReference::new("my-app-source", "follows_1"),
 *         labels: Arc::new([Arc::from("FOLLOWS")]),
 *         effective_from: timestamp_ns(),
 *     },
 *     out_node: ElementReference::new("my-app-source", "user_123"),
 *     in_node: ElementReference::new("my-app-source", "user_456"),
 *     properties: ElementPropertyMap::new(),
 * };
 *
 * // Send insert change
 * handle.send_change(SourceChange::Insert { element: relation }).await?;
 *
 * // Update a node
 * let updated_node = Element::Node {
 *     metadata: ElementMetadata {
 *         reference: ElementReference::new("my-app-source", "user_123"),
 *         labels: Arc::new([Arc::from("User")]),
 *         effective_from: timestamp_ns(),
 *     },
 *     properties: {
 *         let mut props = ElementPropertyMap::new();
 *         props.insert("name", ElementValue::String(Arc::from("Jane Doe")));
 *         props.insert("age", ElementValue::Integer(31));
 *         props
 *     },
 * };
 *
 * handle.send_change(SourceChange::Update { element: updated_node }).await?;
 *
 * // Delete an element
 * let metadata = ElementMetadata {
 *     reference: ElementReference::new("my-app-source", "user_123"),
 *     labels: Arc::new([Arc::from("User")]),
 *     effective_from: timestamp_ns(),
 * };
 *
 * handle.send_change(SourceChange::Delete { metadata }).await?;
 * ```
 */

/**
 * Application Source Configuration
 *
 * When configuring an application source in YAML, minimal configuration
 * is needed since the source is internal to the application.
 *
 * Example configuration:
 * ```yaml
 * sources:
 *   - id: my-app-source
 *     source_type: application
 *     properties:
 *       # Optional: configure bootstrap provider for initial data
 *       bootstrap_provider:
 *         type: application
 * ```
 */

/**
 * Data Flow
 *
 * 1. Application embeds DrasiServerCore
 * 2. Application creates or retrieves ApplicationSourceHandle
 * 3. Application creates SourceChange events using drasi_core::models types
 * 4. Application calls handle.send_change(change)
 * 5. Drasi routes changes to registered queries
 * 6. Query results are sent to reactions
 *
 * This provides a zero-serialization, in-process integration path for
 * applications that want to use Drasi without external protocols.
 */

/**
 * Related Types
 *
 * The Application source uses the core Drasi types directly:
 * - SourceChange (from common-types.tsp)
 * - Element (from common-types.tsp)
 * - ElementMetadata (from common-types.tsp)
 * - ElementValue (from common-types.tsp)
 *
 * See common-types.tsp for detailed type definitions.
 */

/**
 * Bootstrap Support
 *
 * The Application source supports bootstrap via the Application bootstrap provider,
 * which replays stored insert events. This is configured in the source's
 * bootstrap_provider section.
 *
 * Example:
 * ```yaml
 * sources:
 *   - id: my-app-source
 *     source_type: application
 *     bootstrap_provider:
 *       type: application
 *     properties: {}
 * ```
 */

/**
 * For more information:
 * - Rust documentation: cargo doc --open
 * - Source code: server-core/src/sources/application/
 * - Core models: core/src/models/
 * - Example usage: server-core/tests/
 */
