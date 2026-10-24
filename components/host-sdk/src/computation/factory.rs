// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    loader::PluginOwner,
    proxy::{BoundConsumerProgress, BoundProgress, NativeComponentProxy, SourceBindings},
};
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{checked_table, take_status},
    wire::{self, CreateRequest, Scope},
};
use drasi_lib::computation::v1::*;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) fn runtime_configuration_schema(
    schema: &drasi_computation_plugin_sdk::ConfigSchema,
) -> ConfigurationSchema {
    use drasi_computation_plugin_sdk::ConfigType;
    ConfigurationSchema {
        fields: schema
            .fields
            .iter()
            .map(|(name, field)| {
                (
                    Arc::from(name.as_str()),
                    ConfigurationField {
                        value_type: match field.value_type {
                            ConfigType::Boolean => ConfigurationType::Boolean,
                            ConfigType::Integer => ConfigurationType::Integer,
                            ConfigType::String => ConfigurationType::String,
                            ConfigType::Object => ConfigurationType::Object,
                            ConfigType::Array => ConfigurationType::Array,
                            ConfigType::Json => ConfigurationType::Json,
                        },
                        required: field.required,
                        secret: field.secret,
                    },
                )
            })
            .collect(),
        allow_additional: schema.allow_additional,
    }
}

fn runtime_descriptor(metadata: &FactoryMetadata) -> FactoryDescriptor {
    FactoryDescriptor {
        implementation: metadata.implementation.clone(),
        role: metadata.role,
        configuration_version: metadata.configuration_version,
        configuration: runtime_configuration_schema(&metadata.configuration),
        dependencies: BTreeMap::new(),
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
pub(super) mod recovery_tests;

pub struct NativeFactory {
    metadata: FactoryMetadata,
    configuration_details: Option<drasi_computation_plugin_sdk::ConfigDetails>,
    descriptor: FactoryDescriptor,
    raw: abi::FactoryHandle,
    _plugin: Arc<PluginOwner>,
    codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    admission: Option<abi::services::PluginServicesV1>,
    recovery: Option<abi::recovery::PluginRecoveryV1>,
    consumer_progress: Option<abi::consumer_progress::PluginConsumerProgressV1>,
    consumer: Option<(
        drasi_computation_plugin_sdk::ConsumerMode,
        abi::consumer::PluginConsumerV1,
    )>,
    batching: Option<abi::batching::PluginBatchingV1>,
    query_snapshots: Option<abi::query_snapshots::PluginQuerySnapshotsV1>,
    source_schema: Option<abi::source_schema::PluginSourceSchemaV1>,
    bootstrap_coordination: Option<abi::bootstrap_coordination::PluginBootstrapCoordinationV1>,
    activation_credentials: Option<abi::activation_credentials::PluginActivationCredentialsV1>,
}
unsafe impl Send for NativeFactory {}
unsafe impl Sync for NativeFactory {}
impl NativeFactory {
    pub(super) unsafe fn new(
        metadata: FactoryMetadata,
        raw: abi::FactoryHandle,
        plugin: Arc<PluginOwner>,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        admission: Option<abi::services::PluginServicesV1>,
        recovery: Option<abi::recovery::PluginRecoveryV1>,
    ) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null() && table.create.is_some() && table.release.is_some(),
            "incomplete native factory table"
        );
        let mut descriptor = runtime_descriptor(&metadata);
        if admission.is_some() {
            let mut requirement =
                ResourceRequirement::exactly_one::<QosChannel>(ResourceRole::StateStore);
            requirement.minimum = 0;
            descriptor
                .dependencies
                .insert(Arc::from("admission"), requirement);
        }
        if recovery.is_some() {
            let mut requirement = ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                ResourceRole::Checkpoint,
            );
            requirement.minimum = 0;
            descriptor
                .dependencies
                .insert(Arc::from("source_progress"), requirement);
        }
        if metadata.role == ComponentRole::Transformer
            || (metadata.role == ComponentRole::Sink
                && metadata.completion == Some(SinkCompletion::Handled))
        {
            let mut requirement = ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                ResourceRole::Checkpoint,
            );
            requirement.minimum = 0;
            descriptor
                .dependencies
                .insert("input_progress".into(), requirement);
        }
        Ok(Self {
            descriptor,
            metadata,
            configuration_details: None,
            raw,
            _plugin: plugin,
            codec,
            schemas,
            admission,
            recovery,
            consumer_progress: None,
            consumer: None,
            batching: None,
            query_snapshots: None,
            source_schema: None,
            bootstrap_coordination: None,
            activation_credentials: None,
        })
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }
    pub(super) fn with_activation_credentials(
        mut self,
        extension: Option<abi::activation_credentials::PluginActivationCredentialsV1>,
    ) -> anyhow::Result<Self> {
        if let Some(extension) = extension {
            let response = unsafe {
                drasi_computation_plugin_sdk::transport::take_reply(extension
                    .factory
                    .expect("validated")(
                    self.raw.state
                ))?
            };
            if wire::decode::<bool>(&response)? {
                let mut requirement = ResourceRequirement::exactly_one::<
                    super::NativeIdentityResource,
                >(ResourceRole::Identity);
                requirement.minimum = 0;
                self.descriptor
                    .dependencies
                    .insert("identity".into(), requirement);
                self.activation_credentials = Some(extension);
            }
        }
        Ok(self)
    }

    pub fn supports_activation_credentials(&self) -> bool {
        self.activation_credentials.is_some()
    }

    pub fn create_component_with_identity(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        identity: Arc<super::NativeIdentityResource>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound_with_identity(
            CreateRequest {
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: None,
                activation_credentials_bound: false,
            },
            None,
            None,
            false,
            None,
            None,
            None,
            None,
            Some(identity),
        )
    }
    pub(super) fn with_bootstrap_coordination(
        mut self,
        extension: Option<abi::bootstrap_coordination::PluginBootstrapCoordinationV1>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            extension.is_none()
                || (self.metadata.role == ComponentRole::Source
                    && (self.recovery.is_some() ^ self.consumer_progress.is_some())),
            "source bootstrap coordination requires one negotiated source progress mode"
        );
        if extension.is_some() {
            let mut requirement = ResourceRequirement::exactly_one::<
                super::NativeSourceBootstrapResource,
            >(ResourceRole::Bootstrap);
            requirement.minimum = 0;
            self.descriptor
                .dependencies
                .insert("source_bootstrap".into(), requirement);
        }
        self.bootstrap_coordination = extension;
        Ok(self)
    }
    pub fn supports_source_bootstrap(&self) -> bool {
        self.bootstrap_coordination.is_some()
    }
    pub fn source_bootstrap_uses_consumer_progress(&self) -> bool {
        self.consumer_progress.is_some()
    }
    pub fn create_source_bootstrap(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        progress: super::NativeSourceBootstrapProgress,
    ) -> anyhow::Result<Arc<super::NativeSourceBootstrapResource>> {
        self.create_source_bootstrap_with_identity(id, configuration, scope, progress, None)
    }
    pub fn create_source_bootstrap_with_identity(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        progress: super::NativeSourceBootstrapProgress,
        identity: Option<Arc<super::NativeIdentityResource>>,
    ) -> anyhow::Result<Arc<super::NativeSourceBootstrapResource>> {
        anyhow::ensure!(
            identity.is_none() || self.activation_credentials.is_some(),
            "source bootstrap identity requires activation credentials"
        );
        let extension = self
            .bootstrap_coordination
            .ok_or_else(|| anyhow::anyhow!("native factory has no source bootstrap coordinator"))?;
        anyhow::ensure!(
            matches!(&progress, super::NativeSourceBootstrapProgress::Shared(_))
                == self.consumer_progress.is_some(),
            "source bootstrap progress mode does not match its source factory"
        );
        self.validate_configuration(&configuration)?;
        super::NativeSourceBootstrapResource::create(
            CreateRequest {
                activation_credentials_bound: identity.is_some(),
                id,
                configuration,
                scope: Some(scope),
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
            },
            progress,
            extension,
            self.raw.state,
            self._plugin.clone(),
            identity,
        )
    }
    pub(super) fn with_source_schema(
        mut self,
        extension: Option<abi::source_schema::PluginSourceSchemaV1>,
    ) -> Self {
        self.source_schema = extension;
        self
    }
    pub(super) fn with_configuration_details(
        mut self,
        details: Option<drasi_computation_plugin_sdk::ConfigDetails>,
    ) -> Self {
        self.configuration_details = details;
        self
    }
    pub fn configuration_details(&self) -> Option<&drasi_computation_plugin_sdk::ConfigDetails> {
        self.configuration_details.as_ref()
    }
    pub fn configuration_schema(&self) -> serde_json::Value {
        drasi_computation_plugin_sdk::configuration::configuration_json_schema(
            &self.metadata.configuration,
            self.configuration_details(),
        )
    }
    fn validate_configuration(&self, configuration: &serde_json::Value) -> anyhow::Result<()> {
        self.metadata.configuration.validate(configuration)?;
        if let Some(details) = &self.configuration_details {
            details.validate(configuration)?;
        }
        Ok(())
    }
    pub(super) fn with_consumer(
        mut self,
        consumer: Option<(
            drasi_computation_plugin_sdk::ConsumerMode,
            abi::consumer::PluginConsumerV1,
        )>,
    ) -> anyhow::Result<Self> {
        if let Some((mode, _)) = consumer {
            mode.validate_factory(&self.metadata)?;
            self.descriptor.dependencies.insert(
                "consumer".into(),
                ResourceRequirement::exactly_one::<super::NativeConsumerResource>(
                    ResourceRole::IndexBackend,
                ),
            );
            self.descriptor.dependencies.remove("input_progress");
        }
        self.consumer = consumer;
        Ok(self)
    }
    pub fn consumer_mode(&self) -> Option<drasi_computation_plugin_sdk::ConsumerMode> {
        self.consumer.map(|(mode, _)| mode)
    }
    fn supports_input_progress(&self) -> bool {
        self.descriptor.dependencies.contains_key("input_progress")
    }
    pub(super) fn with_consumer_progress(
        mut self,
        progress: Option<abi::consumer_progress::PluginConsumerProgressV1>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            progress.is_none()
                || (matches!(
                    self.metadata.role,
                    ComponentRole::Source | ComponentRole::Transformer
                ) && self.recovery.is_none()
                    && self.consumer.is_none()),
            "consumer progress requires a producer without single-owner progress"
        );
        if progress.is_some() {
            self.descriptor.dependencies.insert(
                "consumer_progress".into(),
                ResourceRequirement::exactly_one::<ConsumerProgressSet>(ResourceRole::Checkpoint),
            );
        }
        self.consumer_progress = progress;
        Ok(self)
    }
    pub(super) fn with_batching(
        mut self,
        batching: Option<abi::batching::PluginBatchingV1>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            batching.is_none()
                || (self.metadata.role == ComponentRole::Sink && self.consumer.is_none()),
            "native batching requires a sink without an independent consumer wrapper"
        );
        self.batching = batching;
        Ok(self)
    }
    pub(super) fn with_query_snapshots(
        mut self,
        snapshots: Option<abi::query_snapshots::PluginQuerySnapshotsV1>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            snapshots.is_none()
                || (self.metadata.role == ComponentRole::Sink && self.consumer.is_none()),
            "native query-snapshot reads require a sink without an independent consumer wrapper"
        );
        if snapshots.is_some() {
            let mut requirement = ResourceRequirement::exactly_one::<
                super::NativeQuerySnapshotsResource,
            >(ResourceRole::QueryCatalog);
            requirement.minimum = 0;
            self.descriptor
                .dependencies
                .insert("query_snapshots".into(), requirement);
        }
        self.query_snapshots = snapshots;
        Ok(self)
    }
    pub async fn create_consumer(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        options: DeliveryOptions,
    ) -> anyhow::Result<super::NativeConsumerProxy> {
        let (mode, extension) = self
            .consumer
            .ok_or_else(|| anyhow::anyhow!("native factory has no consumer service"))?;
        let component = self.construct_bound(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope.clone()),
            },
            None,
            None,
            true,
            None,
            None,
            None,
            None,
        )?;
        let response = unsafe {
            drasi_computation_plugin_sdk::transport::take_reply(extension
                .inspect
                .expect("validated")(
                component.inner.state()
            ))?
        };
        let actual: drasi_computation_plugin_sdk::consumer::FactoryConsumer =
            wire::decode(&response)?;
        anyhow::ensure!(
            actual.version == abi::consumer::VERSION && actual.mode == Some(mode),
            "native consumer changed its negotiated mode"
        );
        super::NativeConsumerProxy::new(
            component,
            extension,
            mode,
            scope,
            provider,
            options,
            self.schemas.clone(),
        )
        .await
    }

    /// Convenience for configuration-only construction outside topology import.
    /// The returned object still has the real graph interfaces/lifecycle hooks.
    pub fn create_component(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: None,
            },
            None,
            None,
        )
    }

    pub fn create_component_with_query_snapshots(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        snapshots: Arc<super::NativeQuerySnapshotsResource>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope),
            },
            None,
            None,
            false,
            Some(snapshots),
            None,
            None,
            None,
        )
    }

    pub fn create_component_with_consumer_progress(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        progress: Arc<ConsumerProgressSet>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope),
            },
            None,
            None,
            false,
            None,
            Some(progress),
            None,
            None,
        )
    }

    pub fn create_component_with_source_bootstrap(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        coordinator: Arc<super::NativeSourceBootstrapResource>,
        progress: super::NativeSourceBootstrapProgress,
    ) -> anyhow::Result<NativeComponentProxy> {
        let (single, shared) = match progress {
            super::NativeSourceBootstrapProgress::Single(owner) => (Some(owner), None),
            super::NativeSourceBootstrapProgress::Shared(owners) => (None, Some(owners)),
        };
        self.construct_bound(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope),
            },
            None,
            single,
            false,
            None,
            shared,
            None,
            Some(coordinator),
        )
    }

    /// Opt into graph-owned volatile handled-input tracking. The input owner is
    /// never passed to the plugin; downstream consumer reads remain independent.
    pub fn create_component_with_input_progress(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        input_progress: Arc<QuerySourceProgress>,
        consumer_progress: Option<Arc<ConsumerProgressSet>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound(
            CreateRequest {
                activation_credentials_bound: false,
                id,
                implementation: self.metadata.implementation.clone(),
                configuration_version: self.metadata.configuration_version,
                configuration,
                scope: Some(scope),
            },
            None,
            None,
            false,
            None,
            consumer_progress,
            Some(input_progress),
            None,
        )
    }

    /// Original, literal construction recipe; the graph retains this for pending
    /// and failed construction. Live configuration is read from the component,
    /// not from a second mutable configuration registry.
    pub fn specification(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        self.validate_configuration(&configuration)?;
        let configuration = configuration
            .as_object()
            .expect("validated object")
            .iter()
            .map(|(name, value)| {
                (
                    Arc::from(name.as_str()),
                    ConfigurationValue::Literal(value.clone()),
                )
            })
            .collect();
        Ok(ComponentSpecification {
            descriptor: self.metadata.descriptor(id)?,
            role: self.metadata.role,
            completion: self.metadata.completion,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
            configuration,
            dependencies: BTreeMap::new(),
        })
    }

    fn construct(
        &self,
        request: CreateRequest,
        admission: Option<Arc<SourceAdmission>>,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound(request, admission, progress, false, None, None, None, None)
    }
    #[allow(clippy::too_many_arguments)]
    fn construct_bound(
        &self,
        request: CreateRequest,
        admission: Option<Arc<SourceAdmission>>,
        progress: Option<Arc<QuerySourceProgress>>,
        consumer: bool,
        query_snapshots: Option<Arc<super::NativeQuerySnapshotsResource>>,
        consumer_progress: Option<Arc<ConsumerProgressSet>>,
        input_progress: Option<Arc<QuerySourceProgress>>,
        source_bootstrap: Option<Arc<super::NativeSourceBootstrapResource>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct_bound_with_identity(
            request,
            admission,
            progress,
            consumer,
            query_snapshots,
            consumer_progress,
            input_progress,
            source_bootstrap,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn construct_bound_with_identity(
        &self,
        mut request: CreateRequest,
        admission: Option<Arc<SourceAdmission>>,
        progress: Option<Arc<QuerySourceProgress>>,
        consumer: bool,
        query_snapshots: Option<Arc<super::NativeQuerySnapshotsResource>>,
        consumer_progress: Option<Arc<ConsumerProgressSet>>,
        input_progress: Option<Arc<QuerySourceProgress>>,
        source_bootstrap: Option<Arc<super::NativeSourceBootstrapResource>>,
        identity: Option<Arc<super::NativeIdentityResource>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        request.activation_credentials_bound = identity.is_some();
        if let Some(owner) = &input_progress {
            anyhow::ensure!(
                self.supports_input_progress(),
                "native input progress requires a non-atomic transformer or Handled sink"
            );
            anyhow::ensure!(
                owner.component_id() == &request.id
                    && request
                        .scope
                        .as_ref()
                        .is_some_and(|scope| scope.graph_id == owner.graph_id()),
                "native input progress belongs to a different graph or component"
            );
        }
        anyhow::ensure!(
            consumer == self.consumer.is_some(),
            "native consumer requires its host-owned delivery binding"
        );
        anyhow::ensure!(
            consumer_progress.is_some() == self.consumer_progress.is_some(),
            "native producer requires its negotiated consumer progress binding"
        );
        anyhow::ensure!(
            consumer_progress.is_none()
                || (admission.is_none()
                    && progress.is_none()
                    && !consumer
                    && query_snapshots.is_none()),
            "native consumer progress cannot be combined with other service bindings"
        );
        let consumer_progress = consumer_progress
            .map(|owner| -> anyhow::Result<_> {
                anyhow::ensure!(
                    owner.producer_id() == &request.id
                        && request
                            .scope
                            .as_ref()
                            .is_some_and(|scope| scope.graph_id == owner.graph_id()),
                    "native consumer progress belongs to a different graph or producer"
                );
                Ok(BoundConsumerProgress {
                    binding: super::consumer_progress::ConsumerProgressBinding::new(&owner)?,
                    owner,
                    extension: self.consumer_progress.expect("validated"),
                    retention: None,
                    position_policy: None,
                })
            })
            .transpose()?;
        self.validate_configuration(&request.configuration)?;
        let activation_identity = identity
            .map(|resource| -> anyhow::Result<_> {
                let extension = self.activation_credentials.ok_or_else(|| {
                    anyhow::anyhow!("native factory has no activation-credentials extension")
                })?;
                let request_bytes = wire::encode(&request)?;
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(extension
                        .context
                        .expect("validated")(
                        self.raw.state,
                        abi::BorrowedBytes::new(&request_bytes),
                    ))?
                };
                let endpoint: drasi_computation_plugin_sdk::ActivationCredentialContext =
                    wire::decode(&response)?;
                endpoint.validate()?;
                Ok((resource, endpoint))
            })
            .transpose()?;
        let bytes = if activation_identity.is_some() {
            drasi_computation_plugin_sdk::activation_credentials::encode_bound_create_request(
                &request,
            )?
        } else {
            wire::encode(&request)?
        };
        let table = unsafe { &*self.raw.vtable };
        let mut component = abi::ComponentHandle::null();
        let query_snapshots = query_snapshots
            .map(|resource| -> anyhow::Result<_> {
                anyhow::ensure!(
                    self.query_snapshots.is_some()
                        && !consumer
                        && admission.is_none()
                        && progress.is_none(),
                    "native query snapshots require their negotiated read service"
                );
                anyhow::ensure!(
                    request
                        .scope
                        .as_ref()
                        .is_some_and(|scope| scope.graph_id == resource.graph_id()),
                    "query-snapshot resource belongs to another graph"
                );
                Ok(super::query_snapshots::QuerySnapshotsBinding::new(resource))
            })
            .transpose()?;
        let binding = admission.as_ref().map(|admission| {
            super::admission::AdmissionBinding::new(admission, self.codec.clone())
        });
        anyhow::ensure!(
            admission.is_none() || progress.is_none(),
            "native source cannot bind admission and a separate replay owner"
        );
        let progress = progress
            .map(|owner| -> anyhow::Result<_> {
                Ok(BoundProgress {
                    binding: super::progress::ProgressBinding::new(&owner),
                    owner,
                    extension: self.recovery.ok_or_else(|| {
                        anyhow::anyhow!("native factory has no source progress service")
                    })?,
                    retention: None,
                })
            })
            .transpose()?;
        unsafe {
            if let Some(coordinator) = &source_bootstrap {
                anyhow::ensure!(
                    admission.is_none() && !consumer && query_snapshots.is_none(),
                    "source bootstrap cannot be combined with unrelated service bindings"
                );
                let extension = self.bootstrap_coordination.ok_or_else(|| {
                    anyhow::anyhow!("source bootstrap coordinator was not negotiated")
                })?;
                coordinator.validate_factory(self.raw.state, &self._plugin)?;
                coordinator.validate_identity(
                    activation_identity.as_ref().map(|(resource, _)| resource),
                )?;
                let single = progress.as_ref().map(|progress| progress.binding.table());
                let shared = consumer_progress
                    .as_ref()
                    .map(|progress| progress.binding.table());
                coordinator.with_source(
                    &request,
                    progress.as_ref().map(|progress| &progress.owner),
                    consumer_progress.as_ref().map(|progress| &progress.owner),
                    |coordinator| {
                        take_status(extension.create_source.expect("validated")(
                            self.raw.state,
                            abi::BorrowedBytes::new(&bytes),
                            coordinator,
                            single.as_ref().map_or(std::ptr::null(), |table| table),
                            shared.as_ref().map_or(std::ptr::null(), |table| table),
                            &mut component,
                        ))?;
                        Ok(())
                    },
                )?;
            } else if let Some(progress) = &consumer_progress {
                take_status(progress.extension.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &progress.binding.table(),
                    &mut component,
                ))?;
            } else if let Some(binding) = &query_snapshots {
                take_status(self
                    .query_snapshots
                    .expect("validated")
                    .create
                    .expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &binding.table(),
                    &mut component,
                ))?;
            } else if consumer {
                let (_, extension) = self.consumer.expect("validated");
                take_status(extension.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &mut component,
                ))?;
            } else if let Some(progress) = &progress {
                take_status(progress.extension.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &progress.binding.table(),
                    &mut component,
                ))?;
            } else if let Some(binding) = &binding {
                let services = self.admission.ok_or_else(|| {
                    anyhow::anyhow!("native factory has no source admission service")
                })?;
                take_status(services.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &binding.table(),
                    &mut component,
                ))?;
            } else {
                take_status(table.create.expect("validated")(
                    self.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                    &mut component,
                ))?;
            }
        };
        let component = unsafe {
            NativeComponentProxy::new(
                component,
                self.metadata.clone(),
                &request.id,
                self.codec.clone(),
                self.schemas.clone(),
                self._plugin.clone(),
                SourceBindings {
                    admission: admission.zip(binding),
                    progress,
                    consumer_progress,
                    input_progress,
                },
            )?
        };
        let component = match self.batching {
            Some(extension) => component.with_batching(extension),
            None => Ok(component),
        }?;
        component
            .with_identity(activation_identity)
            .with_query_snapshots(query_snapshots)
            .with_source_schema(self.source_schema)
    }
}
impl Drop for NativeFactory {
    fn drop(&mut self) {
        unsafe { (*self.raw.vtable).release.expect("validated")(self.raw.state) };
    }
}

#[async_trait]
impl ComponentFactory for NativeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        self.specification(id, configuration)
    }
    fn record_schemas(&self) -> Vec<Arc<Schema>> {
        self.schemas.clone()
    }
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.implementation == self.metadata.implementation
                && specification.configuration_version == self.metadata.configuration_version
                && specification.role == self.metadata.role
                && specification.completion == self.metadata.completion,
            "native factory implementation, version, role or completion mismatch"
        );
        anyhow::ensure!(
            specification.dependencies.iter().all(|(name, resources)| {
                resources.len() <= 1
                    && match name.as_ref() {
                        "admission" => self.admission.is_some(),
                        "source_progress" => self.recovery.is_some(),
                        "source_bootstrap" => self.bootstrap_coordination.is_some(),
                        "input_progress" => self.supports_input_progress(),
                        "consumer_progress" => {
                            self.consumer_progress.is_some() && resources.len() == 1
                        }
                        "consumer" => self.consumer.is_some() && resources.len() == 1,
                        "query_snapshots" => self.query_snapshots.is_some(),
                        "identity" => {
                            self.activation_credentials.is_some() && self.consumer.is_none()
                        }
                        _ => false,
                    }
            }),
            "native resource binding requires its negotiated service interface"
        );
        anyhow::ensure!(
            self.consumer.is_none()
                || specification
                    .dependencies
                    .get("consumer")
                    .is_some_and(|ids| ids.len() == 1),
            "native consumer requires one host-owned delivery resource"
        );
        if specification
            .dependencies
            .get("source_bootstrap")
            .is_some_and(|ids| !ids.is_empty())
        {
            let slot = if self.consumer_progress.is_some() {
                "consumer_progress"
            } else {
                "source_progress"
            };
            anyhow::ensure!(
                specification
                    .dependencies
                    .get(slot)
                    .is_some_and(|ids| ids.len() == 1)
                    && specification
                        .dependencies
                        .get("admission")
                        .is_none_or(Vec::is_empty),
                "source bootstrap requires exactly its live progress dependency"
            );
        }
        anyhow::ensure!(
            self.consumer_progress.is_none()
                || specification
                    .dependencies
                    .get("consumer_progress")
                    .is_some_and(|ids| ids.len() == 1),
            "native producer requires one consumer progress resource"
        );
        anyhow::ensure!(
            self.consumer_progress.is_none()
                || ["admission", "source_progress"].into_iter().all(|name|
                    specification.dependencies.get(name).is_none_or(Vec::is_empty)),
            "native consumer progress cannot be combined with source admission or single-owner progress"
        );
        anyhow::ensure!(
            !["admission", "source_progress"].into_iter().all(|name| {
                specification
                    .dependencies
                    .get(name)
                    .is_some_and(|resources| !resources.is_empty())
            }),
            "native source cannot bind admission and a separate replay owner"
        );
        anyhow::ensure!(
            specification.descriptor
                == self
                    .metadata
                    .descriptor(specification.descriptor.id().clone())?,
            "native specification descriptor mismatch"
        );
        if specification
            .configuration
            .values()
            .all(|value| matches!(value, ConfigurationValue::Literal(_)))
        {
            let configuration = serde_json::Value::Object(
                specification
                    .configuration
                    .iter()
                    .map(|(name, value)| {
                        let ConfigurationValue::Literal(value) = value else {
                            unreachable!()
                        };
                        (name.to_string(), value.clone())
                    })
                    .collect(),
            );
            self.validate_configuration(&configuration)?;
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.validate(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        if self.consumer.is_some() {
            let resource = context
                .resources::<super::NativeConsumerResource>("consumer")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "missing native consumer delivery resource"
                    ))
                })?;
            let configuration = serde_json::Value::Object(
                context
                    .configuration()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            );
            let proxy = self
                .create_consumer(
                    context.component_id,
                    configuration,
                    Scope {
                        instance_id: context.instance_id.to_string(),
                        graph_id: context.graph_id.to_string(),
                        generation: context.generation.0,
                    },
                    resource.provider.clone(),
                    resource.options.clone(),
                )
                .await
                .map_err(ComponentCreationError::terminal)?;
            return Ok(ConstructedComponent::sink(Box::new(proxy)));
        }
        let progress = if context
            .specification
            .dependencies
            .contains_key("source_progress")
        {
            let resources = context
                .resources::<QuerySourceProgressResource>("source_progress")
                .map_err(ComponentCreationError::terminal)?;
            let owner = resources
                .into_iter()
                .next()
                .map(|resource| resource.0.clone());
            if owner.as_ref().is_some_and(|owner| {
                owner.graph_id() != context.graph_id.as_ref()
                    || owner.component_id() == &context.component_id
            }) {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "native progress belongs to another graph or to the source itself"
                )));
            }
            owner
        } else {
            None
        };
        let input_progress = if context
            .specification
            .dependencies
            .contains_key("input_progress")
        {
            context
                .resources::<QuerySourceProgressResource>("input_progress")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
                .map(|resource| resource.0.clone())
        } else {
            None
        };
        let admission = if context.specification.dependencies.contains_key("admission") {
            let channels = context
                .resources::<QosChannel>("admission")
                .map_err(ComponentCreationError::terminal)?;
            if let Some(channel) = channels.into_iter().next() {
                let admission = SourceAdmission::new(channel, self.metadata.ports[0].id().clone())
                    .await
                    .map_err(ComponentCreationError::terminal)?;
                if admission.component_id() != &context.component_id
                    || admission.identity().graph_id() != context.graph_id.as_ref()
                    || admission.identity().construction_scope() != context.instance_id.as_ref()
                {
                    return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                        "native admission binding belongs to another component or construction scope")));
                }
                Some(admission)
            } else {
                None
            }
        } else {
            None
        };
        let configuration = serde_json::Value::Object(
            context
                .configuration()
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        );
        let snapshots = if context
            .specification
            .dependencies
            .contains_key("query_snapshots")
        {
            context
                .resources::<super::NativeQuerySnapshotsResource>("query_snapshots")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
        } else {
            None
        };
        let consumer_progress = if self.consumer_progress.is_some() {
            Some(
                context
                    .resources::<ConsumerProgressSet>("consumer_progress")
                    .map_err(ComponentCreationError::terminal)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        ComponentCreationError::terminal(anyhow::anyhow!(
                            "missing native consumer progress resource"
                        ))
                    })?,
            )
        } else {
            None
        };
        let source_bootstrap = if context
            .specification
            .dependencies
            .contains_key("source_bootstrap")
        {
            context
                .resources::<super::NativeSourceBootstrapResource>("source_bootstrap")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
        } else {
            None
        };
        let identity = if context.specification.dependencies.contains_key("identity") {
            context
                .resources::<super::NativeIdentityResource>("identity")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
        } else {
            None
        };
        let proxy = self
            .construct_bound_with_identity(
                CreateRequest {
                    activation_credentials_bound: false,
                    id: context.component_id,
                    implementation: context.specification.implementation.clone(),
                    configuration_version: context.specification.configuration_version,
                    configuration,
                    scope: Some(Scope {
                        instance_id: context.instance_id.to_string(),
                        graph_id: context.graph_id.to_string(),
                        generation: context.generation.0,
                    }),
                },
                admission,
                progress,
                false,
                snapshots,
                consumer_progress,
                input_progress,
                source_bootstrap,
                identity,
            )
            .map_err(|error| {
                if error
                    .downcast_ref::<super::NativeFailure>()
                    .is_some_and(|failure| {
                        super::proxy::native_failure_disposition(failure)
                            == FailureDisposition::Retryable
                    })
                {
                    ComponentCreationError::retryable(error)
                } else {
                    ComponentCreationError::terminal(error)
                }
            })?;
        Ok(match self.metadata.role {
            ComponentRole::Source => ConstructedComponent::source(Box::new(proxy)),
            ComponentRole::Transformer => ConstructedComponent::transformer(Box::new(proxy)),
            ComponentRole::Sink => ConstructedComponent::sink(Box::new(proxy)),
            ComponentRole::Service => ConstructedComponent::service(Box::new(proxy)),
            ComponentRole::Query => {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "native dynamic query snapshot/outbox is unsupported"
                )))
            }
        })
    }
}

impl TransactionalTransformerFactory for NativeFactory {
    fn implementation(&self) -> ImplementationIdentity {
        self.metadata.implementation.clone()
    }
    fn configuration_version(&self) -> u32 {
        self.metadata.configuration_version
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> anyhow::Result<Box<dyn TransactionalTransformer>> {
        anyhow::ensure!(
            self.metadata.capabilities.transactional,
            "native factory did not opt into transaction participation"
        );
        anyhow::ensure!(
            definition.implementation == self.metadata.implementation
                && definition.configuration_version == self.metadata.configuration_version,
            "native transaction implementation/configuration version mismatch"
        );
        Ok(Box::new(
            self.create_component(definition.id.clone(), definition.configuration.clone())?
                .into_transactional()?,
        ))
    }
}
