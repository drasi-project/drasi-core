// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Internal source/bootstrap coordination without application database access.

use crate::{
    abi::{self, bootstrap_coordination::*},
    bootstrap,
    transport::{self, Failure},
    ControlSender, CreateRequest, CreatedComponent, NativeBootstrapProvider,
};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentId, ConsumerProgressView, SourceProgressView};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::c_void, sync::Arc};

/// Readers kept active by the coordinator resource, including before source start.
#[derive(Clone)]
pub enum SourceBootstrapProgress {
    Single(SourceProgressView),
    Shared {
        consumers: ConsumerProgressView,
        members: BTreeMap<ComponentId, SourceProgressView>,
    },
}

/// The source's independently negotiated live-reader binding.
pub enum SourceBootstrapLiveProgress {
    Single(SourceProgressView),
    Shared(ConsumerProgressView),
}

#[async_trait]
pub trait NativeSourceBootstrapCoordinator: Send + Sync {
    /// Configuration-only source construction using this coordinator's engine.
    fn create_source(
        &self,
        request: &CreateRequest,
        control: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> anyhow::Result<CreatedComponent>;

    /// Open a provider using the SDK's captured actual member reader.
    fn open(
        &self,
        query: &ComponentId,
        progress: SourceProgressView,
    ) -> anyhow::Result<Arc<dyn NativeBootstrapProvider>>;

    /// Retire the actual workers and leases before the host revokes readers.
    async fn stop(&self) -> anyhow::Result<()>;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBootstrapMetadata {
    pub source: CreateRequest,
    pub query: ComponentId,
}

/// A query-scoped pairing supplied only to its independently loaded bootstrapper.
/// It can acquire a bootstrap provider, but cannot run SQL or mutate progress.
pub struct NativeSourceBootstrap {
    raw: SourceBootstrapV1,
    metadata: SourceBootstrapMetadata,
}

unsafe impl Send for NativeSourceBootstrap {}
unsafe impl Sync for NativeSourceBootstrap {}

impl NativeSourceBootstrap {
    /// # Safety
    /// The table must obey the versioned producer-owned callback contract.
    pub(crate) unsafe fn from_borrowed(raw: *const SourceBootstrapV1) -> anyhow::Result<Self> {
        let raw = *unsafe { transport::checked_table(raw)? };
        anyhow::ensure!(
            !raw.context.is_null()
                && raw.retain.is_some()
                && raw.release.is_some()
                && raw.describe.is_some()
                && raw.open.is_some(),
            "incomplete native source/bootstrap pairing"
        );
        let bytes =
            unsafe { transport::take_reply(raw.describe.expect("validated")(raw.context))? };
        let metadata: SourceBootstrapMetadata = bootstrap::decode(&bytes)?;
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self { raw, metadata })
    }

    pub fn metadata(&self) -> &SourceBootstrapMetadata {
        &self.metadata
    }

    /// Transfer one actual source-owned provider through the existing bootstrap ABI.
    pub fn open(&self) -> anyhow::Result<LinkedBootstrapProvider> {
        let mut raw = abi::bootstrap::BootstrapHandle::null();
        unsafe {
            transport::take_status(self.raw.open.expect("validated")(
                self.raw.context,
                &mut raw,
            ))?;
            LinkedBootstrapProvider::from_raw(raw)
        }
    }
}

impl Drop for NativeSourceBootstrap {
    fn drop(&mut self) {
        unsafe { self.raw.release.expect("validated")(self.raw.context) };
    }
}

/// Owns a provider allocated by the source library. Its Rust layout is never
/// transferred: only the original handle and producer callbacks cross the ABI.
pub struct LinkedBootstrapProvider {
    raw: Option<abi::bootstrap::BootstrapHandle>,
}

impl LinkedBootstrapProvider {
    unsafe fn from_raw(raw: abi::bootstrap::BootstrapHandle) -> anyhow::Result<Self> {
        let table = unsafe { transport::checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null() && table.release.is_some(),
            "incomplete paired bootstrap provider"
        );
        let provider = Self { raw: Some(raw) };
        anyhow::ensure!(
            table.begin.is_some() && table.cancel_snapshot.is_some(),
            "incomplete paired bootstrap operations"
        );
        Ok(provider)
    }

    pub(crate) fn into_raw(mut self) -> abi::bootstrap::BootstrapHandle {
        self.raw.take().expect("owned linked bootstrap provider")
    }
}

impl Drop for LinkedBootstrapProvider {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { (*raw.vtable).release.expect("validated")(raw.state) };
        }
    }
}

pub(crate) unsafe fn decode_query(bytes: abi::BorrowedBytes) -> Result<ComponentId, Failure> {
    let value = std::str::from_utf8(unsafe {
        transport::borrowed_bytes(bytes, abi::bootstrap::MAX_CONTROL_BYTES)?
    })
    .map_err(|_| Failure::protocol("invalid bootstrap query identifier"))?;
    ComponentId::try_new(value).map_err(|error| Failure::from(anyhow::Error::from(error)))
}

pub(crate) fn require_context(context: *mut c_void) -> Result<(), Failure> {
    if context.is_null() {
        Err(Failure::protocol("null bootstrap coordinator"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    unsafe extern "C" fn release_provider(raw: *mut c_void) {
        let count = unsafe { Box::from_raw(raw.cast::<Arc<AtomicUsize>>()) };
        count.fetch_add(1, Ordering::AcqRel);
    }

    #[test]
    fn malformed_linked_provider_releases_transferred_ownership() {
        static INCOMPLETE: abi::bootstrap::BootstrapVTable = abi::bootstrap::BootstrapVTable {
            header: abi::Header::new::<abi::bootstrap::BootstrapVTable>(),
            release: Some(release_provider),
            begin: None,
            cancel_snapshot: None,
        };
        let released = Arc::new(AtomicUsize::new(0));
        let raw = abi::bootstrap::BootstrapHandle {
            state: Box::into_raw(Box::new(released.clone())).cast(),
            vtable: &INCOMPLETE,
        };
        assert!(unsafe { LinkedBootstrapProvider::from_raw(raw) }.is_err());
        assert_eq!(released.load(Ordering::Acquire), 1);
        assert_eq!(Arc::strong_count(&released), 1);
    }
}
