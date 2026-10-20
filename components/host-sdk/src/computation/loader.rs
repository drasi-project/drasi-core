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
                || Some(exported.services()),
                || Some(exported.recovery()),
                || Some(exported.bootstrap()),
                || Some(exported.consumer()),
                || Some(exported.configuration()),
                || Some(exported.batching()),
                || Some(exported.query_snapshots()),
                || Some(exported.consumer_progress()),
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

    /// Atomic with respect to duplicate registration: failures leave registry as-is.
    pub fn register_factories(&self, registry: &mut FactoryRegistry) -> anyhow::Result<()> {
        let mut next = registry.clone();
        for factory in &self.factories {
            next.register(factory.clone())?;
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
    /// Both functions and every returned pointer must obey ABI 1.0. Their code and
    /// metadata must be pinned for process lifetime. This is not a plugin sandbox.
    pub unsafe fn from_entry_points(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe { Self::from_entry_points_with_services(metadata, entry, None) }
    }

    /// Host an optional versioned service extension without changing ABI 1.0.
    ///
    /// # Safety
    /// The base and optional entry points must belong to the same pinned binary,
    /// and all returned handles must obey their respective callback contracts.
    pub unsafe fn from_entry_points_with_services(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe { Self::from_entry_points_with_recovery(metadata, entry, services, None) }
    }

    /// Host independently negotiated admission and read-only recovery services.
    ///
    /// # Safety
    /// Every entry point must belong to the same process-pinned binary and obey
    /// its versioned ownership and callback contract.
    pub unsafe fn from_entry_points_with_recovery(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
        recovery: Option<abi::recovery::RecoveryFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe { Self::from_entry_points_with_bootstrap(metadata, entry, services, recovery, None) }
    }

    /// Host an independently negotiated, required consumer-progress read service.
    ///
    /// # Safety
    /// All entry points must belong to the same process-pinned binary and obey
    /// their versioned ownership and callback contracts.
    pub unsafe fn from_entry_points_with_consumer_progress(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
        recovery: Option<abi::recovery::RecoveryFn>,
        consumer_progress: Option<abi::consumer_progress::ConsumerProgressFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe {
            Self::from_exports(
                || metadata(),
                |out| take_status(entry(out)),
                || services.map(|entry| entry()),
                || recovery.map(|entry| entry()),
                || None,
                || None,
                || None,
                || None,
                || None,
                || consumer_progress.map(|entry| entry()),
            )
        }
    }

    /// Host independently negotiated query-owned bootstrap providers.
    ///
    /// # Safety
    /// All entry points must belong to the same process-pinned binary and obey
    /// their versioned ownership/callback contracts.
    pub unsafe fn from_entry_points_with_bootstrap(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
        recovery: Option<abi::recovery::RecoveryFn>,
        bootstrap: Option<abi::bootstrap::BootstrapFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe {
            Self::from_entry_points_with_consumer(
                metadata, entry, services, recovery, bootstrap, None,
            )
        }
    }

    /// Host native consumer completion while retaining progress/commit ownership.
    ///
    /// # Safety
    /// All entry points must belong to the same process-pinned binary and obey
    /// their versioned ownership and callback contracts.
    pub unsafe fn from_entry_points_with_consumer(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
        recovery: Option<abi::recovery::RecoveryFn>,
        bootstrap: Option<abi::bootstrap::BootstrapFn>,
        consumer: Option<abi::consumer::ConsumerFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe {
            Self::from_entry_points_with_configuration(
                metadata, entry, services, recovery, bootstrap, consumer, None,
            )
        }
    }

    /// Host optional configuration descriptions without changing the base ABI.
    ///
    /// # Safety
    /// All entry points must belong to the same pinned binary and their metadata
    /// must remain immutable and valid for the lifetime of that binary.
    pub unsafe fn from_entry_points_with_configuration(
        metadata: abi::MetadataFn,
        entry: abi::EntryFn,
        services: Option<abi::services::ServicesFn>,
        recovery: Option<abi::recovery::RecoveryFn>,
        bootstrap: Option<abi::bootstrap::BootstrapFn>,
        consumer: Option<abi::consumer::ConsumerFn>,
        configuration: Option<abi::configuration::ConfigurationFn>,
    ) -> anyhow::Result<Arc<Self>> {
        unsafe {
            Self::from_exports(
                || metadata(),
                |out| take_status(entry(out)),
                || services.map(|entry| entry()),
                || recovery.map(|entry| entry()),
                || bootstrap.map(|entry| entry()),
                || consumer.map(|entry| entry()),
                || configuration.map(|entry| entry()),
                || None,
                || None,
                || None,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn from_exports(
        metadata: impl FnOnce() -> *const abi::Metadata,
        entry: impl FnOnce(
            *mut abi::PluginHandle,
        ) -> Result<(), drasi_computation_plugin_sdk::transport::Failure>,
        services: impl FnOnce() -> Option<*const abi::services::PluginServicesV1>,
        recovery: impl FnOnce() -> Option<*const abi::recovery::PluginRecoveryV1>,
        bootstrap: impl FnOnce() -> Option<*const abi::bootstrap::PluginBootstrapV1>,
        consumer: impl FnOnce() -> Option<*const abi::consumer::PluginConsumerV1>,
        configuration: impl FnOnce() -> Option<*const abi::configuration::ConfigurationMetadataV1>,
        batching: impl FnOnce() -> Option<*const abi::batching::PluginBatchingV1>,
        query_snapshots: impl FnOnce() -> Option<*const abi::query_snapshots::PluginQuerySnapshotsV1>,
        consumer_progress: impl FnOnce() -> Option<
            *const abi::consumer_progress::PluginConsumerProgressV1,
        >,
    ) -> anyhow::Result<Arc<Self>> {
        let manifest = unsafe { read_metadata(metadata())? };
        let mut configurations = unsafe { read_configuration(configuration(), &manifest)? };
        let batching = unsafe { read_batching(batching())? };
        let query_snapshots = unsafe { read_query_snapshots(query_snapshots())? };
        let consumer_progress = unsafe { read_consumer_progress(consumer_progress())? };
        let consumer = consumer()
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry)? };
                anyhow::ensure!(
                    table.version == abi::consumer::VERSION
                        && table.reserved == 0
                        && table.factory.is_some()
                        && table.create.is_some()
                        && table.inspect.is_some()
                        && table.begin.is_some()
                        && table.end_batch.is_some(),
                    "incompatible or incomplete native consumer extension"
                );
                Ok(table)
            })
            .transpose()?;
        let bootstrap = unsafe { read_bootstrap(bootstrap())? };
        let recovery = recovery()
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry)? };
                anyhow::ensure!(
                    table.version == abi::recovery::VERSION
                        && table.reserved == 0
                        && table.factory.is_some()
                        && table.create.is_some()
                        && table.inspect.is_some(),
                    "incompatible or incomplete native recovery extension"
                );
                Ok(table)
            })
            .transpose()?;
        let services = services()
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry)? };
                anyhow::ensure!(
                    table.version == abi::services::VERSION
                        && table.reserved == 0
                        && table.factory.is_some()
                        && table.create.is_some(),
                    "incompatible or incomplete native service extension"
                );
                Ok(table)
            })
            .transpose()?;
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
            let sink_batching = if let Some(extension) = batching {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(extension
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                wire::decode::<bool>(&response)?.then_some(extension)
            } else {
                None
            };
            let snapshot_reads = if let Some(extension) = query_snapshots {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(extension
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                wire::decode::<bool>(&response)?.then_some(extension)
            } else {
                None
            };
            let consumer_binding = if let Some(extension) = consumer {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(extension
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                let declared: drasi_computation_plugin_sdk::consumer::FactoryConsumer =
                    wire::decode(&response)?;
                anyhow::ensure!(
                    declared.version == abi::consumer::VERSION,
                    "unsupported native consumer version"
                );
                declared.mode.map(|mode| (mode, extension))
            } else {
                None
            };
            let source_progress = if let Some(recovery) = recovery {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(recovery
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                let capability: drasi_computation_plugin_sdk::progress::FactoryRecovery =
                    drasi_computation_plugin_sdk::progress::decode(&response)?;
                anyhow::ensure!(
                    capability.version == abi::recovery::VERSION,
                    "unsupported native factory recovery version"
                );
                if capability.source_progress {
                    anyhow::ensure!(
                        metadata.role == drasi_lib::computation::v1::ComponentRole::Source,
                        "native source progress requires a source"
                    );
                    Some(recovery)
                } else {
                    None
                }
            } else {
                None
            };
            let admission = if let Some(services) = services {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(services
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                let capabilities: drasi_computation_plugin_sdk::admission::FactoryServices =
                    wire::decode(&response)?;
                anyhow::ensure!(
                    capabilities.version == abi::services::VERSION,
                    "unsupported native factory service version"
                );
                if capabilities.source_admission {
                    anyhow::ensure!(
                        metadata.role == drasi_lib::computation::v1::ComponentRole::Source
                            && metadata.ports.len() == 1,
                        "native admission requires a single-output source"
                    );
                    Some(services)
                } else {
                    None
                }
            } else {
                None
            };
            let consumer_progress = if let Some(extension) = consumer_progress {
                let response = unsafe {
                    drasi_computation_plugin_sdk::transport::take_reply(extension
                        .factory
                        .expect("validated")(
                        plugin.raw.state, index
                    ))?
                };
                let declared: drasi_computation_plugin_sdk::consumer_progress::FactoryProgress =
                    drasi_computation_plugin_sdk::consumer_progress::decode(&response)?;
                anyhow::ensure!(
                    declared.version == abi::consumer_progress::VERSION,
                    "unsupported native factory consumer progress version"
                );
                declared.consumer_progress.then_some(extension)
            } else {
                None
            };
            unsafe {
                take_status(plugin.table().factory.expect("validated")(
                    plugin.raw.state,
                    index,
                    &mut handle,
                ))?
            };
            factories.push(Arc::new(unsafe {
                NativeFactory::new(
                    metadata.clone(),
                    handle,
                    plugin.clone(),
                    codec.clone(),
                    schemas.clone(),
                    admission,
                    source_progress,
                )?
                .with_consumer(consumer_binding)?
                .with_consumer_progress(consumer_progress)?
                .with_batching(sink_batching)?
                .with_query_snapshots(snapshot_reads)?
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
                bootstrap_factories.push(Arc::new(NativeBootstrapFactory::new(
                    metadata,
                    u32::try_from(index)?,
                    extension,
                    plugin.clone(),
                    codec.clone(),
                )));
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
    let metadata = unsafe { library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let entry = unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let services = unsafe { library.get::<abi::services::ServicesFn>(abi::services::SYMBOL) }
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
    let query_snapshots = unsafe {
        library.get::<abi::query_snapshots::QuerySnapshotsFn>(abi::query_snapshots::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let consumer_progress = unsafe {
        library.get::<abi::consumer_progress::ConsumerProgressFn>(abi::consumer_progress::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    if metadata.is_none()
        && entry.is_none()
        && services.is_none()
        && recovery.is_none()
        && bootstrap.is_none()
        && consumer.is_none()
        && configuration.is_none()
        && batching.is_none()
        && query_snapshots.is_none()
        && consumer_progress.is_none()
    {
        return Ok(None);
    }

    // Retaining only successful handles is insufficient: metadata/entry code can
    // have retained callbacks or initialized I/O before reporting an error.
    std::mem::forget(library);
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("native computation metadata symbol is missing"))?;
    let entry =
        entry.ok_or_else(|| anyhow::anyhow!("native computation entry symbol is missing"))?;
    Ok(Some(unsafe {
        NativePlugin::from_exports(
            || metadata(),
            |out| take_status(entry(out)),
            || services.map(|entry| entry()),
            || recovery.map(|entry| entry()),
            || bootstrap.map(|entry| entry()),
            || consumer.map(|entry| entry()),
            || configuration.map(|entry| entry()),
            || batching.map(|entry| entry()),
            || query_snapshots.map(|entry| entry()),
            || consumer_progress.map(|entry| entry()),
        )?
    }))
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
                    && table.create.is_some(),
                "incompatible or incomplete native bootstrap extension"
            );
            Ok(table)
        })
        .transpose()
}

unsafe fn read_batching(
    entry: Option<*const abi::batching::PluginBatchingV1>,
) -> anyhow::Result<Option<abi::batching::PluginBatchingV1>> {
    entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::batching::VERSION
                    && table.reserved == 0
                    && table.factory.is_some()
                    && table.policy.is_some(),
                "incompatible or incomplete native batching extension"
            );
            Ok(table)
        })
        .transpose()
}

unsafe fn read_query_snapshots(
    entry: Option<*const abi::query_snapshots::PluginQuerySnapshotsV1>,
) -> anyhow::Result<Option<abi::query_snapshots::PluginQuerySnapshotsV1>> {
    entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::query_snapshots::VERSION
                    && table.reserved == 0
                    && table.factory.is_some()
                    && table.create.is_some(),
                "incompatible or incomplete native query-snapshot extension"
            );
            Ok(table)
        })
        .transpose()
}

unsafe fn read_consumer_progress(
    entry: Option<*const abi::consumer_progress::PluginConsumerProgressV1>,
) -> anyhow::Result<Option<abi::consumer_progress::PluginConsumerProgressV1>> {
    entry
        .map(|entry| {
            let table = *unsafe { checked_table(entry)? };
            anyhow::ensure!(
                table.version == abi::consumer_progress::VERSION
                    && table.reserved == 0
                    && table.factory.is_some()
                    && table.create.is_some()
                    && table.inspect.is_some()
                    && table.position_policy.is_some(),
                "incompatible or incomplete native consumer progress extension"
            );
            Ok(table)
        })
        .transpose()
}

/// Inspect without calling the plugin entry or creating factory handles.
/// Extension factory catalogues are validated when the plugin is loaded.
pub(crate) fn try_read_metadata(library: Arc<Library>) -> anyhow::Result<Option<PluginMetadata>> {
    let metadata = unsafe { library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let has_entry = unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }.is_ok();
    let has_services =
        unsafe { library.get::<abi::services::ServicesFn>(abi::services::SYMBOL) }.is_ok();
    let has_recovery =
        unsafe { library.get::<abi::recovery::RecoveryFn>(abi::recovery::SYMBOL) }.is_ok();
    let bootstrap = unsafe { library.get::<abi::bootstrap::BootstrapFn>(abi::bootstrap::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let has_consumer =
        unsafe { library.get::<abi::consumer::ConsumerFn>(abi::consumer::SYMBOL) }.is_ok();
    let configuration =
        unsafe { library.get::<abi::configuration::ConfigurationFn>(abi::configuration::SYMBOL) }
            .ok()
            .map(|symbol| *symbol);
    let batching = unsafe { library.get::<abi::batching::BatchingFn>(abi::batching::SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let query_snapshots = unsafe {
        library.get::<abi::query_snapshots::QuerySnapshotsFn>(abi::query_snapshots::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    let consumer_progress = unsafe {
        library.get::<abi::consumer_progress::ConsumerProgressFn>(abi::consumer_progress::SYMBOL)
    }
    .ok()
    .map(|symbol| *symbol);
    if metadata.is_none()
        && !has_entry
        && !has_services
        && !has_recovery
        && bootstrap.is_none()
        && !has_consumer
        && configuration.is_none()
        && batching.is_none()
        && query_snapshots.is_none()
        && consumer_progress.is_none()
    {
        return Ok(None);
    }
    std::mem::forget(library);
    anyhow::ensure!(has_entry, "native computation entry symbol is missing");
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("native computation metadata symbol is missing"))?;
    let manifest = unsafe { read_metadata(metadata())? };
    unsafe { read_configuration(configuration.map(|entry| entry()), &manifest)? };
    unsafe { read_batching(batching.map(|entry| entry()))? };
    unsafe { read_query_snapshots(query_snapshots.map(|entry| entry()))? };
    unsafe { read_consumer_progress(consumer_progress.map(|entry| entry()))? };
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
        let table = unsafe { *export.consumer_progress() };
        assert!(unsafe { read_consumer_progress(None)? }.is_none());
        assert!(unsafe { read_consumer_progress(Some(std::ptr::null())) }.is_err());
        assert!(unsafe { read_consumer_progress(Some(&table))? }.is_some());
        for field in 0..7 {
            let mut invalid = table;
            match field {
                0 => invalid.header.size += 1,
                1 => invalid.version += 1,
                2 => invalid.reserved = 1,
                3 => invalid.factory = None,
                4 => invalid.create = None,
                5 => invalid.inspect = None,
                _ => invalid.position_policy = None,
            }
            assert!(unsafe { read_consumer_progress(Some(&invalid)) }.is_err());
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
        let path = target.join("debug/examples").join(format!(
            "{}native_consumer_progress_metadata{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        ));
        anyhow::ensure!(
            path.is_file(),
            "build drasi-host-sdk --example native_consumer_progress_metadata first"
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
