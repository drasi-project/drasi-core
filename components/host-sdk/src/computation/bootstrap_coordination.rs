// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    consumer_progress::ConsumerProgressBinding, loader::PluginOwner, progress::ProgressBinding,
    NativeFactory,
};
use async_trait::async_trait;
use drasi_computation_plugin_abi::{self as abi, bootstrap_coordination::*};
use drasi_computation_plugin_sdk::{
    bootstrap,
    transport::{self, checked_table, take_reply, take_status, Failure, OperationFuture},
    CreateRequest, Scope, SourceBootstrapMetadata,
};
use drasi_lib::computation::v1::{
    ComponentId, ConsumerProgressSet, ImplementationIdentity, QuerySourceProgress, ResourceCleanup,
};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[cfg(test)]
#[path = "bootstrap_coordination_tests.rs"]
mod tests;

/// An immutable catalogue of actual native source factories, not graph instances.
pub type NativeSourceBootstrapFactories = BTreeMap<ImplementationIdentity, Arc<NativeFactory>>;

/// Actual graph-owned progress owners retained by a source coordinator.
pub enum NativeSourceBootstrapProgress {
    Single(Arc<QuerySourceProgress>),
    Shared(Arc<ConsumerProgressSet>),
}

impl NativeSourceBootstrapProgress {
    fn validate(&self, request: &CreateRequest) -> anyhow::Result<()> {
        let scope = request
            .scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("source bootstrap requires graph scope"))?;
        ComponentId::try_new(scope.instance_id.as_str())?;
        ComponentId::try_new(scope.graph_id.as_str())?;
        match self {
            Self::Single(owner) => anyhow::ensure!(
                owner.graph_id() == scope.graph_id && owner.component_id() != &request.id,
                "source bootstrap progress belongs to another graph or the source itself"
            ),
            Self::Shared(owners) => anyhow::ensure!(
                owners.graph_id() == scope.graph_id
                    && owners.producer_id() == &request.id
                    && !owners.owners().is_empty()
                    && owners.owners().len() <= MAX_MEMBERS,
                "source bootstrap consumer progress belongs to another graph or producer"
            ),
        }
        Ok(())
    }

    fn owns(&self, owner: &Arc<QuerySourceProgress>) -> bool {
        match self {
            Self::Single(actual) => Arc::ptr_eq(actual, owner),
            Self::Shared(actual) => actual.owns(owner),
        }
    }
}

enum Bindings {
    Single(ProgressBinding),
    Shared {
        consumers: ConsumerProgressBinding,
        members: BTreeMap<ComponentId, ProgressBinding>,
    },
}

impl Bindings {
    fn new(owners: &NativeSourceBootstrapProgress) -> anyhow::Result<Self> {
        Ok(match owners {
            NativeSourceBootstrapProgress::Single(owner) => {
                Self::Single(ProgressBinding::new(owner))
            }
            NativeSourceBootstrapProgress::Shared(owners) => {
                let consumers = ConsumerProgressBinding::new(owners)?;
                // This only activates a capability. It reads no snapshot and starts no I/O.
                consumers.activate()?.complete();
                Self::Shared {
                    consumers,
                    members: owners
                        .owners()
                        .iter()
                        .map(|(id, owner)| (id.clone(), ProgressBinding::new(owner)))
                        .collect(),
                }
            }
        })
    }

    fn with_table<T>(&self, call: impl FnOnce(&BootstrapProgressV1) -> T) -> T {
        let single = match self {
            Self::Single(binding) => Some(binding.table()),
            _ => None,
        };
        let consumers = match self {
            Self::Shared { consumers, .. } => Some(consumers.table()),
            _ => None,
        };
        let members: Vec<_> = match self {
            Self::Shared { members, .. } => members
                .iter()
                .map(|(id, binding)| BootstrapProgressMemberV1 {
                    id: abi::BorrowedBytes::new(id.as_str().as_bytes()),
                    progress: binding.table(),
                })
                .collect(),
            _ => Vec::new(),
        };
        call(&BootstrapProgressV1 {
            header: abi::Header::new::<BootstrapProgressV1>(),
            single: single.as_ref().map_or(std::ptr::null(), |table| table),
            consumers: consumers.as_ref().map_or(std::ptr::null(), |table| table),
            members: if members.is_empty() {
                std::ptr::null()
            } else {
                members.as_ptr()
            },
            member_count: members.len() as u32,
            reserved: 0,
        })
    }
}

struct Lifecycle {
    retiring: bool,
    stopped: bool,
    bindings: Option<Bindings>,
}

/// One graph-owned resource shared by the live source and query bootstrap resources.
///
/// Only the source library owns the engine. The Host retains real progress owners
/// and fences construction/open against final retirement.
pub struct NativeSourceBootstrapResource {
    request: CreateRequest,
    raw: BootstrapCoordinatorHandle,
    _plugin: Arc<PluginOwner>,
    factory_state: usize,
    owners: NativeSourceBootstrapProgress,
    identity: Option<Arc<super::NativeIdentityResource>>,
    lifecycle: Mutex<Lifecycle>,
    stop: tokio::sync::Mutex<Option<OperationFuture>>,
}

unsafe impl Send for NativeSourceBootstrapResource {}
unsafe impl Sync for NativeSourceBootstrapResource {}

struct CoordinatorReleaseGuard {
    raw: Option<BootstrapCoordinatorHandle>,
    release: unsafe extern "C" fn(*mut c_void),
}

impl CoordinatorReleaseGuard {
    unsafe fn new(raw: BootstrapCoordinatorHandle) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null(),
            "null native bootstrap coordinator state"
        );
        let release = table.release.ok_or_else(|| {
            anyhow::anyhow!("native bootstrap coordinator has no release callback")
        })?;
        Ok(Self {
            raw: Some(raw),
            release,
        })
    }

    fn into_raw(mut self) -> BootstrapCoordinatorHandle {
        self.raw.take().expect("owned coordinator")
    }
}

impl Drop for CoordinatorReleaseGuard {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { (self.release)(raw.state) };
        }
    }
}

unsafe fn coordinator_table(
    raw: BootstrapCoordinatorHandle,
) -> anyhow::Result<BootstrapCoordinatorVTable> {
    let table = *unsafe { checked_table(raw.vtable)? };
    anyhow::ensure!(
        !raw.state.is_null()
            && table.retain.is_some()
            && table.release.is_some()
            && table.describe.is_some()
            && table.open.is_some()
            && table.stop.is_some(),
        "incomplete native bootstrap coordinator table"
    );
    Ok(table)
}

impl NativeSourceBootstrapResource {
    pub(super) fn create(
        request: CreateRequest,
        owners: NativeSourceBootstrapProgress,
        extension: PluginBootstrapCoordinationV1,
        factory_state: *mut c_void,
        plugin: Arc<PluginOwner>,
        identity: Option<Arc<super::NativeIdentityResource>>,
    ) -> anyhow::Result<Arc<Self>> {
        owners.validate(&request)?;
        let bindings = Bindings::new(&owners)?;
        anyhow::ensure!(
            request.activation_credentials_bound == identity.is_some(),
            "source bootstrap identity binding does not match its creation request"
        );
        let bytes = if identity.is_some() {
            drasi_computation_plugin_sdk::activation_credentials::encode_bound_create_request(
                &request,
            )?
        } else {
            bootstrap::encode(&request)?
        };
        anyhow::ensure!(
            bytes.len() <= abi::bootstrap::MAX_CONTROL_BYTES,
            "source bootstrap request exceeds supported size"
        );
        let mut raw = BootstrapCoordinatorHandle::null();
        bindings.with_table(|table| unsafe {
            take_status(extension.create.expect("validated")(
                factory_state,
                abi::BorrowedBytes::new(&bytes),
                table,
                &mut raw,
            ))
        })?;
        // A valid release callback owns even an otherwise incomplete handle.
        // Install it before validating the rest of the producer's table.
        let owner = unsafe { CoordinatorReleaseGuard::new(raw)? };
        let table = unsafe { coordinator_table(raw)? };
        let resource = Arc::new(Self {
            request,
            raw: owner.into_raw(),
            _plugin: plugin,
            factory_state: factory_state as usize,
            owners,
            identity,
            lifecycle: Mutex::new(Lifecycle {
                retiring: false,
                stopped: false,
                bindings: Some(bindings),
            }),
            stop: tokio::sync::Mutex::new(None),
        });
        let description = unsafe { take_reply(table.describe.expect("validated")(raw.state))? };
        let described: CreateRequest = bootstrap::decode(&description)?;
        resource.validate_source_request(&described)?;
        Ok(resource)
    }

    fn validate_scope(&self, scope: Option<&Scope>) -> anyhow::Result<()> {
        let expected = self.request.scope.as_ref().expect("validated scope");
        anyhow::ensure!(
            scope.is_some_and(|scope| scope.instance_id == expected.instance_id
                && scope.graph_id == expected.graph_id),
            "source bootstrap belongs to another instance or graph"
        );
        Ok(())
    }

    fn validate_source_request(&self, request: &CreateRequest) -> anyhow::Result<()> {
        self.validate_scope(request.scope.as_ref())?;
        anyhow::ensure!(
            request.id == self.request.id
                && request.implementation == self.request.implementation
                && request.configuration_version == self.request.configuration_version
                && request.configuration == self.request.configuration,
            "source bootstrap source identity, implementation or configuration mismatch"
        );
        // Resource generation is not a component generation. Actual owner identity
        // and the retired engine's leases fence reconstructed source instances.
        Ok(())
    }

    pub(super) fn validate_factory(
        &self,
        state: *mut c_void,
        plugin: &Arc<PluginOwner>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self._plugin, plugin) && self.factory_state == state as usize,
            "source bootstrap belongs to another actual native factory"
        );
        Ok(())
    }

    pub(super) fn validate_identity(
        &self,
        identity: Option<&Arc<super::NativeIdentityResource>>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            match (&self.identity, identity) {
                (Some(expected), Some(actual)) => Arc::ptr_eq(expected, actual),
                (None, None) => true,
                _ => false,
            },
            "source bootstrap requires the exact actual live identity resource"
        );
        Ok(())
    }

    pub(super) fn with_source<T>(
        &self,
        request: &CreateRequest,
        single: Option<&Arc<QuerySourceProgress>>,
        shared: Option<&Arc<ConsumerProgressSet>>,
        call: impl FnOnce(&BootstrapCoordinatorHandle) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.validate_source_request(request)?;
        anyhow::ensure!(
            match (&self.owners, single, shared) {
                (NativeSourceBootstrapProgress::Single(actual), Some(owner), None) =>
                    Arc::ptr_eq(actual, owner),
                (NativeSourceBootstrapProgress::Shared(actual), None, Some(owner)) =>
                    Arc::ptr_eq(actual, owner),
                _ => false,
            },
            "source bootstrap requires the exact actual live progress owner"
        );
        let lifecycle = self.lifecycle.lock().map_err(|_| Failure::closed())?;
        anyhow::ensure!(!lifecycle.retiring, "source bootstrap resource is retired");
        call(&self.raw)
    }

    pub(super) fn bind_query(
        self: &Arc<Self>,
        query: &ComponentId,
        scope: &Scope,
        owner: &Arc<QuerySourceProgress>,
    ) -> anyhow::Result<SourceBootstrapBinding> {
        self.validate_scope(Some(scope))?;
        anyhow::ensure!(
            owner.component_id() == query && self.owners.owns(owner),
            "paired bootstrap requires its captured actual query progress owner"
        );
        let lifecycle = self.lifecycle.lock().map_err(|_| Failure::closed())?;
        anyhow::ensure!(!lifecycle.retiring, "source bootstrap resource is retired");
        Ok(SourceBootstrapBinding(Arc::new(QueryBinding {
            coordinator: self.clone(),
            query: query.clone(),
            metadata: bootstrap::encode(&SourceBootstrapMetadata {
                source: self.request.clone(),
                query: query.clone(),
            })?,
            revoked: AtomicBool::new(false),
        })))
    }

    fn open(&self, query: &ComponentId) -> anyhow::Result<abi::bootstrap::BootstrapHandle> {
        let lifecycle = self.lifecycle.lock().map_err(|_| Failure::closed())?;
        anyhow::ensure!(!lifecycle.retiring, "source bootstrap resource is retired");
        let mut raw = abi::bootstrap::BootstrapHandle::null();
        unsafe {
            take_status((*self.raw.vtable).open.expect("validated")(
                self.raw.state,
                abi::BorrowedBytes::new(query.as_str().as_bytes()),
                &mut raw,
            ))?;
        }
        Ok(raw)
    }
}

#[async_trait]
impl ResourceCleanup for NativeSourceBootstrapResource {
    async fn shutdown(&self) -> anyhow::Result<()> {
        {
            let mut lifecycle = self.lifecycle.lock().map_err(|_| Failure::closed())?;
            if lifecycle.stopped {
                return Ok(());
            }
            lifecycle.retiring = true;
        }
        let mut pending = self.stop.lock().await;
        if self
            .lifecycle
            .lock()
            .map_err(|_| Failure::closed())?
            .stopped
        {
            return Ok(());
        }
        if pending.is_none() {
            let mut raw = abi::OperationHandle::null();
            unsafe {
                take_status((*self.raw.vtable).stop.expect("validated")(
                    self.raw.state,
                    &mut raw,
                ))?;
                *pending = Some(OperationFuture::new(raw)?);
            }
        }
        // Poll in place: dropping this await leaves the accepted operation and
        // every progress binding in the resource for the next cleanup attempt.
        let result = pending.as_mut().expect("accepted stop").await;
        *pending = None;
        anyhow::ensure!(
            result?.is_empty(),
            "native bootstrap coordinator stop returned an unexpected payload"
        );
        let mut lifecycle = self.lifecycle.lock().map_err(|_| Failure::closed())?;
        lifecycle.stopped = true;
        lifecycle.bindings = None;
        Ok(())
    }
}

impl Drop for NativeSourceBootstrapResource {
    fn drop(&mut self) {
        unsafe { (*self.raw.vtable).release.expect("validated")(self.raw.state) };
    }
}

struct QueryBinding {
    coordinator: Arc<NativeSourceBootstrapResource>,
    query: ComponentId,
    metadata: Vec<u8>,
    revoked: AtomicBool,
}

pub(super) struct SourceBootstrapBinding(Arc<QueryBinding>);

impl SourceBootstrapBinding {
    pub(super) fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }

    pub(super) fn table(&self) -> SourceBootstrapV1 {
        SourceBootstrapV1 {
            header: abi::Header::new::<SourceBootstrapV1>(),
            context: Arc::as_ptr(&self.0) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            open: Some(open),
        }
    }
}

impl Drop for SourceBootstrapBinding {
    fn drop(&mut self) {
        self.revoke();
    }
}

unsafe fn binding<'a>(context: *mut c_void) -> Result<&'a QueryBinding, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null source bootstrap binding"));
    }
    Ok(unsafe { &*context.cast::<QueryBinding>() })
}
unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe {
        Arc::increment_strong_count(context.cast::<QueryBinding>())
    });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe {
        Arc::decrement_strong_count(context.cast::<QueryBinding>())
    });
}
unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let binding = unsafe { binding(context)? };
        if binding.revoked.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        let lifecycle = binding
            .coordinator
            .lifecycle
            .lock()
            .map_err(|_| Failure::closed())?;
        if lifecycle.retiring {
            return Err(Failure::closed());
        }
        Ok(binding.metadata.clone())
    })
}
unsafe extern "C" fn open(
    context: *mut c_void,
    out: *mut abi::bootstrap::BootstrapHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol("null paired bootstrap output"));
        }
        let binding = unsafe { binding(context)? };
        if binding.revoked.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        let raw = binding
            .coordinator
            .open(&binding.query)
            .map_err(Failure::from)?;
        unsafe { transport::write_out(out, raw) }
    })
}
