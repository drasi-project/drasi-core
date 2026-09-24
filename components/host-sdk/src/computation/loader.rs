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

use super::NativeFactory;

pub struct NativePlugin {
    metadata: PluginMetadata,
    factories: Vec<Arc<NativeFactory>>,
}
impl NativePlugin {
    pub fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }
    pub fn factories(&self) -> &[Arc<NativeFactory>] {
        &self.factories
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
        let manifest = unsafe { read_metadata(metadata)? };
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
                )?
            }));
        }
        Ok(Arc::new(Self {
            metadata: manifest,
            factories,
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
    if metadata.is_none() && entry.is_none() {
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
        NativePlugin::from_entry_points(metadata, entry)?
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
    if metadata.is_none() && !has_entry {
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
