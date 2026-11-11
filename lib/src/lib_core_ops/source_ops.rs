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

//! Source management operations for DrasiLib
//!
//! This module provides all source-related operations including adding, removing,
//! starting, and stopping sources.
//! String selectors are exact current source names, including UUID-shaped names.
//! Use the paired `_by_id` methods for explicit [`crate::config::ComponentId`]
//! selection.

use futures::stream::Stream;
use std::collections::HashMap;

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
use crate::config::{ComponentId, SourceRuntime};
use crate::error::Result;
use crate::lib_core::DrasiLib;
use crate::schema::{GraphSchema, SourceSchema};
use crate::sources::Source;

impl DrasiLib {
    #[cfg(test)]
    pub(crate) async fn source_instance(
        &self,
        id: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn Source>> {
        self.computation_runtime.source(id).await
    }

    /// Add a source instance to a running server, taking ownership.
    ///
    /// The source instance is wrapped in an Arc internally - callers transfer
    /// ownership rather than pre-wrapping in Arc.
    ///
    /// If the server is running and the source has `auto_start=true`, the source
    /// will be started immediately after being added.
    ///
    /// This returns once the node is declared. Initialization
    /// and activation failures remain visible on that node; use
    /// `add_source_with_handle` to await readiness.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Create the source and transfer ownership
    /// // let source = MySource::new("new-source", config)?;
    /// // core.add_source(source).await?;  // Ownership transferred, auto-started if server running
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_source(
        &self,
        name: impl Into<String>,
        source: impl Source + 'static,
    ) -> Result<()> {
        self.add_source_with_metadata(name, source, HashMap::new())
            .await
    }

    /// Declare a source and return its generation-bound computation handle.
    ///
    /// A successful return confirms graph addition, not initialization or activation. Await
    /// `wait_created()` or `wait_started()` on the handle for those outcomes.
    /// `wait_started()` requires the source to report `Running`, not merely to
    /// return successfully from its start hook.
    /// Auto-start is requested only when the instance is running.
    /// Use the handle's `control()` and `set_control_handler()` methods for
    /// out-of-band notifications without changing the legacy source contract.
    /// Ownership-bearing native rejections retain their `GraphError` through
    /// `DrasiError::Internal`, including the rejected addition's recovery handle.
    pub async fn add_source_with_handle(
        &self,
        name: impl Into<String>,
        source: impl Source + 'static,
    ) -> Result<crate::computation::v1::ComponentHandle> {
        self.add_source_with_metadata_and_handle(name, source, HashMap::new())
            .await
    }

    /// Declare a native factory-built source in the instance's computation graph.
    ///
    /// Use [`crate::computation::v1::ComponentAddition::from_specification`] to
    /// supply a factory, or `ComponentAddition::declaration` when it will be
    /// registered later. The addition carries its output streams, dependencies
    /// and auto-start policy. Its semantic kind must be `Source` (the default
    /// for the native source role).
    ///
    /// Construction happens only after the graph owns the declaration. Missing
    /// factories/resources remain dependency waits; configuration, secret
    /// resolution and constructor failures remain inspectable on the node.
    /// Success acknowledges declaration, not readiness. Use the returned handle
    /// to await creation, and [`Self::start_source`] to retry failed creation.
    /// This does not construct legacy [`Source`] objects from native recipes;
    /// the existing object-based [`Self::add_source`] is unchanged.
    pub async fn add_source_from_factory(
        &self,
        addition: crate::computation::v1::ComponentAddition,
    ) -> Result<crate::computation::v1::ComponentHandle> {
        self.add_factory_component(
            addition,
            crate::computation::v1::ComponentSemanticKind::Source,
        )
        .await
    }

    async fn add_source_with_metadata_and_handle(
        &self,
        name: impl Into<String>,
        source: impl Source + 'static,
        metadata: HashMap<String, String>,
    ) -> Result<crate::computation::v1::ComponentHandle> {
        self.state_guard.require_initialized()?;
        let id = source.id().to_owned();
        let name =
            crate::config::ComponentName::try_new(name.into()).map_err(anyhow::Error::from)?;
        self.computation_runtime
            .add_source(name, Box::new(source), metadata, self.is_running().await)
            .await
            .and_then(|handle| self.computation_runtime.public_handle(handle))
            .map_err(|error| crate::computation::runtime::map_addition_error("source", &id, error))
    }

    /// Add a source to a running server with additional metadata.
    ///
    /// Same as [`add_source`](Self::add_source) but merges `extra_metadata`
    /// (e.g. `pluginId`) into the component graph node.
    pub async fn add_source_with_metadata(
        &self,
        name: impl Into<String>,
        source: impl Source + 'static,
        extra_metadata: HashMap<String, String>,
    ) -> Result<()> {
        self.state_guard.require_initialized()?;
        self.add_source_with_metadata_and_handle(name, source, extra_metadata)
            .await
            .map(|_| ())
    }

    /// Create and admit a source with a library-assigned UUID.
    /// Opaque constructors are memory-only; persistent instances require managed recipes.
    pub async fn create_source<S: Source + 'static>(
        &self,
        name: impl Into<String>,
        auto_start: bool,
        factory: impl FnOnce(crate::config::ComponentId) -> anyhow::Result<S> + Send + 'static,
    ) -> Result<crate::computation::v1::ComponentHandle> {
        self.state_guard.require_initialized()?;
        let name =
            crate::config::ComponentName::try_new(name.into()).map_err(anyhow::Error::from)?;
        self.computation_runtime
            .create_source(name, auto_start, move |id| {
                Ok(Box::new(factory(id)?) as Box<dyn Source>)
            })
            .await
            .and_then(|handle| self.computation_runtime.public_handle(handle))
            .map_err(Into::into)
    }

    /// Declare an owned asynchronous source constructor. Admission returns its
    /// UUID on admission; await `wait_created` on the handle for construction.
    /// The graph owns the future, including after a caller cancels its wait.
    /// Constructors must release partial acquisitions if their future is dropped
    /// by stop/shutdown or the construction deadline. Returned objects are retained
    /// before validation and cleaned up by the graph.
    pub async fn create_source_async<S, F, Fut>(
        &self,
        name: impl Into<String>,
        requested_id: Option<crate::config::ComponentId>,
        auto_start: bool,
        factory: F,
    ) -> Result<crate::computation::v1::ComponentHandle>
    where
        S: Source + 'static,
        F: FnOnce(crate::config::ComponentId) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<(S, HashMap<String, String>)>>
            + Send
            + 'static,
    {
        self.state_guard.require_initialized()?;
        let name =
            crate::config::ComponentName::try_new(name.into()).map_err(anyhow::Error::from)?;
        let handle = self
            .computation_runtime
            .create_source_async(
                name,
                requested_id,
                auto_start,
                Box::new(move |id| {
                    Box::pin(async move {
                        let (source, metadata) = factory(id).await?;
                        Ok((Box::new(source) as Box<dyn Source>, metadata))
                    })
                }),
                self.is_running().await,
            )
            .await?;
        self.computation_runtime
            .public_handle(handle)
            .map_err(Into::into)
    }

    /// Remove a source from a running server
    ///
    /// If the source is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_source("old-source", false).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_source(&self, name: &str, cleanup: bool) -> Result<()> {
        let id = self.resolve_component_name(name, "source")?;
        self.remove_source_by_id(&id, cleanup).await
    }

    /// Remove the source with the supplied immutable UUID.
    pub async fn remove_source_by_id(&self, id: &ComponentId, cleanup: bool) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime
                .remove_component(id.as_str(), "source", cleanup)
                .await,
            "source",
            id.as_str(),
            "remove",
        )
    }

    /// Submit an updated registered source recipe through the configuration owner.
    ///
    /// The UUID and graph-owned name must match the existing source. Its registered
    /// factory assesses whether to retain, restart or reconstruct the object;
    /// creation support alone does not permit updates. Opaque objects and
    /// process-local creation callbacks require explicit removal and addition.
    /// Use `rename_component` for a metadata-only name change.
    pub async fn update_source(
        &self,
        name: &str,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        let id = self.resolve_component_name(name, "source")?;
        self.update_source_by_id(&id, definition).await
    }

    /// Update the source with the supplied immutable UUID.
    pub async fn update_source_by_id(
        &self,
        id: &ComponentId,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime
                .update_ordinary_definition(id.as_str(), "source", definition)
                .await,
            "source",
            id.as_str(),
            "update",
        )
    }

    /// Start a stopped source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_source(&self, name: &str) -> Result<()> {
        let id = self.resolve_component_name(name, "source")?;
        self.start_source_by_id(&id).await
    }

    /// Start the source with the supplied immutable UUID.
    ///
    /// Use a `ComponentId` obtained from an admission handle or
    /// [`Self::component_by_name`]; UUID text passed to [`Self::start_source`]
    /// is always interpreted as a source name.
    pub async fn start_source_by_id(&self, id: &ComponentId) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime
                .start_component(id.as_str(), "source")
                .await,
            "source",
            id.as_str(),
            "start",
        )
    }

    /// Stop a running, starting, or failed source.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_source(&self, name: &str) -> Result<()> {
        let id = self.resolve_component_name(name, "source")?;
        self.stop_source_by_id(&id).await
    }

    /// Stop the source with the supplied immutable UUID.
    pub async fn stop_source_by_id(&self, id: &ComponentId) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime
                .stop_component(id.as_str(), "source")
                .await,
            "source",
            id.as_str(),
            "stop",
        )
    }

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(&self) -> Result<Vec<(String, ComponentStatus)>> {
        self.inspection.list_sources().await
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(&self, name: &str) -> Result<SourceRuntime> {
        let id = self.resolve_component_name(name, "source")?;
        self.get_source_info_by_id(&id).await
    }

    /// Get detailed source information by immutable UUID.
    pub async fn get_source_info_by_id(&self, id: &ComponentId) -> Result<SourceRuntime> {
        self.inspection.get_source_info(id.as_str()).await
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(&self, name: &str) -> Result<ComponentStatus> {
        let id = self.resolve_component_name(name, "source")?;
        self.get_source_status_by_id(&id).await
    }

    /// Get source status by immutable UUID.
    pub async fn get_source_status_by_id(&self, id: &ComponentId) -> Result<ComponentStatus> {
        self.inspection.get_source_status(id.as_str()).await
    }

    /// Get best-effort schema information for a specific source.
    pub async fn get_source_schema(&self, name: &str) -> Result<Option<SourceSchema>> {
        let id = self.resolve_component_name(name, "source")?;
        self.get_source_schema_by_id(&id).await
    }

    /// Get best-effort source schema information by immutable UUID.
    pub async fn get_source_schema_by_id(&self, id: &ComponentId) -> Result<Option<SourceSchema>> {
        self.inspection.get_source_schema(id.as_str()).await
    }

    /// Get the merged graph schema across all registered sources and queries.
    pub async fn get_graph_schema(&self) -> Result<GraphSchema> {
        self.inspection.get_graph_schema().await
    }

    /// Get lifecycle events for a specific source as an async stream.
    ///
    /// Returns events in chronological order (oldest first). Up to 100 most recent
    /// events are retained per component.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_source_events("my-source").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_events(
        &self,
        name: &str,
    ) -> Result<impl Stream<Item = ComponentEvent>> {
        let id = self.resolve_component_name(name, "source")?;
        self.get_source_events_by_id(&id).await
    }

    /// Get source lifecycle events by immutable UUID.
    pub async fn get_source_events_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_source_events(id.as_str()).await
    }

    /// Get all lifecycle events across all sources as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_source_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_source_events(&self) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_all_source_events().await
    }

    /// Subscribe to live logs for a source.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the source.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;
    ///
    /// // Print historical logs
    /// for log in history {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    ///
    /// // Listen for new logs
    /// while let Ok(log) = receiver.recv().await {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_source_logs(
        &self,
        name: &str,
    ) -> Result<(
        Vec<crate::observability::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::observability::LogMessage>,
    )> {
        let id = self.resolve_component_name(name, "source")?;
        self.subscribe_source_logs_by_id(&id).await
    }

    /// Subscribe to source logs by immutable UUID.
    pub async fn subscribe_source_logs_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<(
        Vec<crate::observability::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::observability::LogMessage>,
    )> {
        self.inspection.subscribe_source_logs(id.as_str()).await
    }

    /// Subscribe to live events for a source.
    ///
    /// Returns the event history (oldest first) and a broadcast receiver for new events
    /// as they occur. Events include lifecycle status changes such as Starting, Running,
    /// Error, Stopped.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> anyhow::Result<()> {
    /// let (history, mut receiver) = core.subscribe_source_events("my-source").await?;
    ///
    /// // Print historical events
    /// for event in history {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    ///
    /// // Listen for new events
    /// while let Ok(event) = receiver.recv().await {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_source_events(
        &self,
        name: &str,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let id = self.resolve_component_name(name, "source")?;
        self.subscribe_source_events_by_id(&id).await
    }

    /// Subscribe to source lifecycle events by immutable UUID.
    pub async fn subscribe_source_events_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.inspection.subscribe_source_events(id.as_str()).await
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
    use crate::channels::ComponentStatus;
    use crate::channels::{SourceEventWrapper, SubscriptionResponse};
    use crate::component_graph::ComponentStatusHandle;
    use crate::error::DrasiError;
    use crate::lib_core::DrasiLib;
    use crate::schema::{NodeSchema, PropertySchema, PropertyType, RelationSchema, SourceSchema};
    use crate::sources::tests::{create_test_mock_source, TestMockSource};
    use crate::test_helpers::creation::CoreFixtures;
    use crate::test_helpers::wait_for_component_status;
    use crate::{Query, Source};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::time::Duration;

    struct SchemaAwareSource {
        id: String,
        status_handle: ComponentStatusHandle,
        dispatchers:
            std::sync::Arc<tokio::sync::RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    }

    impl SchemaAwareSource {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                status_handle: ComponentStatusHandle::new(id),
                dispatchers: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Source for SchemaAwareSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "schema-aware"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn auto_start(&self) -> bool {
            false
        }

        fn describe_schema(&self) -> Option<SourceSchema> {
            Some(SourceSchema {
                nodes: vec![
                    NodeSchema {
                        label: "Sensor".to_string(),
                        properties: vec![PropertySchema {
                            name: "temperature".to_string(),
                            data_type: Some(PropertyType::Float),
                            description: None,
                        }],
                    },
                    NodeSchema {
                        label: "Counter".to_string(),
                        properties: vec![PropertySchema {
                            name: "value".to_string(),
                            data_type: Some(PropertyType::Integer),
                            description: None,
                        }],
                    },
                ],
                relations: vec![RelationSchema {
                    label: "CONNECTS_TO".to_string(),
                    from: Some("Sensor".to_string()),
                    to: Some("Counter".to_string()),
                    properties: vec![PropertySchema {
                        name: "strength".to_string(),
                        data_type: Some(PropertyType::Float),
                        description: None,
                    }],
                }],
            })
        }

        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, Some("Source started".to_string()))
                .await;
            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, Some("Source stopped".to_string()))
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }

        async fn subscribe(
            &self,
            settings: crate::config::SourceSubscriptionSettings,
        ) -> Result<SubscriptionResponse> {
            let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(16);
            let receiver = dispatcher.create_receiver().await?;
            self.dispatchers.write().await.push(Box::new(dispatcher));

            Ok(SubscriptionResponse {
                query_id: settings.query_id,
                source_id: self.id.clone(),
                receiver,
                bootstrap_receiver: None,
                position_handle: None,
                bootstrap_result_receiver: None,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }
    }

    /// Helper: build a DrasiLib, start it, and return it.
    async fn build_and_start() -> DrasiLib {
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("test")
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        core
    }

    // ========================================================================
    // add_source
    // ========================================================================

    #[tokio::test]
    async fn add_source_happy_path() {
        let core = build_and_start().await;
        let source = create_test_mock_source("src-1".to_string());

        let handle = core.add_source_fixture_with_handle(source).await.unwrap();
        handle.wait_created().await.unwrap();

        let sources = core.list_sources().await.unwrap();
        assert!(
            sources.iter().any(|(id, _)| id == handle.id().as_str()),
            "Added source should appear in list"
        );
        assert_eq!(core.get_source_info("src-1").await.unwrap().name, "src-1");
        assert_eq!(
            core.get_source_info_by_id(handle.id()).await.unwrap().name,
            "src-1"
        );
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn add_source_duplicate_returns_error() {
        let core = build_and_start().await;
        let s1 = create_test_mock_source("dup-src".to_string());
        let s2 = create_test_mock_source("dup-src".to_string());

        let existing = core.add_source_fixture_with_handle(s1).await.unwrap();
        let err = core.add_source_fixture(s2).await.unwrap_err();
        assert!(
            matches!(err.downcast_ref::<crate::computation::v1::GraphError>(),
            Some(crate::computation::v1::GraphError::DuplicateComponentName { existing: conflict })
                if &conflict.id == existing.id() && conflict.name.as_str() == "dup-src")
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // remove_source
    // ========================================================================

    #[tokio::test]
    async fn remove_source_happy_path() {
        let core = build_and_start().await;
        let source = TestMockSource::fixture("rm-src", false);

        let handle = core.add_source_fixture_with_handle(source).await.unwrap();
        let source_id = handle.id().to_string();
        assert!(core
            .list_sources()
            .await
            .unwrap()
            .iter()
            .any(|(id, _)| id == &source_id));

        core.remove_source("rm-src", false).await.unwrap();
        assert!(
            !core
                .list_sources()
                .await
                .unwrap()
                .iter()
                .any(|(id, _)| id == &source_id),
            "Removed source should not appear in list"
        );
        assert!(handle.observed().is_err());
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remove_source_nonexistent_returns_error() {
        let core = build_and_start().await;
        let err = core
            .remove_source("missing-source", false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Removing a missing source name should fail lookup, got: {err:?}"
        );

        let missing = crate::config::ComponentId::generate();
        let err = core.remove_source_by_id(&missing, false).await.unwrap_err();

        assert!(
            matches!(err.classification(), DrasiError::OperationFailed { .. }),
            "Removing a missing source UUID should preserve the identity operation failure, got: {err:?}"
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // list_sources
    // ========================================================================

    #[tokio::test]
    async fn list_sources_empty_then_populated() {
        let core = build_and_start().await;

        let initial = core.list_sources().await.unwrap();
        assert!(initial.is_empty(), "No sources initially");

        // Add two sources
        let s1 = TestMockSource::fixture("list-s1", false);
        let s2 = TestMockSource::fixture("list-s2", false);
        core.add_source_fixture(s1).await.unwrap();
        core.add_source_fixture(s2).await.unwrap();

        let after = core.list_sources().await.unwrap();
        assert_eq!(after.len(), 2, "Should have 2 sources");
        assert_ne!(
            core.component_by_name("list-s1").unwrap().id(),
            core.component_by_name("list-s2").unwrap().id()
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // get_source_status
    // ========================================================================

    #[tokio::test]
    async fn get_source_status_returns_correct_status() {
        let core = build_and_start().await;
        let source = TestMockSource::fixture("status-src", false);

        let source = core.add_source_fixture_with_handle(source).await.unwrap();

        let status = core.get_source_status("status-src").await.unwrap();
        assert_eq!(
            status,
            ComponentStatus::Added,
            "Newly added non-autostart source should be Added"
        );
        assert_eq!(
            core.get_source_status_by_id(source.id()).await.unwrap(),
            ComponentStatus::Added
        );
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn get_source_status_nonexistent_returns_error() {
        let core = build_and_start().await;
        let err = core.get_source_status("missing-source").await.unwrap_err();

        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Status of nonexistent source should return ComponentNotFound, got: {err:?}"
        );
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // start_source / stop_source lifecycle
    // ========================================================================

    #[tokio::test]
    async fn start_and_stop_source_lifecycle() {
        let core = build_and_start().await;

        // Subscribe BEFORE adding so we catch all events
        let mut event_rx = core.subscribe_all_component_events();

        let source = TestMockSource::fixture("lifecycle-src", false);
        let source = core.add_source_fixture_with_handle(source).await.unwrap();
        let source_id = source.id().as_str();

        // Initially in Added state
        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);

        // Start
        core.start_source("lifecycle-src").await.unwrap();
        wait_for_component_status(
            &mut event_rx,
            source_id,
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop
        core.stop_source("lifecycle-src").await.unwrap();
        wait_for_component_status(
            &mut event_rx,
            source_id,
            ComponentStatus::Stopped,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // get_source_info
    // ========================================================================

    #[tokio::test]
    async fn get_source_info_returns_correct_fields() {
        let core = build_and_start().await;
        let source = TestMockSource::fixture("info-src", false);

        let source = core.add_source_fixture_with_handle(source).await.unwrap();
        source.wait_created().await.unwrap();

        let info = core.get_source_info("info-src").await.unwrap();
        assert_eq!(info.id, source.id().as_str());
        assert_eq!(info.name, "info-src");
        assert_eq!(info.source_type, "mock");
        assert_eq!(info.status, ComponentStatus::Added);
        assert!(info.error_message.is_none());
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn get_source_schema_returns_opt_in_schema() {
        let core = build_and_start().await;
        let source = core
            .create_source("schema-src", false, |id| {
                Ok(SchemaAwareSource::new(id.as_str()))
            })
            .await
            .unwrap();
        source.wait_created().await.unwrap();

        let schema = core
            .get_source_schema("schema-src")
            .await
            .unwrap()
            .expect("schema-aware source should expose schema");

        assert_eq!(schema.nodes.len(), 2);
        assert!(schema.nodes.iter().any(|node| node.label == "Sensor"));
        assert!(schema
            .relations
            .iter()
            .any(|relation| relation.label == "CONNECTS_TO"));
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn get_graph_schema_merges_source_and_query_context_end_to_end() {
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("schema-e2e")
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        let source = core
            .create_source("schema-src", false, |id| {
                Ok(SchemaAwareSource::new(id.as_str()))
            })
            .await
            .unwrap();
        source.wait_created().await.unwrap();
        let query = core
            .add_query_with_handle(
                Query::cypher("schema-query")
                    .query("MATCH (s:Sensor)-[r:CONNECTS_TO]->(c:Counter) RETURN s, r, c")
                    .from_input("schema-src")
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();

        let schema = core.get_graph_schema().await.unwrap();

        let sensor = schema
            .nodes
            .get("Sensor")
            .expect("Sensor label should exist");
        assert!(sensor.sources.contains(source.id().as_str()));
        assert!(sensor.queried_by.contains(query.id().as_str()));
        assert!(sensor
            .properties
            .iter()
            .any(|property| property.name == "temperature"));

        let relation = schema
            .relations
            .get("CONNECTS_TO")
            .expect("CONNECTS_TO relation should exist");
        assert!(relation.sources.contains(source.id().as_str()));
        assert!(relation.queried_by.contains(query.id().as_str()));
        assert_eq!(relation.from.as_deref(), Some("Sensor"));
        assert_eq!(relation.to.as_deref(), Some("Counter"));
        core.shutdown().await.unwrap();
    }
}
