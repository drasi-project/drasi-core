// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    abi::{self, query_snapshots::*},
    transport::{checked_table, take_status, Failure, OperationFuture, ReceivedBytes},
    wire,
};
use serde::{Deserialize, Serialize};
use std::{pin::Pin, sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySnapshotMetadata {
    pub as_of_sequence: u64,
    pub generation: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySnapshotRow {
    pub value: serde_json::Value,
}

pub struct NativeQuerySnapshot {
    pub metadata: QuerySnapshotMetadata,
    pub rows: Pin<
        Box<dyn futures_core::Stream<Item = anyhow::Result<serde_json::Value>> + Send + 'static>,
    >,
}

struct RetainedSnapshots(QuerySnapshotsV1);
// Only producer callbacks dereference context; the capability permits
// concurrent bounded readers and thread-safe retain/release.
unsafe impl Send for RetainedSnapshots {}
unsafe impl Sync for RetainedSnapshots {}
impl Drop for RetainedSnapshots {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

#[derive(Clone)]
pub struct NativeQuerySnapshots(Arc<RetainedSnapshots>);

impl NativeQuerySnapshots {
    /// # Safety
    /// The host table must obey the version-1 snapshot contract and remain
    /// callable until the retained capability is released.
    pub unsafe fn from_borrowed(raw: *const QuerySnapshotsV1) -> Result<Self, Failure> {
        let raw = *unsafe { checked_table(raw)? };
        if raw.context.is_null()
            || raw.retain.is_none()
            || raw.release.is_none()
            || raw.open.is_none()
        {
            return Err(Failure::protocol(
                "incomplete native query-snapshot capability",
            ));
        }
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(Arc::new(RetainedSnapshots(raw))))
    }

    pub async fn snapshot(&self, query: &str) -> anyhow::Result<NativeQuerySnapshot> {
        let reader = self.open(query)?;
        let metadata = wire::decode(&reader.request(operation::PREPARE).await?)?;
        Ok(NativeQuerySnapshot {
            metadata,
            rows: Box::pin(futures_util::stream::try_unfold(
                reader,
                |reader| async move {
                    let bytes = reader.request(operation::NEXT).await?;
                    let row: Option<QuerySnapshotRow> = wire::decode(&bytes)?;
                    Ok(row.map(|row| (row.value, reader)))
                },
            )),
        })
    }

    fn open(&self, query: &str) -> anyhow::Result<Reader> {
        drasi_computation_contracts::ComponentId::try_new(query)?;
        let raw = &self.0 .0;
        let mut reader = QuerySnapshotHandle::null();
        unsafe {
            take_status(raw.open.expect("validated")(
                raw.context,
                abi::BorrowedBytes::new(query.as_bytes()),
                &mut reader,
            ))?;
        }
        Ok(unsafe { Reader::new(reader)? })
    }
}

struct Reader(QuerySnapshotHandle);
// Reader operations are serialized by the owning stream, and host callbacks
// enforce independent operation/lifetime guards.
unsafe impl Send for Reader {}
unsafe impl Sync for Reader {}
impl Reader {
    unsafe fn new(raw: QuerySnapshotHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(raw.vtable)? };
        if raw.state.is_null() || table.release.is_none() || table.request.is_none() {
            return Err(Failure::protocol("incomplete native query-snapshot reader"));
        }
        Ok(Self(raw))
    }

    async fn request(&self, code: u32) -> Result<ReceivedBytes, Failure> {
        self.begin(code)?.await
    }

    fn begin(&self, code: u32) -> Result<OperationFuture, Failure> {
        let table = unsafe { &*self.0.vtable };
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(table.request.expect("validated")(
                self.0.state,
                code,
                &mut operation,
            ))?;
            OperationFuture::new(operation)
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        let table = unsafe { &*self.0.vtable };
        unsafe { table.release.expect("validated")(self.0.state) };
    }
}
