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
                            ConfigType::ComponentId => ConfigurationType::ComponentId,
                            ConfigType::ComponentName => ConfigurationType::ComponentName,
                            ConfigType::Object => ConfigurationType::Object,
                            ConfigType::Array => ConfigurationType::Array,
                            ConfigType::Json => ConfigurationType::Json,
                        },
                        required: field.required,
                        secret: field.secret,
                        constraint: field.constraint.clone(),
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
    pub(super) query_authoring: std::sync::Mutex<Option<std::sync::Weak<dyn ComponentFactory>>>,
    metadata: FactoryMetadata,
    configuration_details: Option<drasi_computation_plugin_sdk::ConfigDetails>,
    descriptor: FactoryDescriptor,
    port_formats: Option<PortFormatCatalogue>,
    raw: abi::FactoryHandle,
    _plugin: Arc<PluginOwner>,
    codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    extensions: FactoryExtensions,
}
unsafe impl Send for NativeFactory {}
unsafe impl Sync for NativeFactory {}

/// The validated extension tables a factory's declared services use. Each is
/// present exactly when the factory declares the service (the loader requires
/// the table at load).
#[derive(Clone, Copy, Default)]
pub(super) struct FactoryExtensions {
    pub(super) recovery: Option<abi::recovery::PluginRecoveryV1>,
    pub(super) consumer_progress: Option<abi::consumer_progress::PluginConsumerProgressV1>,
    pub(super) consumer: Option<abi::consumer::PluginConsumerV1>,
    pub(super) batching: Option<abi::batching::PluginBatchingV1>,
    pub(super) coordination: Option<abi::bootstrap_coordination::PluginBootstrapCoordinationV1>,
    pub(super) activation_credentials:
        Option<abi::activation_credentials::PluginActivationCredentialsV1>,
    pub(super) source_schema: Option<abi::source_schema::PluginSourceSchemaV1>,
    pub(super) output_reads: Option<abi::output_reads::PluginOutputReadsV2>,
    pub(super) output_follow: Option<abi::output_follow::PluginOutputFollowV2>,
    pub(super) management: Option<abi::management::PluginManagementV1>,
}

#[derive(Default)]
pub(super) struct ProcessingCreation {
    pub(super) owner: Option<Arc<QuerySourceProgress>>,
    pub(super) replacement: bool,
    pub(super) update_directive: Option<wire::UpdateDirective>,
}

/// Every Host service bound to one construction, delivered to the plugin
/// together through its single create entry. Bind any subset the factory
/// declares (`Capabilities::services`); required services (consumer progress,
/// indexed processing, consumer delivery) must be present.
#[derive(Default)]
pub struct NativeServices {
    pub admission: Option<Arc<SourceAdmission>>,
    pub source_progress: Option<Arc<QuerySourceProgress>>,
    pub consumer_progress: Option<Arc<ConsumerProgressSet>>,
    pub output_snapshots: Option<super::OutputSnapshotsService>,
    /// Graph-owned volatile handled-input tracking; never passed to the plugin.
    pub input_progress: Option<Arc<QuerySourceProgress>>,
    pub identity: Option<Arc<super::NativeIdentityResource>>,
    pub processing: Option<Arc<super::NativeProcessingResource>>,
    pub source_bootstrap: Option<Arc<super::NativeSourceBootstrapResource>>,
}

/// The bound services of one construction after the Host prepared them.
#[derive(Default)]
struct Bindings {
    admission: Option<Arc<SourceAdmission>>,
    source_progress: Option<Arc<QuerySourceProgress>>,
    consumer_progress: Option<Arc<ConsumerProgressSet>>,
    output_snapshots: Option<super::OutputSnapshotsService>,
    input_progress: Option<Arc<QuerySourceProgress>>,
    identity: Option<Arc<super::NativeIdentityResource>>,
    processing: Option<super::processing::ProcessingBinding>,
    source_bootstrap: Option<(
        Arc<super::NativeSourceBootstrapResource>,
        bool,
        Option<drasi_lib::computation::v1::OutputCatalogue>,
    )>,
    consumer: bool,
}

impl NativeFactory {
    pub(super) unsafe fn new(
        metadata: FactoryMetadata,
        raw: abi::FactoryHandle,
        plugin: Arc<PluginOwner>,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        extensions: FactoryExtensions,
    ) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null()
                && table.create.is_some()
                && table.assess.is_some()
                && table.release.is_some(),
            "incomplete native factory table"
        );
        let services = &metadata.capabilities.services;
        anyhow::ensure!(
            extensions.recovery.is_some() == services.source_progress
                && extensions.consumer_progress.is_some() == services.consumer_progress
                && extensions.consumer.is_some() == services.consumer.is_some()
                && extensions.batching.is_some() == services.sink_batching
                && extensions.coordination.is_some() == services.bootstrap_coordination
                && extensions.activation_credentials.is_some() == services.activation_credentials,
            "native factory services do not match their extension tables"
        );
        // Every resource dependency comes from the factory's declaration.
        let mut descriptor = runtime_descriptor(&metadata);
        let optional = |mut requirement: ResourceRequirement| {
            requirement.minimum = 0;
            requirement
        };
        if services.source_admission {
            descriptor.dependencies.insert(
                Arc::from("admission"),
                optional(ResourceRequirement::exactly_one::<QosChannel>(
                    ResourceRole::StateStore,
                )),
            );
        }
        if services.source_progress {
            descriptor.dependencies.insert(
                Arc::from("source_progress"),
                optional(ResourceRequirement::exactly_one::<
                    QuerySourceProgressResource,
                >(ResourceRole::Checkpoint)),
            );
        }
        if services.consumer.is_none()
            && metadata.capabilities.committed_input_progress.is_none()
            && (metadata.role == ComponentRole::Transformer
                || (metadata.role == ComponentRole::Sink
                    && metadata.completion == Some(SinkCompletion::Handled)))
        {
            descriptor.dependencies.insert(
                "input_progress".into(),
                optional(ResourceRequirement::exactly_one::<
                    QuerySourceProgressResource,
                >(ResourceRole::Checkpoint)),
            );
        }
        if services.activation_credentials {
            descriptor.dependencies.insert(
                "identity".into(),
                optional(ResourceRequirement::exactly_one::<
                    super::NativeIdentityResource,
                >(ResourceRole::Identity)),
            );
        }
        if services.consumer.is_some() {
            descriptor.dependencies.insert(
                "consumer".into(),
                ResourceRequirement::exactly_one::<super::NativeConsumerResource>(
                    ResourceRole::IndexBackend,
                ),
            );
        }
        if services.consumer_progress {
            descriptor.dependencies.insert(
                "consumer_progress".into(),
                ResourceRequirement::exactly_one::<ConsumerProgressSet>(ResourceRole::Checkpoint),
            );
        }
        if services.output_snapshot_reads {
            descriptor.dependencies.insert(
                "output_snapshots".into(),
                optional(ResourceRequirement::exactly_one::<
                    super::NativeOutputSnapshotsResource,
                >(ResourceRole::OutputCatalogue)),
            );
        }
        if metadata.capabilities.indexed_processing {
            descriptor.dependencies.insert(
                "processing".into(),
                ResourceRequirement::exactly_one::<super::NativeProcessingResource>(
                    ResourceRole::StateStore,
                ),
            );
        }
        let port_formats = metadata
            .supports_port_format_overrides()
            .then(|| metadata.port_catalogue())
            .transpose()?;
        Ok(Self {
            query_authoring: std::sync::Mutex::new(None),
            descriptor,
            port_formats,
            metadata,
            configuration_details: None,
            raw,
            _plugin: plugin,
            codec,
            schemas,
            extensions,
        })
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }

    /// Ask the actual plugin factory to assess a change without constructing or
    /// mutating an instance. A plan does not authorize discarding retained work.
    pub fn plan_update_request(
        &self,
        request: &drasi_computation_plugin_sdk::wire::UpdateRequest,
    ) -> anyhow::Result<drasi_computation_contracts::ComponentUpdatePlan> {
        anyhow::ensure!(
            !self.metadata.capabilities.update_modes.is_empty(),
            "native factory does not support managed updates"
        );
        anyhow::ensure!(
            request.current.id == request.proposed.id,
            "an update cannot change the component UUID"
        );
        anyhow::ensure!(
            request.proposed.implementation == self.metadata.implementation
                && request.proposed.configuration_version == self.metadata.configuration_version,
            "native update targets a different factory or configuration version"
        );
        self.validate_configuration(&request.proposed.configuration)?;
        let payload = serde_json::to_vec(request)?;
        anyhow::ensure!(
            payload.len() <= abi::MAX_MESSAGE_BYTES,
            "native update request exceeds size limit"
        );
        let table = unsafe { checked_table(self.raw.vtable)? };
        let response = unsafe {
            drasi_computation_plugin_sdk::transport::take_reply(table
                .assess
                .expect("validated factory assessment")(
                self.raw.state,
                abi::BorrowedBytes::new(&payload),
            ))?
        };
        anyhow::ensure!(
            response.len() <= abi::MAX_MESSAGE_BYTES,
            "native update plan exceeds size limit"
        );
        let assessment: Assessment = serde_json::from_slice(&response)?;
        let plan = match assessment {
            Assessment::Unsupported { reason } => {
                anyhow::bail!("native factory reports the update as unsupported: {reason}")
            }
            supported => supported.plan().expect("supported assessment"),
        };
        anyhow::ensure!(
            self.metadata.capabilities.update_modes.contains(&plan.mode),
            "native factory returned an undeclared update mode"
        );
        Ok(plan)
    }
    pub fn supports_activation_credentials(&self) -> bool {
        self.extensions.activation_credentials.is_some()
    }

    fn request(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Option<Scope>,
        inputs: Vec<InputBinding>,
    ) -> CreateRequest {
        CreateRequest {
            inputs,
            id,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
            configuration,
            scope,
            activation_credentials_bound: false,
            update_directive: None,
        }
    }

    pub fn create_component_with_identity(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        identity: Arc<super::NativeIdentityResource>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_identity_and_inputs(id, configuration, identity, Vec::new())
    }

    /// Like [`Self::create_component_with_identity`], for a component that binds
    /// its declared incoming edges (`inputs`).
    pub fn create_component_with_identity_and_inputs(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        identity: Arc<super::NativeIdentityResource>,
        inputs: Vec<InputBinding>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(
            self.request(id, configuration, None, inputs),
            Bindings {
                identity: Some(identity),
                ..Default::default()
            },
        )
    }
    pub fn supports_source_bootstrap(&self) -> bool {
        self.extensions.coordination.is_some()
    }
    pub fn source_bootstrap_uses_consumer_progress(&self) -> bool {
        self.metadata.capabilities.services.consumer_progress
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
            identity.is_none() || self.extensions.activation_credentials.is_some(),
            "source bootstrap identity requires activation credentials"
        );
        let extension = self
            .extensions
            .coordination
            .ok_or_else(|| anyhow::anyhow!("native factory has no source bootstrap coordinator"))?;
        anyhow::ensure!(
            matches!(&progress, super::NativeSourceBootstrapProgress::Shared(_))
                == self.metadata.capabilities.services.consumer_progress,
            "source bootstrap progress mode does not match its source factory"
        );
        self.validate_configuration(&configuration)?;
        let mut request = self.request(id, configuration, Some(scope), Vec::new());
        request.activation_credentials_bound = identity.is_some();
        super::NativeSourceBootstrapResource::create(
            request,
            progress,
            extension,
            self.raw.state,
            self._plugin.clone(),
            identity,
        )
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
    pub fn consumer_mode(&self) -> Option<drasi_computation_plugin_sdk::ConsumerMode> {
        self.metadata.capabilities.services.consumer
    }
    /// The creation request a graph construction context describes.
    fn context_request(
        &self,
        context: &ConstructionContext,
        update_directive: Option<drasi_computation_plugin_sdk::UpdateDirective>,
    ) -> CreateRequest {
        CreateRequest {
            id: context.component_id.clone(),
            implementation: context.specification.implementation.clone(),
            configuration_version: context.specification.configuration_version,
            configuration: serde_json::Value::Object(
                context
                    .configuration()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            ),
            inputs: if self.metadata.capabilities.binds_inputs {
                context.input_bindings().to_vec()
            } else {
                Vec::new()
            },
            activation_credentials_bound: false,
            update_directive,
            scope: Some(Scope {
                instance_id: context.instance_id.to_string(),
                graph_id: context.graph_id.to_string(),
                generation: context.generation.0,
            }),
        }
    }
    /// Ask the plugin's pure describer for the instance `request` constructs.
    pub(super) fn describe_request(
        &self,
        request: &CreateRequest,
        descriptor: &ComponentDescriptor,
        dependencies: drasi_lib::computation::v1::DependencyCapabilities,
    ) -> anyhow::Result<drasi_lib::computation::v1::ComponentDescription> {
        let payload = serde_json::to_vec(&drasi_computation_plugin_sdk::wire::DescribeRequest {
            request: request.clone(),
            descriptor: descriptor.clone(),
            dependencies,
        })?;
        anyhow::ensure!(
            payload.len() <= abi::MAX_METADATA_BYTES,
            "native describe request exceeds size limit"
        );
        let table = unsafe { checked_table(self.raw.vtable)? };
        let response = unsafe {
            drasi_computation_plugin_sdk::transport::take_reply(table
                .describe
                .expect("validated factory describer")(
                self.raw.state,
                abi::BorrowedBytes::new(&payload),
            ))?
        };
        anyhow::ensure!(
            response.len() <= abi::MAX_METADATA_BYTES,
            "native description exceeds size limit"
        );
        let description: drasi_lib::computation::v1::ComponentDescription =
            serde_json::from_slice(&response)?;
        let description = self.with_bootstrap_facts(description, descriptor)?;
        description
            .validate(descriptor)
            .map_err(|reason| anyhow::anyhow!("invalid native description: {reason}"))?;
        anyhow::ensure!(
            self.consumer_mode().is_none() || description.has_keyed_inputs(),
            "native consumer {} must declare the delivery guarantee of its identity-keyed inputs",
            self.metadata.implementation.name
        );
        Ok(description)
    }
    /// The Host owns the bootstrap ports it adds to every instance, so it
    /// declares their facts: a source relays its consumers' sessions to the
    /// provider on its bootstrap input (whose namespace it is) and completes
    /// them as that provider does; a transformer accepts the initialization
    /// its capability names and serves committed output snapshots. Plugins
    /// declare no bootstrap port facts, and only a source offers coordination.
    fn with_bootstrap_facts(
        &self,
        description: drasi_lib::computation::v1::ComponentDescription,
        descriptor: &ComponentDescriptor,
    ) -> anyhow::Result<drasi_lib::computation::v1::ComponentDescription> {
        use drasi_computation_contracts::{BootstrapInputDescription, BootstrapOutputDescription};
        use drasi_lib::computation::v1::PortDescription;
        anyhow::ensure!(
            !description.ports.values().any(|port| matches!(
                port,
                PortDescription::BootstrapInput(_) | PortDescription::BootstrapOutput(_)
            )),
            "native plugins do not declare bootstrap port facts; the host owns those ports"
        );
        anyhow::ensure!(
            (description.coordination.is_none() && description.element_identity.is_none())
                || self.metadata.role == ComponentRole::Source,
            "only a native source declares bootstrap coordination or element identity"
        );
        anyhow::ensure!(
            description.coordination.is_none() || self.supports_source_bootstrap(),
            "native source declares bootstrap coordination without a coordinator"
        );
        let capabilities = &self.metadata.capabilities;
        let input = match self.metadata.role {
            ComponentRole::Source => BootstrapInputDescription::SOURCE_RELAY,
            _ if capabilities.quiescent_bootstrap.is_some() => BootstrapInputDescription::QUIESCENT,
            _ => BootstrapInputDescription::DURABLE,
        };
        let output = match self.metadata.role {
            ComponentRole::Source => BootstrapOutputDescription::RELAYED,
            _ => BootstrapOutputDescription::OUTPUT_SNAPSHOT,
        };
        Ok(description.with_bootstrap_ports(descriptor, input, output))
    }
    fn supports_input_progress(&self) -> bool {
        self.descriptor.dependencies.contains_key("input_progress")
    }
    pub async fn create_consumer(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        options: DeliveryOptions,
    ) -> anyhow::Result<super::NativeConsumerProxy> {
        self.create_consumer_with_inputs(id, configuration, scope, provider, options, Vec::new())
            .await
    }

    /// Like [`Self::create_consumer`], for a consumer that binds its declared
    /// incoming edges (`inputs`).
    pub async fn create_consumer_with_inputs(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        options: DeliveryOptions,
        inputs: Vec<InputBinding>,
    ) -> anyhow::Result<super::NativeConsumerProxy> {
        self.create_consumer_request(
            self.request(id, configuration, Some(scope), inputs),
            Bindings::default(),
            provider,
            options,
        )
        .await
    }

    /// A Host-owned consumer, constructed with every other service bound to it.
    async fn create_consumer_request(
        &self,
        request: CreateRequest,
        mut bindings: Bindings,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        options: DeliveryOptions,
    ) -> anyhow::Result<super::NativeConsumerProxy> {
        let scope = request
            .scope
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native consumer requires its construction scope"))?;
        let mode = self
            .consumer_mode()
            .ok_or_else(|| anyhow::anyhow!("native factory has no consumer service"))?;
        let extension = self
            .extensions
            .consumer
            .expect("declared consumer extension");
        bindings.consumer = true;
        let component = self.construct(request, bindings)?;
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
            "native consumer changed its declared mode"
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
            self.request(id, configuration, None, Vec::new()),
            Bindings::default(),
        )
    }

    /// Construct with any combination of the Host services this factory
    /// declares, delivered to the plugin together.
    pub fn create_component_with_services(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        services: NativeServices,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_services_and_inputs(
            id,
            configuration,
            scope,
            services,
            Vec::new(),
        )
    }

    /// Like [`Self::create_component_with_services`], for a component that
    /// binds its declared incoming edges (`inputs`).
    pub fn create_component_with_services_and_inputs(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        services: NativeServices,
        inputs: Vec<InputBinding>,
    ) -> anyhow::Result<NativeComponentProxy> {
        let processing = services
            .processing
            .map(|resource| {
                self.processing_binding(&id, &scope, &resource, ProcessingCreation::default())
            })
            .transpose()?;
        self.construct(
            self.request(id, configuration, Some(scope), inputs),
            Bindings {
                admission: services.admission,
                source_progress: services.source_progress,
                consumer_progress: services.consumer_progress,
                output_snapshots: services.output_snapshots,
                input_progress: services.input_progress,
                identity: services.identity,
                processing,
                source_bootstrap: services
                    .source_bootstrap
                    .map(|coordinator| (coordinator, false, None)),
                consumer: false,
            },
        )
    }

    /// Construct a transformer with the actual graph-owned index provider.
    /// Index resources are acquired on start and retained through cleanup.
    pub fn create_component_with_processing(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_processing_owner(id, configuration, scope, provider, None)
    }

    pub fn create_component_with_processing_owner(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        owner: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_processing_publication(
            id,
            configuration,
            scope,
            provider,
            owner,
            drasi_lib::computation::v1::QueryPublicationMode::Atomic,
        )
    }

    pub fn create_component_with_processing_publication(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        owner: Option<Arc<QuerySourceProgress>>,
        publication: drasi_lib::computation::v1::QueryPublicationMode,
    ) -> anyhow::Result<NativeComponentProxy> {
        let resource = super::NativeProcessingResource::new(provider).with_publication(publication);
        self.create_processing_component(
            id,
            configuration,
            scope,
            &resource,
            ProcessingCreation {
                owner,
                ..Default::default()
            },
        )
    }

    /// Construct with the same processing resource used by graph factory wiring,
    /// including an explicitly supplied scheduled-processing physical clock.
    pub fn create_component_with_processing_resource(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        resource: &super::NativeProcessingResource,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_processing_component(
            id,
            configuration,
            scope,
            resource,
            ProcessingCreation::default(),
        )
    }

    fn processing_binding(
        &self,
        id: &ComponentId,
        scope: &Scope,
        resource: &super::NativeProcessingResource,
        creation: ProcessingCreation,
    ) -> anyhow::Result<super::processing::ProcessingBinding> {
        super::processing::ProcessingBinding::new(
            resource.provider().clone(),
            scope.graph_id.clone(),
            id.clone(),
        )
        .with_volatile_processing(self.metadata.capabilities.volatile_processing)
        .with_replacement(creation.replacement)
        .with_publication(
            resource.publication(),
            self.metadata.capabilities.non_atomic_publication,
        )?
        .with_committed_input_progress_owner(
            self.metadata.capabilities.committed_input_progress.clone(),
            creation.owner,
        )?
        .with_durable_output(
            self.metadata.capabilities.durable_output,
            self.codec.clone(),
        )?
        .with_scheduling(
            self.metadata.capabilities.scheduled_processing,
            resource.scheduling_clock(),
        )
    }

    pub(super) fn create_processing_component(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        resource: &super::NativeProcessingResource,
        mut creation: ProcessingCreation,
    ) -> anyhow::Result<NativeComponentProxy> {
        let directive = creation.update_directive.take();
        let processing = self.processing_binding(&id, &scope, resource, creation)?;
        let mut request = self.request(id, configuration, Some(scope), Vec::new());
        request.update_directive = directive;
        self.construct(
            request,
            Bindings {
                processing: Some(processing),
                ..Default::default()
            },
        )
    }

    pub fn create_component_with_bootstrap(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        provider: Arc<super::NativeBootstrapResource>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(
            self.request(id, configuration, Some(scope), Vec::new()),
            Bindings::default(),
        )?
        .with_bootstrap(provider)
    }

    /// Construct a component that binds its incoming edges, as the graph does
    /// from its declared topology. `inputs` are the actual producer outputs and
    /// streams feeding the component; a factory that does not bind inputs
    /// rejects them.
    pub fn create_component_with_inputs(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        inputs: Vec<InputBinding>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.construct(
            self.request(id, configuration, None, inputs),
            Bindings::default(),
        )
    }

    pub fn create_component_with_output_snapshots(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        snapshots: Arc<super::NativeOutputSnapshotsResource>,
        catalogue: OutputCatalogue,
        inputs: Vec<InputBinding>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_services_and_inputs(
            id,
            configuration,
            scope,
            NativeServices {
                output_snapshots: Some(super::OutputSnapshotsService {
                    resource: snapshots,
                    catalogue,
                }),
                ..Default::default()
            },
            inputs,
        )
    }

    pub fn create_component_with_consumer_progress(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        progress: Arc<ConsumerProgressSet>,
    ) -> anyhow::Result<NativeComponentProxy> {
        self.create_component_with_services(
            id,
            configuration,
            scope,
            NativeServices {
                consumer_progress: Some(progress),
                ..Default::default()
            },
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
        let (source_progress, consumer_progress) = match progress {
            super::NativeSourceBootstrapProgress::Single(owner) => (Some(owner), None),
            super::NativeSourceBootstrapProgress::Shared(owners) => (None, Some(owners)),
        };
        self.create_component_with_services(
            id,
            configuration,
            scope,
            NativeServices {
                source_progress,
                consumer_progress,
                source_bootstrap: Some(coordinator),
                ..Default::default()
            },
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
        self.create_component_with_services(
            id,
            configuration,
            scope,
            NativeServices {
                input_progress: Some(input_progress),
                consumer_progress,
                ..Default::default()
            },
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
        let descriptor = self.metadata.descriptor_for(id, &configuration)?;
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
            descriptor,
            role: self.metadata.role,
            completion: self.metadata.completion,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
            configuration,
            dependencies: BTreeMap::new(),
        })
    }

    /// The single construction path: every bound service goes to the plugin
    /// together through its one create entry. Only undeclared and missing
    /// required services are refused; declared services combine freely
    /// (contradictory declarations were refused when the plugin loaded).
    fn construct(
        &self,
        mut request: CreateRequest,
        bindings: Bindings,
    ) -> anyhow::Result<NativeComponentProxy> {
        let Bindings {
            admission,
            source_progress,
            consumer_progress,
            output_snapshots,
            input_progress,
            identity,
            processing,
            source_bootstrap,
            consumer,
        } = bindings;
        let services = &self.metadata.capabilities.services;
        // The declared capabilities of the bindings this construction holds,
        // for the plugin's describer: an admission channel's producer
        // identity and a processing provider's durability.
        let mut dependencies = drasi_lib::computation::v1::DependencyCapabilities::new();
        if let Some(admission) = &admission {
            dependencies.insert(
                "admission".into(),
                vec![if admission.identity().persistent() {
                    drasi_lib::computation::v1::ResourceCapabilities::DURABLE
                } else {
                    drasi_lib::computation::v1::ResourceCapabilities::VOLATILE
                }],
            );
        }
        if let Some(processing) = &processing {
            dependencies.insert("processing".into(), vec![processing.capabilities()]);
        }
        anyhow::ensure!(
            request.inputs.is_empty() || self.metadata.capabilities.binds_inputs,
            "native factory does not bind inputs"
        );
        for (name, bound, declared) in [
            (
                "source admission",
                admission.is_some(),
                services.source_admission,
            ),
            (
                "source progress",
                source_progress.is_some(),
                services.source_progress,
            ),
            (
                "output snapshot reads",
                output_snapshots.is_some(),
                services.output_snapshot_reads,
            ),
            (
                "activation credentials",
                identity.is_some(),
                services.activation_credentials,
            ),
            (
                "source bootstrap coordination",
                source_bootstrap.is_some(),
                services.bootstrap_coordination,
            ),
            (
                "input progress",
                input_progress.is_some(),
                self.supports_input_progress(),
            ),
        ] {
            anyhow::ensure!(!bound || declared, "native factory does not declare {name}");
        }
        for (name, bound, required) in [
            ("consumer delivery", consumer, services.consumer.is_some()),
            (
                "consumer progress",
                consumer_progress.is_some(),
                services.consumer_progress,
            ),
            (
                "indexed processing",
                processing.is_some(),
                self.metadata.capabilities.indexed_processing,
            ),
        ] {
            anyhow::ensure!(
                bound == required,
                "native factory requires exactly its declared {name} binding"
            );
        }
        if let Some(owner) = &input_progress {
            anyhow::ensure!(
                owner.component_id() == &request.id
                    && request
                        .scope
                        .as_ref()
                        .is_some_and(|scope| scope.graph_id == owner.graph_id()),
                "native input progress belongs to a different graph or component"
            );
        }
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
                    extension: self.extensions.consumer_progress.expect("declared"),
                    retention: None,
                    position_policy: None,
                })
            })
            .transpose()?;
        self.validate_configuration(&request.configuration)?;
        let activation_identity = identity
            .map(|resource| -> anyhow::Result<_> {
                let extension = self.extensions.activation_credentials.expect("declared");
                request.activation_credentials_bound = true;
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
        request.activation_credentials_bound = activation_identity.is_some();
        let bytes = wire::encode_create_request(&request)?;
        let table = unsafe { &*self.raw.vtable };
        let mut component = abi::ComponentHandle::null();
        let output_snapshots = output_snapshots.map(|service| {
            super::output_snapshots::OutputSnapshotsBinding::new(
                service.resource,
                service.catalogue,
            )
        });
        let admission_binding = admission.as_ref().map(|admission| {
            super::admission::AdmissionBinding::new(admission, self.codec.clone())
        });
        let progress = source_progress.map(|owner| BoundProgress {
            binding: super::progress::ProgressBinding::new(&owner),
            owner,
            extension: self.extensions.recovery.expect("declared"),
            retention: None,
        });
        // Borrowed for the create call; the plugin retains what it keeps.
        let admission_table = admission_binding.as_ref().map(|binding| binding.table());
        let progress_table = progress.as_ref().map(|progress| progress.binding.table());
        let consumer_progress_table = consumer_progress
            .as_ref()
            .map(|progress| progress.binding.table());
        let snapshots_table = output_snapshots.as_ref().map(|binding| binding.table());
        let create =
            |processing: *const abi::processing::ProcessingV1,
             coordinator: *const abi::bootstrap_coordination::BootstrapCoordinatorHandle,
             component: &mut abi::ComponentHandle|
             -> anyhow::Result<()> {
                fn pointer<T>(table: &Option<T>) -> *const T {
                    table.as_ref().map_or(std::ptr::null(), |table| table)
                }
                let services = abi::services::ServiceBindingsV1 {
                    admission: pointer(&admission_table),
                    source_progress: pointer(&progress_table),
                    consumer_progress: pointer(&consumer_progress_table),
                    output_snapshots: pointer(&snapshots_table),
                    processing,
                    coordinator,
                    consumer: u32::from(consumer),
                    ..abi::services::ServiceBindingsV1::none()
                };
                unsafe {
                    take_status(table.create.expect("validated")(
                        self.raw.state,
                        abi::BorrowedBytes::new(&bytes),
                        &services,
                        component,
                    ))?;
                }
                Ok(())
            };
        let create_with_processing =
            |coordinator: *const abi::bootstrap_coordination::BootstrapCoordinatorHandle,
             component: &mut abi::ComponentHandle|
             -> anyhow::Result<()> {
                match &processing {
                    Some(processing) => {
                        processing.with_table(|table| create(table, coordinator, component))
                    }
                    None => create(std::ptr::null(), coordinator, component),
                }
            };
        match &source_bootstrap {
            Some((coordinator, _, _)) => {
                coordinator.validate_factory(self.raw.state, &self._plugin)?;
                coordinator.validate_identity(
                    activation_identity.as_ref().map(|(resource, _)| resource),
                )?;
                coordinator.with_source(
                    &request,
                    progress.as_ref().map(|progress| &progress.owner),
                    consumer_progress.as_ref().map(|progress| &progress.owner),
                    |coordinator| create_with_processing(coordinator, &mut component),
                )?;
            }
            None => create_with_processing(std::ptr::null(), &mut component)?,
        }
        let component = unsafe {
            NativeComponentProxy::new(
                component,
                self.metadata.for_configuration(&request.configuration)?,
                &request,
                self.codec.clone(),
                self.schemas.clone(),
                self._plugin.clone(),
                SourceBindings {
                    admission: admission.zip(admission_binding),
                    progress,
                    consumer_progress,
                    input_progress,
                },
            )?
        };
        let component = match self.extensions.batching {
            Some(extension) => component.with_batching(extension),
            None => Ok(component),
        }?;
        let source_coordinator = source_bootstrap
            .map(|(coordinator, owned, graph)| {
                super::proxy::SourceCoordinator::new(coordinator, owned, graph)
            })
            .transpose()?;
        let component = component
            .with_identity(activation_identity)
            .with_source_coordinator(source_coordinator)
            .with_processing(processing)
            .with_output_snapshots(output_snapshots)
            .with_source_schema(self.extensions.source_schema)?
            .with_output_reads(self.extensions.output_reads, self.extensions.output_follow)?
            .with_management(self.extensions.management)?;
        let description = self.describe_request(&request, component.descriptor(), dependencies)?;
        Ok(component.with_description(description))
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
    /// The plugin's own declaration, from its pure describer, for the
    /// resolved configuration and the declared capabilities of the bound
    /// resources. The Host interprets none of the configuration.
    fn describe(
        &self,
        context: &ConstructionContext,
    ) -> anyhow::Result<drasi_lib::computation::v1::ComponentDescription> {
        let request = self.context_request(context, None);
        self.describe_request(
            &request,
            &context.specification.descriptor,
            context.dependency_capabilities(),
        )
    }
    fn port_formats(&self) -> Option<&PortFormatCatalogue> {
        self.port_formats.as_ref()
    }
    fn primary_ports(&self) -> PortPrimaries {
        self.metadata.capabilities.primary_ports.clone()
    }
    fn binds_inputs(&self) -> bool {
        self.metadata.capabilities.binds_inputs
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
    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        self.validate(proposed)?;
        anyhow::ensure!(
            !self.metadata.capabilities.binds_inputs,
            "native update planning requires the factory's actual input bindings"
        );
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.role == proposed.role
                && current.completion == proposed.completion
                && current.dependencies == proposed.dependencies,
            "native configuration updates cannot change ports, role or resource bindings"
        );
        let request = |specification: &ComponentSpecification| -> anyhow::Result<CreateRequest> {
            Ok(CreateRequest {
                id: specification.descriptor.id().clone(),
                implementation: specification.implementation.clone(),
                configuration_version: specification.configuration_version,
                configuration: literal_configuration(specification)?,
                scope: None,
                inputs: Vec::new(),
                activation_credentials_bound: false,
                update_directive: None,
            })
        };
        self.plan_update_request(&wire::UpdateRequest {
            current: request(current)?,
            proposed: request(proposed)?,
        })
    }
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.implementation == self.metadata.implementation
                && specification.configuration_version == self.metadata.configuration_version
                && specification.role == self.metadata.role
                && specification.completion == self.metadata.completion,
            "native factory implementation, version, role or completion mismatch"
        );
        if specification.dependencies.contains_key("catalog") {
            if self.supports_ordinary_queries() {
                anyhow::bail!(
                    "native query implementation '{}' does not bind a query result catalog; \
                     declare the query with implementation '{}' to use a 'catalog' dependency",
                    self.metadata.implementation.name,
                    super::query::continuous_query_implementation(&self.metadata.implementation)?
                        .name
                );
            }
            anyhow::bail!(
                "native implementation '{}' does not accept a query result catalog",
                self.metadata.implementation.name
            );
        }
        // Every binding must name a declared service; required services must
        // be bound. Declared services combine freely: contradictory
        // declarations were refused when the plugin loaded.
        let services = &self.metadata.capabilities.services;
        for (name, resources) in &specification.dependencies {
            anyhow::ensure!(
                resources.len() <= 1
                    && match name.as_ref() {
                        "admission" => services.source_admission,
                        "source_progress" => services.source_progress,
                        "input_progress" => self.supports_input_progress(),
                        "consumer_progress" => services.consumer_progress,
                        "consumer" => services.consumer.is_some(),
                        "output_snapshots" => services.output_snapshot_reads,
                        "identity" => services.activation_credentials,
                        "processing" => self.metadata.capabilities.indexed_processing,
                        _ => false,
                    },
                "native resource binding '{name}' names a service the factory does not declare"
            );
        }
        for (name, required) in [
            ("consumer", services.consumer.is_some()),
            ("processing", self.metadata.capabilities.indexed_processing),
            ("consumer_progress", services.consumer_progress),
        ] {
            anyhow::ensure!(
                !required
                    || specification
                        .dependencies
                        .get(name)
                        .is_some_and(|ids| ids.len() == 1),
                "native factory requires one '{name}' resource"
            );
        }
        let port_overrides = match specification.configuration.get(PORT_FORMATS_KEY) {
            Some(ConfigurationValue::Literal(value)) => Some(value),
            Some(_) => anyhow::bail!("port format overrides must be literal configuration"),
            None => None,
        };
        anyhow::ensure!(
            specification.descriptor
                == self
                    .metadata
                    .for_port_overrides(port_overrides)?
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
        // The instance carries the declaration the graph checks: the plugin's
        // answer for this context's declared resource capabilities.
        let description =
            ComponentFactory::describe(self, &context).map_err(ComponentCreationError::terminal)?;
        let directive = context.update_directive().map(update_directive);
        let bound = |name: &str| context.specification.dependencies.contains_key(name);
        let source_progress = if bound("source_progress") {
            let owner = context
                .resources::<QuerySourceProgressResource>("source_progress")
                .map_err(ComponentCreationError::terminal)?
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
        let input_progress = if bound("input_progress") {
            context
                .resources::<QuerySourceProgressResource>("input_progress")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
                .map(|resource| resource.0.clone())
        } else {
            None
        };
        let admission = if bound("admission") {
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
        let output_snapshots = if bound("output_snapshots") {
            context
                .resources::<super::NativeOutputSnapshotsResource>("output_snapshots")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
                .map(|resource| super::OutputSnapshotsService {
                    resource,
                    catalogue: context.output_catalogue().clone(),
                })
        } else {
            None
        };
        let consumer_progress = if self.metadata.capabilities.services.consumer_progress {
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
        let identity = if bound("identity") {
            context
                .resources::<super::NativeIdentityResource>("identity")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
        } else {
            None
        };
        let processing = if self.metadata.capabilities.indexed_processing {
            let resource = context
                .resources::<super::NativeProcessingResource>("processing")
                .map_err(ComponentCreationError::terminal)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "missing native indexed processing resource"
                    ))
                })?;
            Some(
                self.processing_binding(
                    &context.component_id,
                    &Scope {
                        instance_id: context.instance_id.to_string(),
                        graph_id: context.graph_id.to_string(),
                        generation: context.generation.0,
                    },
                    &resource,
                    ProcessingCreation {
                        replacement: context.is_replacement()
                            && context.update_directive().is_none(),
                        ..Default::default()
                    },
                )
                .map_err(ComponentCreationError::terminal)?,
            )
        } else {
            None
        };
        let scope = Scope {
            instance_id: context.instance_id.to_string(),
            graph_id: context.graph_id.to_string(),
            generation: context.generation.0,
        };
        // A coordination-capable source with a provider on its bootstrap input
        // coordinates its consumers' bootstrap itself: it creates its
        // library's coordinator from its own progress owners, so the paired
        // provider node shares the live engine.
        let running = context.output_catalogue().clone();
        // Only a source that declares coordination creates its coordinator;
        // the graph refuses any provider on its bootstrap input that is not
        // paired through the same protocol when coordination is required.
        let source_bootstrap =
            if description.coordination.is_some() && !context.bootstrap_producers().is_empty() {
                let owners = match (&source_progress, &consumer_progress) {
                    (Some(owner), None) => {
                        Some(super::NativeSourceBootstrapProgress::Single(owner.clone()))
                    }
                    (None, Some(owners)) => {
                        Some(super::NativeSourceBootstrapProgress::Shared(owners.clone()))
                    }
                    _ => None,
                };
                owners
                    .map(|owners| {
                        let coordinator = self.create_source_bootstrap_with_identity(
                            context.component_id.clone(),
                            configuration.clone(),
                            scope.clone(),
                            owners,
                            identity.clone(),
                        )?;
                        let declared = description
                            .coordination
                            .as_ref()
                            .map(|coordination| &coordination.protocol);
                        anyhow::ensure!(
                            declared == Some(coordinator.protocol()),
                            "native source coordinator implements {} but the source declares {:?}",
                            coordinator.protocol(),
                            declared
                        );
                        Ok(coordinator)
                    })
                    .transpose()
                    .map_err(ComponentCreationError::terminal)?
            } else {
                None
            };
        let mut request = self.request(
            context.component_id.clone(),
            configuration,
            Some(scope),
            if self.metadata.capabilities.binds_inputs {
                context.input_bindings().to_vec()
            } else {
                Vec::new()
            },
        );
        request.update_directive = directive;
        let bindings = Bindings {
            admission,
            source_progress,
            consumer_progress,
            output_snapshots,
            input_progress,
            identity,
            processing,
            source_bootstrap: source_bootstrap
                .map(|coordinator| (coordinator, true, Some(running))),
            consumer: false,
        };
        if self.consumer_mode().is_some() {
            let resource = context
                .resources::<super::NativeConsumerResource>("consumer")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "missing native consumer delivery resource"
                    ))
                })?;
            let proxy = self
                .create_consumer_request(
                    request,
                    bindings,
                    resource.provider.clone(),
                    resource.options.clone(),
                )
                .await
                .map_err(ComponentCreationError::terminal)?;
            return Ok(ConstructedComponent::sink(Box::new(
                proxy
                    .with_description(description)
                    .map_err(ComponentCreationError::terminal)?,
            )));
        }
        let proxy = self.construct(request, bindings).map_err(|error| {
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
        let mut proxy = proxy;
        if self
            .metadata
            .capabilities
            .committed_input_progress
            .is_some()
        {
            proxy
                .prepare_processing()
                .await
                .map_err(ComponentCreationError::terminal)?;
        }
        let proxy = proxy.with_description(description);
        Ok(match self.metadata.role {
            ComponentRole::Source => ConstructedComponent::source(Box::new(proxy)),
            ComponentRole::Transformer => ConstructedComponent::transformer(Box::new(proxy)),
            ComponentRole::Sink => ConstructedComponent::sink(Box::new(proxy)),
            ComponentRole::Service => ConstructedComponent::service(Box::new(proxy)),
            ComponentRole::BootstrapProvider => {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "native bootstrap provider components are not supported by the plugin ABI"
                )))
            }
        })
    }
}

pub(super) fn literal_configuration(
    specification: &ComponentSpecification,
) -> anyhow::Result<serde_json::Value> {
    specification
        .configuration
        .iter()
        .map(|(name, value)| {
            let ConfigurationValue::Literal(value) = value else {
                anyhow::bail!(
                    "native update assessment requires literal configuration; unresolved references cannot be assessed"
                );
            };
            Ok((name.to_string(), value.clone()))
        })
        .collect::<anyhow::Result<serde_json::Map<_, _>>>()
        .map(serde_json::Value::Object)
}

pub(super) fn update_directive(directive: &ComponentUpdateDirective) -> wire::UpdateDirective {
    wire::UpdateDirective {
        operation_id: directive.operation_id.to_string(),
        plan: directive.plan,
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
