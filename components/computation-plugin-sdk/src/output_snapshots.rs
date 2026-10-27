// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host-provided read capability over graph output snapshots.
//!
//! A component names one real output endpoint (component and port) of a source,
//! transformer or query. The host answers with the output's logical position
//! (producer epoch and sequence), dataset and declared schema, then streams the
//! full-image records one bounded call at a time. Typed read outcomes (absent
//! capability, unknown output, not ready, retired) arrive as
//! [`OutputReadError`], never as an empty snapshot.

use crate::{
    abi::{self, output_snapshots::*},
    output_reads::{Outcome, WireCursor, WireRecord},
    transport::{checked_table, take_status, Failure, OperationFuture, ReceivedBytes},
    wire,
};
use bytes::Bytes;
use drasi_computation_contracts::{
    ComponentId, OutputCapability, OutputCursor, OutputEndpoint, OutputReadError, PortId, Record,
    RecordId, RecordImage, Schema, SchemaDescriptor,
};
use serde::{Deserialize, Serialize};
use std::{pin::Pin, sync::Arc};

/// The bounded open request: the endpoint whose snapshot is read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotOpen {
    pub component: String,
    pub port: String,
}

impl SnapshotOpen {
    pub fn new(endpoint: &OutputEndpoint) -> Self {
        Self {
            component: endpoint.component.to_string(),
            port: endpoint.port.to_string(),
        }
    }

    pub fn endpoint(&self) -> Result<OutputEndpoint, drasi_computation_contracts::ContractError> {
        Ok(OutputEndpoint::new(
            ComponentId::try_new(self.component.as_str())?,
            PortId::try_new(self.port.as_str())?,
        ))
    }
}

/// PREPARE reply: the captured snapshot's endpoint, logical position, dataset
/// and declared schema. Records follow through NEXT.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshotHeader {
    pub component: String,
    pub port: String,
    pub cursor: WireCursor,
    pub dataset: String,
    pub schema: SchemaDescriptor,
}

pub struct NativeOutputSnapshot {
    pub endpoint: OutputEndpoint,
    /// The output's producer epoch and logical sequence at capture.
    pub cursor: OutputCursor,
    pub dataset: String,
    pub records: Pin<Box<dyn futures_core::Stream<Item = anyhow::Result<Record>> + Send + 'static>>,
}

struct RetainedSnapshots(OutputSnapshotsV1);
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
pub struct NativeOutputSnapshots(Arc<RetainedSnapshots>);

impl NativeOutputSnapshots {
    /// # Safety
    /// The host table must obey the version-1 snapshot contract and remain
    /// callable until the retained capability is released.
    pub unsafe fn from_borrowed(raw: *const OutputSnapshotsV1) -> Result<Self, Failure> {
        let raw = *unsafe { checked_table(raw)? };
        if raw.context.is_null()
            || raw.retain.is_none()
            || raw.release.is_none()
            || raw.open.is_none()
        {
            return Err(Failure::protocol(
                "incomplete native output-snapshot capability",
            ));
        }
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(Arc::new(RetainedSnapshots(raw))))
    }

    /// Capture `endpoint`'s snapshot. `schema` is the schema the caller decodes:
    /// the output's declared schema must be identical, and every record is
    /// validated against it. A typed [`OutputReadError`] (absent capability,
    /// unknown output, not ready, retired) is returned as the error.
    pub async fn snapshot(
        &self,
        endpoint: &OutputEndpoint,
        schema: &Arc<Schema>,
    ) -> anyhow::Result<NativeOutputSnapshot> {
        let reader = self.open(endpoint)?;
        let bytes = reader.request(operation::PREPARE).await?;
        let outcome: Outcome<OutputSnapshotHeader> = wire::decode(&bytes)?;
        let header = outcome.into_result(endpoint, OutputCapability::Snapshot)?;
        anyhow::ensure!(
            header.component == endpoint.component.as_str()
                && header.port == endpoint.port.as_str(),
            "output snapshot answered for another endpoint"
        );
        anyhow::ensure!(
            &header.schema == schema.descriptor(),
            "output snapshot does not use the expected schema"
        );
        let cursor = header.cursor.into_cursor()?;
        anyhow::ensure!(
            cursor.producer().component_id() == &endpoint.component,
            "output snapshot position belongs to another component"
        );
        let schema = schema.clone();
        Ok(NativeOutputSnapshot {
            endpoint: endpoint.clone(),
            cursor,
            dataset: header.dataset,
            records: Box::pin(futures_util::stream::try_unfold(reader, move |reader| {
                let schema = schema.clone();
                async move {
                    let bytes = reader.request(operation::NEXT).await?;
                    let next: Option<WireRecord<'_>> = wire::decode(&bytes)?;
                    let Some(wire_record) = next else {
                        return Ok(None);
                    };
                    anyhow::ensure!(
                        wire_record.image == 0,
                        "snapshot record is not a full image"
                    );
                    let identity = RecordId::try_new(
                        &*wire_record.namespace,
                        Bytes::copy_from_slice(&wire_record.identity),
                    )?;
                    let record = Record::try_new(
                        &schema,
                        identity,
                        RecordImage::Full,
                        Bytes::copy_from_slice(&wire_record.payload),
                    )?;
                    Ok(Some((record, reader)))
                }
            })),
        })
    }

    fn open(&self, endpoint: &OutputEndpoint) -> anyhow::Result<Reader> {
        let request = wire::encode(&SnapshotOpen::new(endpoint))?;
        anyhow::ensure!(
            request.len() <= MAX_OPEN_BYTES,
            "output snapshot request is too large"
        );
        let raw = &self.0 .0;
        let mut reader = OutputSnapshotHandle::null();
        unsafe {
            take_status(raw.open.expect("validated")(
                raw.context,
                abi::BorrowedBytes::new(&request),
                &mut reader,
            ))?;
        }
        Ok(unsafe { Reader::new(reader)? })
    }
}

struct Reader(OutputSnapshotHandle);
// Reader operations are serialized by the owning stream, and host callbacks
// enforce independent operation/lifetime guards.
unsafe impl Send for Reader {}
unsafe impl Sync for Reader {}
impl Reader {
    unsafe fn new(raw: OutputSnapshotHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(raw.vtable)? };
        if raw.state.is_null() || table.release.is_none() || table.request.is_none() {
            return Err(Failure::protocol(
                "incomplete native output-snapshot reader",
            ));
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
