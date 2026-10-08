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
        let manifest = unsafe { read_metadata(metadata)? };
        let consumer = consumer
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry())? };
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
        let bootstrap = bootstrap
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry())? };
                anyhow::ensure!(
                    table.version == abi::bootstrap::VERSION
                        && table.reserved == 0
                        && table.factories.is_some()
                        && table.create.is_some(),
                    "incompatible or incomplete native bootstrap extension"
                );
                Ok(table)
            })
            .transpose()?;
        let recovery = recovery
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry())? };
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
        let services = services
            .map(|entry| -> anyhow::Result<_> {
                let table = *unsafe { checked_table(entry())? };
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
        unsafe { take_status(entry(&mut handle))? };
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
    if metadata.is_none()
        && entry.is_none()
        && services.is_none()
        && recovery.is_none()
        && bootstrap.is_none()
        && consumer.is_none()
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
        NativePlugin::from_entry_points_with_consumer(
            metadata, entry, services, recovery, bootstrap, consumer,
        )?
    }))
}

unsafe fn read_metadata(metadata: abi::MetadataFn) -> anyhow::Result<PluginMetadata> {
    let metadata = unsafe { checked_table(metadata())? };
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

/// Inspect the native family without creating plugin factory handles.
pub(crate) fn try_read_metadata(library: Arc<Library>) -> anyhow::Result<Option<PluginMetadata>> {
    let metadata = unsafe { library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL) }
        .ok()
        .map(|symbol| *symbol);
    let has_entry = unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }.is_ok();
    let has_services =
        unsafe { library.get::<abi::services::ServicesFn>(abi::services::SYMBOL) }.is_ok();
    let has_recovery =
        unsafe { library.get::<abi::recovery::RecoveryFn>(abi::recovery::SYMBOL) }.is_ok();
    let has_bootstrap =
        unsafe { library.get::<abi::bootstrap::BootstrapFn>(abi::bootstrap::SYMBOL) }.is_ok();
    let has_consumer =
        unsafe { library.get::<abi::consumer::ConsumerFn>(abi::consumer::SYMBOL) }.is_ok();
    if metadata.is_none()
        && !has_entry
        && !has_services
        && !has_recovery
        && !has_bootstrap
        && !has_consumer
    {
        return Ok(None);
    }
    std::mem::forget(library);
    anyhow::ensure!(has_entry, "native computation entry symbol is missing");
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("native computation metadata symbol is missing"))?;
    Ok(Some(unsafe { read_metadata(metadata)? }))
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
