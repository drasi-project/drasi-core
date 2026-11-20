// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::{PluginMetadata, TARGET},
    transport::{borrowed_bytes, checked_table, take_status},
    wire,
};
use drasi_lib::computation::v1::{
    FactoryRegistry, RecordId, RecordImage, RecordValidationError, RecordValidator, Schema,
    SchemaDescriptor, TransactionalTransformerFactory,
};
use libloading::Library;
use std::{path::Path, sync::Arc};

use super::{NativeBootstrapFactory, NativeFactory};

pub struct NativePlugin {
    metadata: PluginMetadata,
    factories: Vec<Arc<NativeFactory>>,
    bootstrap_factories: Vec<Arc<NativeBootstrapFactory>>,
}
impl NativePlugin {
    /// Link an in-process SDK plugin through the same validated catalogue and
    /// operation tables used for a separately loaded native library.
    pub fn from_definition(
        definition: drasi_computation_plugin_sdk::PluginDefinition,
    ) -> anyhow::Result<Arc<Self>> {
        let exported = drasi_computation_plugin_sdk::ExportedPlugin::new(definition)?;
        // Exported handles retain the definition; metadata is copied before the
        // temporary export backing is released. Statically linked code cannot unload.
        unsafe {
            Self::from_exports(
                || exported.metadata(),
                |out| exported.entry(out),
                Extensions {
                    recovery: Some(exported.recovery()),
                    bootstrap: Some(exported.bootstrap()),
                    consumer: Some(exported.consumer()),
                    configuration: Some(exported.configuration()),
                    batching: Some(exported.batching()),
                    consumer_progress: Some(exported.consumer_progress()),
                    source_schema: Some(exported.source_schema()),
                    coordination: Some(exported.bootstrap_coordination()),
                    activation_credentials: Some(exported.activation_credentials()),
                    output_reads: Some(exported.output_reads()),
                    output_follow: Some(exported.output_follow()),
                    management: Some(exported.management()),
                },
            )
        }
    }

    pub fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }
    pub fn factories(&self) -> &[Arc<NativeFactory>] {
        &self.factories
    }
    pub fn bootstrap_factories(&self) -> &[Arc<NativeBootstrapFactory>] {
        &self.bootstrap_factories
    }

    /// Register native factories and their supported query-authoring recipe identities.
    /// This does not select an instance's default query factory. Registration is
    /// atomic: failures leave the registry unchanged.
    pub fn register_factories(&self, registry: &mut FactoryRegistry) -> anyhow::Result<()> {
        let mut next = registry.clone();
        for factory in &self.factories {
            next.register(factory.clone())?;
            if factory.supports_ordinary_queries() {
                next.register(factory.for_ordinary_queries()?)?;
            }
        }
        for factory in &self.bootstrap_factories {
            next.register(Arc::new(super::NativeBootstrapNodeFactory::new(
                factory.clone(),
            )))?;
        }
        *registry = next;
        Ok(())
    }
    pub fn transactional_factories(&self) -> Vec<Arc<dyn TransactionalTransformerFactory>> {
        self.factories
            .iter()
            .filter(|factory| factory.metadata().capabilities.transactional)
            .map(|factory| factory.clone() as Arc<dyn TransactionalTransformerFactory>)
            .collect()
    }

    /// Host an explicitly supplied native ABI implementation (also useful to test
    /// foreign producers). Production discovery should use load/try_load.
    ///
    /// # Safety
    /// Both functions and every returned pointer must obey ABI 0.1. Their code and
    /// metadata must be pinned for process lifetime. This is not a plugin sandbox.
    pub unsafe fn from_entry_points(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe { Self::from_entry_points_with(metadata, entry, EntryPoints::default()) }
    }

    /// Host a native ABI implementation together with the optional extension
    /// entry points it exports.
    ///
    /// # Safety
    /// All entry points must belong to the same process-pinned binary and obey
    /// their versioned ownership and callback contracts.
    pub unsafe fn from_entry_points_with(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        extensions: EntryPoints,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe {
            Self::from_exports(
                || metadata(),
                |out| take_status(entry(out)),
                extensions.resolve(),
            )
        }
    }

    unsafe fn from_exports(
        metadata: impl FnOnce() -> *const abi::Metadata,
        entry: impl FnOnce(
            *mut abi::PluginHandle,
        ) -> Result<(), drasi_computation_plugin_sdk::transport::Failure>,
        extensions: Extensions,
    ) -> anyhow::Result<Arc<Self>> {
        let manifest = unsafe { read_metadata(metadata())? };
        let management = unsafe { read_management(extensions.management, &manifest)? };
        let output_reads = unsafe { read_output_reads(extensions.output_reads, &manifest)? };
        let output_follow = unsafe { read_output_follow(extensions.output_follow, &manifest)? };
        let mut configurations =
            unsafe { read_configuration(extensions.configuration, &manifest)? };
        let batching = unsafe { read_batching(extensions.batching, &manifest)? };
        let consumer_progress =
            unsafe { read_consumer_progress(extensions.consumer_progress, &manifest)? };
        let source_schema = unsafe { read_source_schema(extensions.source_schema)? };
        let coordination =
            unsafe { read_bootstrap_coordination(extensions.coordination, &manifest)? };
        let activation_credentials =
            unsafe { read_activation_credentials(extensions.activation_credentials, &manifest)? };
        let consumer = unsafe { read_consumer(extensions.consumer, &manifest)? };
        let bootstrap = unsafe { read_bootstrap(extensions.bootstrap)? };
        let recovery = unsafe { read_recovery(extensions.recovery, &manifest)? };
        let mut handle = abi::PluginHandle::null();
        entry(&mut handle)?;
        let plugin = Arc::new(unsafe { PluginOwner::new(handle)? });
        let schemas: Vec<_> = manifest
            .schemas
            .iter()
            .map(|descriptor| {
                Arc::new(Schema::new(
                    descriptor.clone(),
                    Arc::new(RemoteValidator {
                        plugin: plugin.clone(),
                    }),
                ))
            })
            .collect();
        let codec = Arc::new(wire::codec(&schemas)?);
        let mut factories = Vec::new();
        for (index, metadata) in manifest.factories.iter().enumerate() {
            let mut handle = abi::FactoryHandle::null();
            let index = u32::try_from(index).context("too many native factories")?;
            unsafe {
                take_status(plugin.table().factory.expect("validated")(
                    plugin.raw.state,
                    index,
                    &mut handle,
                ))?
            };
            // Every service the factory is constructed with comes from its
            // declaration; the extension tables were required at load.
            let services = &metadata.capabilities.services;
            factories.push(Arc::new(unsafe {
                NativeFactory::new(
                    metadata.clone(),
                    handle,
                    plugin.clone(),
                    codec.clone(),
                    schemas.clone(),
                    super::factory::FactoryExtensions {
                        recovery: recovery.filter(|_| services.source_progress),
                        consumer_progress: consumer_progress.filter(|_| services.consumer_progress),
                        consumer: consumer.filter(|_| services.consumer.is_some()),
                        batching: batching.filter(|_| services.sink_batching),
                        coordination: coordination.filter(|_| services.bootstrap_coordination),
                        activation_credentials: activation_credentials
                            .filter(|_| services.activation_credentials),
                        source_schema,
                        output_reads,
                        output_follow,
                        management,
                    },
                )?
                .with_configuration_details(configurations.remove(&metadata.implementation))
            }));
        }
        let mut bootstrap_factories = Vec::new();
        if let Some(extension) = bootstrap {
            use drasi_computation_plugin_sdk::bootstrap;
            let bytes = unsafe {
                drasi_computation_plugin_sdk::transport::take_reply(extension
                    .factories
                    .expect("validated")(
                    plugin.raw.state
                ))?
            };
            let metadata: Vec<bootstrap::BootstrapFactoryMetadata> = bootstrap::decode(&bytes)?;
            let mut identities = std::collections::BTreeSet::new();
            for (index, metadata) in metadata.into_iter().enumerate() {
                metadata.validate(&manifest.plugin)?;
                anyhow::ensure!(
                    identities.insert((
                        metadata.implementation.name.clone(),
                        metadata.implementation.version.clone()
                    )),
                    "duplicate native bootstrap factory"
                );
                let index = u32::try_from(index)?;
                let pairing = if let Some(extension) = coordination {
                    let response = unsafe {
                        drasi_computation_plugin_sdk::transport::take_reply(extension
                            .bootstrap_factory
                            .expect("validated")(
                            plugin.raw.state,
                            index,
                        ))?
                    };
                    wire::decode::<bool>(&response)?.then_some(extension)
                } else {
                    None
                };
                bootstrap_factories.push(Arc::new(
                    NativeBootstrapFactory::new(
                        metadata,
                        index,
                        extension,
                        plugin.clone(),
                        codec.clone(),
                    )
                    .with_coordination(pairing)?,
                ));
            }
        }
        anyhow::ensure!(
            !factories.is_empty() || !bootstrap_factories.is_empty(),
            "native plugin has no component or bootstrap factories"
        );
        Ok(Arc::new(Self {
            metadata: manifest,
            factories,
            bootstrap_factories,
        }))
    }
}

/// Optional extension entry points of an explicitly hosted native library.
#[derive(Default, Clone, Copy)]
pub struct EntryPoints {
    pub recovery: Option<abi::recovery::RecoveryFn>,
    pub bootstrap: Option<abi::bootstrap::BootstrapFn>,
    pub consumer: Option<abi::consumer::ConsumerFn>,
    pub configuration: Option<abi::configuration::ConfigurationFn>,
    pub batching: Option<abi::batching::BatchingFn>,
    pub consumer_progress: Option<abi::consumer_progress::ConsumerProgressFn>,
    pub source_schema: Option<abi::source_schema::SourceSchemaFn>,
    pub coordination: Option<abi::bootstrap_coordination::BootstrapCoordinationFn>,
    pub activation_credentials: Option<abi::activation_credentials::Entry>,
    pub output_reads: Option<abi::output_reads::OutputReadsFn>,
    pub output_follow: Option<abi::output_follow::OutputFollowFn>,
    pub management: Option<abi::management::ManagementFn>,
}

impl EntryPoints {
    unsafe fn resolve(self) -> Extensions {
        unsafe {
            Extensions {
                recovery: self.recovery.map(|entry| entry()),
                bootstrap: self.bootstrap.map(|entry| entry()),
                consumer: self.consumer.map(|entry| entry()),
                configuration: self.configuration.map(|entry| entry()),
                batching: self.batching.map(|entry| entry()),
                consumer_progress: self.consumer_progress.map(|entry| entry()),
                source_schema: self.source_schema.map(|entry| entry()),
                coordination: self.coordination.map(|entry| entry()),
                activation_credentials: self.activation_credentials.map(|entry| entry()),
                output_reads: self.output_reads.map(|entry| entry()),
                output_follow: self.output_follow.map(|entry| entry()),
                management: self.management.map(|entry| entry()),
            }
        }
    }
}

/// The extension tables a library exported, before validation.
#[derive(Default)]
struct Extensions {
    recovery: Option<*const abi::recovery::PluginRecoveryV1>,
    bootstrap: Option<*const abi::bootstrap::PluginBootstrapV1>,
    consumer: Option<*const abi::consumer::PluginConsumerV1>,
    configuration: Option<*const abi::configuration::ConfigurationMetadataV1>,
    batching: Option<*const abi::batching::PluginBatchingV1>,
    consumer_progress: Option<*const abi::consumer_progress::PluginConsumerProgressV1>,
    source_schema: Option<*const abi::source_schema::PluginSourceSchemaV1>,
    coordination: Option<*const abi::bootstrap_coordination::PluginBootstrapCoordinationV1>,
    activation_credentials:
        Option<*const abi::activation_credentials::PluginActivationCredentialsV1>,
    output_reads: Option<*const abi::output_reads::PluginOutputReadsV2>,
    output_follow: Option<*const abi::output_follow::PluginOutputFollowV2>,
    management: Option<*const abi::management::PluginManagementV1>,
}

/// Load a native library. Accepted and rejected native candidates are pinned once
/// identified; user code/OS loader initializers are trusted in-process code.
pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Arc<NativePlugin>> {
    try_load(path)?.ok_or_else(|| {
        anyhow::anyhow!("library does not export the native computation plugin family")
    })
}

/// None means neither native symbol exists. A partial/malformed native family
/// returns Err and must never be interpreted as a legacy plugin registration.
pub fn try_load(path: impl AsRef<Path>) -> anyhow::Result<Option<Arc<NativePlugin>>> {
    let library =
        Arc::new(unsafe { Library::new(path.as_ref()) }.with_context(|| {
            format!("load native plugin candidate {}", path.as_ref().display())
        })?);
    try_load_library(library)
}

pub(crate) fn try_load_library(library: Arc<Library>) -> anyhow::Result<Option<Arc<NativePlugin>>> {
    let activation_credentials = unsafe {
        library.get::<abi::activation_credentials::Entry>(abi::activation_credentials::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let metadata = unsafe { library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let entry = unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let recovery = unsafe { library.get::<abi::recovery::RecoveryFn>(abi::recovery::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let bootstrap = unsafe { library.get::<abi::bootstrap::BootstrapFn>(abi::bootstrap::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let consumer = unsafe { library.get::<abi::consumer::ConsumerFn>(abi::consumer::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let configuration =
        unsafe { library.get::<abi::configuration::ConfigurationFn>(abi::configuration::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let batching = unsafe { library.get::<abi::batching::BatchingFn>(abi::batching::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let consumer_progress = unsafe {
        library.get::<abi::consumer_progress::ConsumerProgressFn>(abi::consumer_progress::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let source_schema =
        unsafe { library.get::<abi::source_schema::SourceSchemaFn>(abi::source_schema::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let coordination = unsafe {
        library.get::<abi::bootstrap_coordination::BootstrapCoordinationFn>(
            abi::bootstrap_coordination::SYMBOL,
        )
    }
    .ok()
    .map(|symbol| *symbol);
    let output_reads =
        unsafe { library.get::<abi::output_reads::OutputReadsFn>(abi::output_reads::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let output_follow =
        unsafe { library.get::<abi::output_follow::OutputFollowFn>(abi::output_follow::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let management =
        unsafe { library.get::<abi::management::ManagementFn>(abi::management::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let logging = unsafe { library.get::<abi::logging::InstallFn>(abi::logging::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    if metadata.is_none()
        && entry.is_none()
        && recovery.is_none()
        && bootstrap.is_none()
        && consumer.is_none()
        && configuration.is_none()
        && batching.is_none()
        && consumer_progress.is_none()
        && source_schema.is_none()
        && coordination.is_none()
        && activation_credentials.is_none()
        && output_reads.is_none()
        && output_follow.is_none()
        && management.is_none()
    {
        return Ok(None);
    }

    // Retaining only successful handles is insufficient: metadata/entry code can
    // have retained callbacks or initialized I/O before reporting an error.
    std::mem::forget(library);
    // Before any other plugin code runs, so its diagnostics reach this host.
    if let Some(install) = logging {
        super::logging::install(install)?;
    }
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("native computation metadata symbol is missing"))?;
    let entry =
        entry.ok_or_else(|| anyhow::anyhow!("native computation entry symbol is missing"))?;
    Ok(Some(unsafe {
        NativePlugin::from_entry_points_with(
            metadata,
            entry,
            EntryPoints {
                recovery,
                bootstrap,
                consumer,
                configuration,
                batching,
                consumer_progress,
                source_schema,
                coordination,
                activation_credentials,
                output_reads,
                output_follow,
                management,
            },
        )?
    }))
}

/// Whether any factory of the manifest declares a Host service.
fn declares(
    manifest: &PluginMetadata,
    service: impl Fn(&drasi_computation_plugin_sdk::HostServices) -> bool,
) -> bool {
    manifest
        .factories
        .iter()
        .any(|factory| service(&factory.capabilities.services))
}

unsafe fn read_metadata(metadata: *const abi::Metadata) -> anyhow::Result<PluginMetadata> {
    let metadata = unsafe { checked_table(metadata)? };
    let target = std::str::from_utf8(unsafe { borrowed_bytes(metadata.target, 4096)? })?;
    anyhow::ensure!(
        target == TARGET,
        "native plugin target mismatch: expected {TARGET}, got {target}"
    );
    let manifest: PluginMetadata = serde_json::from_slice(unsafe {
        borrowed_bytes(metadata.manifest, abi::MAX_METADATA_BYTES)?
    })
    .context("invalid native computation plugin metadata")?;
    manifest.validate()?;
    Ok(manifest)
}

unsafe fn read_configuration(
    entry: Option<*const abi::configuration::ConfigurationMetadataV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<
    std::collections::BTreeMap<
        drasi_lib::computation::v1::ImplementationIdentity,
        drasi_computation_plugin_sdk::ConfigDetails,
    >,
> {
    let Some(entry) = entry else {
        return Ok(Default::default());
    };
    let metadata = unsafe { checked_table(entry)? };
    anyhow::ensure!(
        metadata.version == abi::configuration::VERSION && metadata.reserved == 0,
        "incompatible native configuration-description extension"
    );
    let details: drasi_computation_plugin_sdk::configuration::ConfigurationMetadata =
        serde_json::from_slice(unsafe {
            borrowed_bytes(metadata.manifest, abi::MAX_METADATA_BYTES)?
        })
        .context("invalid native configuration descriptions")?;
    details.validate(manifest)?;
    Ok(details
        .factories
        .into_iter()
        .map(|factory| (factory.implementation, factory.details))
        .collect())
}

unsafe fn read_bootstrap(
    entry: Option<*const abi::bootstrap::PluginBootstrapV1>,
) -> anyhow::Result<Option<abi::bootstrap::PluginBootstrapV1>> {
    entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::bootstrap::VERSION
                    && table.reserved == 0
                    && table.factories.is_some()
                    && table.describe.is_some()
                    && table.create.is_some(),
                "incompatible or incomplete native bootstrap extension"
            );
            Ok(table)
        })
        .transpose()
}

/// Required exactly when some factory declares Host-owned batching.
unsafe fn read_batching(
    entry: Option<*const abi::batching::PluginBatchingV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::batching::PluginBatchingV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::batching::VERSION
                    && table.reserved == 0
                    && table.policy.is_some(),
                "incompatible or incomplete native batching extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.sink_batching),
        "native factory declares Host-owned batching without its extension"
    );
    Ok(table)
}

/// Required exactly when some factory declares consumer progress.
unsafe fn read_consumer_progress(
    entry: Option<*const abi::consumer_progress::PluginConsumerProgressV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::consumer_progress::PluginConsumerProgressV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::consumer_progress::VERSION
                    && table.reserved == 0
                    && table.inspect.is_some()
                    && table.position_policy.is_some(),
                "incompatible or incomplete native consumer progress extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.consumer_progress),
        "native factory declares consumer progress without its extension"
    );
    Ok(table)
}

unsafe fn read_source_schema(
    entry: Option<*const abi::source_schema::PluginSourceSchemaV1>,
) -> anyhow::Result<Option<abi::source_schema::PluginSourceSchemaV1>> {
    entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::source_schema::VERSION
                    && table.reserved == 0
                    && table.supported.is_some()
                    && table.begin.is_some(),
                "incompatible or incomplete native source-schema extension"
            );
            Ok(table)
        })
        .transpose()
}

/// The extension is required exactly when some factory declares output reads.
unsafe fn read_output_reads(
    entry: Option<*const abi::output_reads::PluginOutputReadsV2>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::output_reads::PluginOutputReadsV2>> {
    let table = entry
        .map(|entry| -> anyhow::Result<_> {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::output_reads::VERSION
                    && table.reserved == 0
                    && table.open.is_some(),
                "incompatible or incomplete native output-reads extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some()
            || !manifest
                .factories
                .iter()
                .any(|factory| factory.capabilities.declares_output_reads()),
        "native factory declares output reads without the negotiated extension"
    );
    Ok(table)
}

/// The extension is required exactly when some factory declares output follows.
unsafe fn read_output_follow(
    entry: Option<*const abi::output_follow::PluginOutputFollowV2>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::output_follow::PluginOutputFollowV2>> {
    let table = entry
        .map(|entry| -> anyhow::Result<_> {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::output_follow::VERSION
                    && table.reserved == 0
                    && table.open.is_some(),
                "incompatible or incomplete native output-follow extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some()
            || !manifest
                .factories
                .iter()
                .any(|factory| factory.capabilities.declares_output_follows()),
        "native factory declares output follows without the negotiated extension"
    );
    Ok(table)
}

/// The extension is required exactly when some factory declares management.
unsafe fn read_management(
    entry: Option<*const abi::management::PluginManagementV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::management::PluginManagementV1>> {
    let table = entry
        .map(|entry| -> anyhow::Result<_> {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::management::VERSION
                    && table.reserved == 0
                    && table.assess.is_some()
                    && table.apply.is_some(),
                "incompatible or incomplete native management extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some()
            || !manifest
                .factories
                .iter()
                .any(|factory| factory.capabilities.management),
        "native factory declares management without the negotiated extension"
    );
    Ok(table)
}

/// Required exactly when some factory declares activation credentials.
unsafe fn read_activation_credentials(
    entry: Option<*const abi::activation_credentials::PluginActivationCredentialsV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::activation_credentials::PluginActivationCredentialsV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::activation_credentials::VERSION
                    && table.reserved == 0
                    && table.context.is_some(),
                "incompatible or incomplete native activation-credentials extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.activation_credentials),
        "native factory declares activation credentials without its extension"
    );
    Ok(table)
}

/// Required exactly when some factory declares bootstrap coordination.
unsafe fn read_bootstrap_coordination(
    entry: Option<*const abi::bootstrap_coordination::PluginBootstrapCoordinationV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::bootstrap_coordination::PluginBootstrapCoordinationV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::bootstrap_coordination::VERSION
                    && table.reserved == 0
                    && table.bootstrap_factory.is_some()
                    && table.create.is_some()
                    && table.create_bootstrap.is_some(),
                "incompatible or incomplete native bootstrap coordination extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.bootstrap_coordination),
        "native factory declares bootstrap coordination without its extension"
    );
    Ok(table)
}

/// Required exactly when some factory declares source progress.
unsafe fn read_recovery(
    entry: Option<*const abi::recovery::PluginRecoveryV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::recovery::PluginRecoveryV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::recovery::VERSION
                    && table.reserved == 0
                    && table.inspect.is_some(),
                "incompatible or incomplete native recovery extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.source_progress),
        "native factory declares source progress without its extension"
    );
    Ok(table)
}

/// Required exactly when some factory declares a Host-owned consumer.
unsafe fn read_consumer(
    entry: Option<*const abi::consumer::PluginConsumerV1>,
    manifest: &PluginMetadata,
) -> anyhow::Result<Option<abi::consumer::PluginConsumerV1>> {
    let table = entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::consumer::VERSION
                    && table.reserved == 0
                    && table.inspect.is_some()
                    && table.begin.is_some()
                    && table.end_batch.is_some(),
                "incompatible or incomplete native consumer extension"
            );
            Ok(table)
        })
        .transpose()?;
    anyhow::ensure!(
        table.is_some() || !declares(manifest, |services| services.consumer.is_some()),
        "native factory declares a Host-owned consumer without its extension"
    );
    Ok(table)
}

/// Inspect without calling the plugin entry or creating factory handles.
/// Extension factory catalogues are validated when the plugin is loaded.
pub(crate) fn try_read_metadata(library: Arc<Library>) -> anyhow::Result<Option<PluginMetadata>> {
    let activation_credentials = unsafe {
        library.get::<abi::activation_credentials::Entry>(abi::activation_credentials::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let metadata = unsafe { library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let has_entry = unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }.is_ok();
    let recovery = unsafe { library.get::<abi::recovery::RecoveryFn>(abi::recovery::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let bootstrap = unsafe { library.get::<abi::bootstrap::BootstrapFn>(abi::bootstrap::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let consumer = unsafe { library.get::<abi::consumer::ConsumerFn>(abi::consumer::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let configuration =
        unsafe { library.get::<abi::configuration::ConfigurationFn>(abi::configuration::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let batching = unsafe { library.get::<abi::batching::BatchingFn>(abi::batching::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let consumer_progress = unsafe {
        library.get::<abi::consumer_progress::ConsumerProgressFn>(abi::consumer_progress::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let source_schema =
        unsafe { library.get::<abi::source_schema::SourceSchemaFn>(abi::source_schema::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let coordination = unsafe {
        library.get::<abi::bootstrap_coordination::BootstrapCoordinationFn>(
            abi::bootstrap_coordination::SYMBOL,
        )
    }
    .ok()
    .map(|symbol| *symbol);
    let output_reads =
        unsafe { library.get::<abi::output_reads::OutputReadsFn>(abi::output_reads::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let output_follow =
        unsafe { library.get::<abi::output_follow::OutputFollowFn>(abi::output_follow::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let management =
        unsafe { library.get::<abi::management::ManagementFn>(abi::management::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    if metadata.is_none()
        && !has_entry
        && activation_credentials.is_none()
        && recovery.is_none()
        && bootstrap.is_none()
        && consumer.is_none()
        && configuration.is_none()
        && batching.is_none()
        && consumer_progress.is_none()
        && source_schema.is_none()
        && coordination.is_none()
        && output_reads.is_none()
        && output_follow.is_none()
        && management.is_none()
    {
        return Ok(None);
    }
    std::mem::forget(library);
    anyhow::ensure!(has_entry, "native computation entry symbol is missing");
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("native computation metadata symbol is missing"))?;
    let manifest = unsafe { read_metadata(metadata())? };
    unsafe { read_activation_credentials(activation_credentials.map(|entry| entry()), &manifest)? };
    unsafe { read_recovery(recovery.map(|entry| entry()), &manifest)? };
    unsafe { read_consumer(consumer.map(|entry| entry()), &manifest)? };
    unsafe { read_configuration(configuration.map(|entry| entry()), &manifest)? };
    unsafe { read_batching(batching.map(|entry| entry()), &manifest)? };
    unsafe { read_consumer_progress(consumer_progress.map(|entry| entry()), &manifest)? };
    unsafe { read_source_schema(source_schema.map(|entry| entry()))? };
    unsafe { read_bootstrap_coordination(coordination.map(|entry| entry()), &manifest)? };
    unsafe { read_output_reads(output_reads.map(|entry| entry()), &manifest)? };
    unsafe { read_output_follow(output_follow.map(|entry| entry()), &manifest)? };
    unsafe { read_management(management.map(|entry| entry()), &manifest)? };
    let bootstrap = unsafe { read_bootstrap(bootstrap.map(|entry| entry()))? };
    anyhow::ensure!(
        !manifest.factories.is_empty() || bootstrap.is_some(),
        "native plugin has no component factories or bootstrap extension"
    );
    Ok(Some(manifest))
}

pub(super) struct PluginOwner {
    raw: abi::PluginHandle,
}
// ABI factories/validators are thread-safe; handle release runs only after the
// last local owner. The producer alone dereferences its opaque state.
unsafe impl Send for PluginOwner {}
unsafe impl Sync for PluginOwner {}
impl PluginOwner {
    pub(super) fn state(&self) -> *mut std::ffi::c_void {
        self.raw.state
    }
    unsafe fn new(raw: abi::PluginHandle) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null()
                && table.release.is_some()
                && table.factory.is_some()
                && table.validate_record.is_some(),
            "incomplete native plugin table"
        );
        Ok(Self { raw })
    }
    fn table(&self) -> &abi::PluginVTable {
        unsafe { &*self.raw.vtable }
    }
}
impl Drop for PluginOwner {
    fn drop(&mut self) {
        unsafe { self.table().release.expect("validated")(self.raw.state) };
    }
}

struct RemoteValidator {
    plugin: Arc<PluginOwner>,
}
impl RemoteValidator {
    fn validate(&self, request: wire::RecordValidation<'_>) -> Result<(), RecordValidationError> {
        let execute = || -> anyhow::Result<()> {
            let bytes = wire::encode(&request)?;
            unsafe {
                take_status(self.plugin.table().validate_record.expect("validated")(
                    self.plugin.raw.state,
                    abi::BorrowedBytes::new(&bytes),
                ))?
            };
            Ok(())
        };
        execute().map_err(|error| RecordValidationError::new("native-schema", error.to_string()))
    }
}
impl RecordValidator for RemoteValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> Result<(), RecordValidationError> {
        self.validate(wire::RecordValidation {
            schema: schema.clone(),
            namespace: std::borrow::Cow::Borrowed(identity.namespace()),
            identity: std::borrow::Cow::Borrowed(identity.value()),
            image: None,
            payload: std::borrow::Cow::Borrowed(&[]),
        })
    }

    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> Result<(), RecordValidationError> {
        self.validate(wire::RecordValidation {
            schema: schema.clone(),
            namespace: std::borrow::Cow::Borrowed(identity.namespace()),
            identity: std::borrow::Cow::Borrowed(identity.value()),
            image: Some(match image {
                RecordImage::Full => 0,
                RecordImage::Patch => 1,
                RecordImage::Partial => 2,
            }),
            payload: std::borrow::Cow::Borrowed(payload),
        })
    }
}

#[cfg(test)]
mod activation_credentials_tests {
    use super::super::activation_credentials_tests::fixture;
    use super::*;
    use drasi_computation_plugin_sdk as sdk;

    #[test]
    fn activation_credentials_extension_rejects_malformed_tables_before_entry() -> anyhow::Result<()>
    {
        let export = sdk::ExportedPlugin::new(fixture::definition()?)?;
        let manifest = unsafe { read_metadata(export.metadata())? };
        let table = unsafe { *export.activation_credentials() };
        assert!(
            unsafe { read_activation_credentials(None, &manifest) }.is_err(),
            "a declared service requires its extension"
        );
        assert!(unsafe { read_activation_credentials(Some(std::ptr::null()), &manifest) }.is_err());
        let prefix = abi::Header::new::<abi::Header>();
        assert!(unsafe {
            read_activation_credentials(Some((&prefix as *const abi::Header).cast()), &manifest)
        }
        .is_err());
        assert!(unsafe {
            read_activation_credentials(
                Some(
                    (&table as *const abi::activation_credentials::PluginActivationCredentialsV1)
                        .cast::<u8>()
                        .wrapping_add(1)
                        .cast(),
                ),
                &manifest,
            )
        }
        .is_err());
        for variant in 0..5 {
            let invalid = (variant < 4).then(|| {
                let mut invalid = table;
                match variant {
                    0 => invalid.version += 1,
                    1 => invalid.reserved = 1,
                    2 => invalid.context = None,
                    _ => invalid.header.size -= 1,
                }
                invalid
            });
            let entered = std::cell::Cell::new(false);
            let result = unsafe {
                NativePlugin::from_exports(
                    || export.metadata(),
                    |out| {
                        entered.set(true);
                        export.entry(out)
                    },
                    Extensions {
                        activation_credentials: invalid.as_ref().map(|table| table as *const _),
                        ..Default::default()
                    },
                )
            };
            assert!(result.is_err(), "variant {variant}");
            assert!(!entered.get(), "malformed extension entered native code");
        }
        Ok(())
    }
}

#[cfg(test)]
mod bootstrap_coordination_tests {
    use super::*;

    fn fixture(name: &str) -> anyhow::Result<Arc<Library>> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"));
        let target = if target.is_absolute() {
            target
        } else {
            workspace.join(target)
        };
        let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| target.join("debug/plugins"));
        let path = directory.join("examples").join(format!(
            "{}{name}{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        ));
        anyhow::ensure!(
            path.is_file(),
            "first run make build-test-plugins (missing fixture {name})"
        );
        Ok(Arc::new(unsafe { Library::new(path)? }))
    }

    #[test]
    fn bootstrap_coordination_real_discovery_rejects_malformed_without_entry() -> anyhow::Result<()>
    {
        let library = fixture("native_bootstrap_coordination_metadata")?;
        let entered =
            unsafe { library.get::<unsafe extern "C" fn() -> u8>(b"drasi_fixture_entered\0")? };
        assert_eq!(unsafe { entered() }, 0);
        let error = try_read_metadata(library.clone()).expect_err("reject malformed metadata");
        assert!(error
            .to_string()
            .contains("bootstrap coordination extension"));
        assert!(try_load_library(library.clone())
            .err()
            .unwrap()
            .to_string()
            .contains("bootstrap coordination extension"));
        assert_eq!(unsafe { entered() }, 0);
        Ok(())
    }

    #[test]
    fn bootstrap_coordination_partial_family_never_falls_back() -> anyhow::Result<()> {
        let library = fixture("native_bootstrap_coordination_partial")?;
        assert!(try_read_metadata(library.clone()).is_err());
        assert!(try_load_library(library).is_err());
        Ok(())
    }

    #[test]
    fn bootstrap_coordination_foreign_tables_validate_before_entry() -> anyhow::Result<()> {
        let export = drasi_computation_plugin_sdk::ExportedPlugin::new(
            drasi_computation_plugin_sdk::PluginDefinition::new(
                "fixture/coordination-shape",
                "1",
                super::super::tests::consumer_progress_fixture::factories(),
                vec![drasi_lib::computation::v1::GraphChangeCodec::schema()],
            )?,
        )?;
        let manifest = unsafe { read_metadata(export.metadata())? };
        let table = unsafe { *export.bootstrap_coordination() };
        assert!(unsafe { read_bootstrap_coordination(None, &manifest)? }.is_none());
        assert!(unsafe { read_bootstrap_coordination(Some(std::ptr::null()), &manifest) }.is_err());
        assert!(unsafe {
            read_bootstrap_coordination(
                Some(
                    (&table as *const abi::bootstrap_coordination::PluginBootstrapCoordinationV1)
                        .cast::<u8>()
                        .wrapping_add(1)
                        .cast(),
                ),
                &manifest,
            )
        }
        .is_err());
        let prefix = abi::Header::new::<abi::Header>();
        assert!(unsafe {
            read_bootstrap_coordination(Some((&prefix as *const abi::Header).cast()), &manifest)
        }
        .is_err());
        assert!(unsafe { read_bootstrap_coordination(Some(&table), &manifest)? }.is_some());
        for field in 0..9 {
            let mut invalid = table;
            match field {
                0 => invalid.header.magic ^= 1,
                1 => invalid.header.major += 1,
                2 => invalid.header.minor += 1,
                3 => invalid.header.size += 1,
                4 => invalid.version += 1,
                5 => invalid.reserved = 1,
                6 => invalid.bootstrap_factory = None,
                7 => invalid.create = None,
                _ => invalid.create_bootstrap = None,
            }
            // Metadata-only and actual load deliberately share this validator.
            assert!(unsafe { read_bootstrap_coordination(Some(&invalid), &manifest) }.is_err());
            let entered = std::cell::Cell::new(false);
            let result = unsafe {
                NativePlugin::from_exports(
                    || export.metadata(),
                    |out| {
                        entered.set(true);
                        export.entry(out)
                    },
                    Extensions {
                        consumer_progress: Some(export.consumer_progress()),
                        coordination: Some(&invalid),
                        ..Default::default()
                    },
                )
            };
            assert!(result.is_err());
            assert!(
                !entered.get(),
                "invalid coordination extension entered the plugin"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod declared_extension_tests {
    use super::super::tests::consumer_progress_fixture;
    use super::*;

    /// A factory's declared services require their extension tables when the
    /// library loads; nothing is probed or discovered at construction.
    #[test]
    fn declared_services_require_their_extension_before_entry() -> anyhow::Result<()> {
        let export = drasi_computation_plugin_sdk::ExportedPlugin::new(
            drasi_computation_plugin_sdk::PluginDefinition::new(
                "fixture/declared-extension",
                "1",
                consumer_progress_fixture::factories(),
                vec![drasi_lib::computation::v1::GraphChangeCodec::schema()],
            )?,
        )?;
        let entered = std::cell::Cell::new(false);
        let error = unsafe {
            NativePlugin::from_exports(
                || export.metadata(),
                |out| {
                    entered.set(true);
                    export.entry(out)
                },
                Extensions::default(),
            )
        }
        .err()
        .expect("consumer progress is declared without its extension");
        assert!(
            error.to_string().contains("declares consumer progress"),
            "{error:#}"
        );
        assert!(!entered.get());
        let plugin = unsafe {
            NativePlugin::from_exports(
                || export.metadata(),
                |out| export.entry(out),
                Extensions {
                    consumer_progress: Some(export.consumer_progress()),
                    ..Default::default()
                },
            )?
        };
        assert!(!plugin.factories().is_empty());
        Ok(())
    }
}

#[cfg(test)]
mod consumer_progress_tests {
    use super::super::tests::consumer_progress_fixture;
    use super::*;

    #[test]
    fn consumer_progress_extension_shape_validates_every_required_field() -> anyhow::Result<()> {
        let export = drasi_computation_plugin_sdk::ExportedPlugin::new(
            drasi_computation_plugin_sdk::PluginDefinition::new(
                "fixture/consumer-progress-shape",
                "1",
                consumer_progress_fixture::factories(),
                vec![drasi_lib::computation::v1::GraphChangeCodec::schema()],
            )?,
        )?;
        let manifest = unsafe { read_metadata(export.metadata())? };
        let table = unsafe { *export.consumer_progress() };
        assert!(unsafe { read_consumer_progress(None, &manifest) }.is_err());
        assert!(unsafe { read_consumer_progress(Some(std::ptr::null()), &manifest) }.is_err());
        assert!(unsafe { read_consumer_progress(Some(&table), &manifest)? }.is_some());
        for field in 0..5 {
            let mut invalid = table;
            match field {
                0 => invalid.header.size += 1,
                1 => invalid.version += 1,
                2 => invalid.reserved = 1,
                3 => invalid.inspect = None,
                _ => invalid.position_policy = None,
            }
            assert!(unsafe { read_consumer_progress(Some(&invalid), &manifest) }.is_err());
        }
        Ok(())
    }
    #[test]
    fn consumer_progress_metadata_only_rejects_a_real_malformed_library_without_entry(
    ) -> anyhow::Result<()> {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"));
        let target = if target.is_absolute() {
            target
        } else {
            workspace.join(target)
        };
        let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| target.join("debug/plugins"));
        let path = directory.join("examples").join(format!(
            "{}native_consumer_progress_metadata{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        ));
        anyhow::ensure!(
            path.is_file(),
            "first run make build-test-plugins (missing native_consumer_progress_metadata fixture)"
        );
        let library = Arc::new(unsafe { Library::new(path)? });
        let entered =
            unsafe { library.get::<unsafe extern "C" fn() -> u8>(b"drasi_fixture_entered\0")? };
        assert_eq!(unsafe { entered() }, 0);
        let error = try_read_metadata(library.clone()).expect_err("metadata-only scan must reject");
        assert!(error.to_string().contains("consumer progress extension"));
        assert_eq!(unsafe { entered() }, 0);
        assert!(try_load_library(library.clone()).is_err());
        assert_eq!(unsafe { entered() }, 0);
        Ok(())
    }
}
