// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Scoped access to a host-owned processing transaction.

use std::{
    ffi::c_void,
    future::Future,
    sync::{Arc, Mutex},
};

use drasi_computation_contracts::{SourceProgressKey, StreamId};
use drasi_core_types::{
    interface::{
        FutureElementRef, IndexError, IndexJoin, OutboxPageLimits, PushType, ResultKey,
        ResultOwner, RowMutation, SourceCheckpoint, StorageDurability, ValueAccumulator,
    },
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    abi::{self, processing::*},
    transaction::RetainedTransaction,
    transport::{self, checked_table, take_status, Failure, OperationFuture},
    wire,
};

struct RetainedProcessing(ProcessingV1);
// Only the producer callbacks dereference the context; retain/release are thread-safe.
unsafe impl Send for RetainedProcessing {}
unsafe impl Sync for RetainedProcessing {}
impl Drop for RetainedProcessing {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

/// Evidence from the actual host-owned transaction bundle, not plugin claims.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicStateInfo {
    pub durability: StorageDurability,
    pub persistent_checkpoints: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessingStateInfo {
    pub atomic: bool,
    pub volatile: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_atomic_publication: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_atomic_reset: Option<ProcessingResetInfo>,
    pub durability: StorageDurability,
    pub persistent_checkpoints: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessingResetInfo {
    pub generation: u64,
    pub in_progress: bool,
}

/// Host-resolved input replay decision. Its prepared progress remains confined
/// to the current operation and must be staged before that operation succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputProgressDecision {
    pub duplicate: bool,
    pub source_id: Option<String>,
}

/// Host-owned storage and transaction authority for one component.
///
/// The service is not yet the complete indexed-state or durable-output
/// contract required by query plugins.
#[derive(Clone)]
pub struct NativeProcessing(Arc<RetainedProcessing>);

impl NativeProcessing {
    /// # Safety
    /// The table must obey the processing ABI and remain callable until released.
    pub unsafe fn from_borrowed(raw: *const ProcessingV1) -> Result<Self, Failure> {
        let raw = *unsafe { checked_table(raw)? };
        if raw.context.is_null()
            || raw.retain.is_none()
            || raw.release.is_none()
            || raw.run.is_none()
        {
            return Err(Failure::protocol("incomplete native processing capability"));
        }
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(Arc::new(RetainedProcessing(raw))))
    }

    /// Run one operation inside the host's session. The host applies the
    /// provider's rollback and fencing rules. Cancellation during commit can
    /// leave an uncertain outcome; non-atomic providers can retain partial
    /// writes. State handles are revoked before this call completes.
    pub async fn run<F, Fut, T>(&self, work: F) -> anyhow::Result<T>
    where
        F: FnOnce(NativeProcessingContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        self.run_kind(work, 0).await
    }

    /// Reserve the actual shared output destinations before beginning the
    /// transaction. At most one retained output may be appended by this work.
    pub async fn run_output<F, Fut, T>(&self, work: F) -> anyhow::Result<T>
    where
        F: FnOnce(NativeProcessingContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        self.run_kind(work, OUTPUT_WORK).await
    }

    /// Commit the evaluation phase of explicit non-atomic publication without
    /// publishing consumed-input progress. Finish with run_output and
    /// resume_input_progress; intervening metadata/evaluation work is rejected.
    pub async fn run_deferred_progress<F, Fut, T>(&self, work: F) -> anyhow::Result<T>
    where
        F: FnOnce(NativeProcessingContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        self.run_kind(work, DEFERRED_PROGRESS_WORK).await
    }

    async fn run_kind<F, Fut, T>(&self, work: F, mode: u32) -> anyhow::Result<T>
    where
        F: FnOnce(NativeProcessingContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        let operation = {
            let work = Arc::new(Work(Mutex::new(Some(work))));
            let table = ProcessingWorkV1 {
                header: abi::Header::new::<ProcessingWorkV1>(),
                context: Arc::as_ptr(&work).cast_mut().cast(),
                retain: Some(retain::<F>),
                release: Some(release::<F>),
                begin: Some(begin::<F, Fut, T>),
            };
            let mut operation = abi::OperationHandle::null();
            let service = &self.0 .0;
            unsafe {
                let extended = OutputProcessingWorkV1 {
                    work: ProcessingWorkV1 {
                        header: abi::Header::new::<OutputProcessingWorkV1>(),
                        ..table
                    },
                    mode,
                    reserved: 0,
                };
                let table = if mode == 0 { &table } else { &extended.work };
                take_status(service.run.expect("validated")(
                    service.context,
                    table,
                    &mut operation,
                ))?;
                OperationFuture::new(operation)?
            }
        };
        let bytes = operation.await.map_err(|error| {
            index_error_from_failure(&error)
                .map(anyhow::Error::new)
                .unwrap_or_else(|| anyhow::Error::new(error))
        })?;
        wire::decode(&bytes)
    }
}

/// A revocable view of the active host session, not a transaction owner.
pub struct NativeProcessingContext {
    transaction: RetainedTransaction,
}
impl NativeProcessingContext {
    /// Persist reset intent before any independently committed state is cleared.
    pub async fn begin_non_atomic_reset(
        &self,
        preserve_bootstrap: bool,
    ) -> Result<u64, IndexError> {
        self.call(request::NON_ATOMIC_RESET_BEGIN, &preserve_bootstrap)
            .await
    }

    /// Apply a previously committed non-atomic reset intent. This operation
    /// stages checkpoint clearing; intent remains until initialization completes.
    pub async fn reset_non_atomic_state(
        &self,
        preserve_bootstrap: bool,
    ) -> Result<u64, IndexError> {
        self.call(request::NON_ATOMIC_STATE_RESET, &preserve_bootstrap)
            .await
    }

    pub async fn finish_non_atomic_reset(&self) -> Result<(), IndexError> {
        self.call(request::NON_ATOMIC_RESET_FINISH, &()).await
    }

    /// Stage publication of the preceding, already committed evaluation phase.
    /// Its progress remains invisible until this output operation commits.
    pub async fn resume_input_progress(&self) -> Result<(), IndexError> {
        self.call(request::INPUT_PROGRESS_RESUME, &()).await
    }

    /// Reserve from the graph's actual stream counter when one is bound. A
    /// failed operation may leave a transport gap, but cannot reuse a sequence.
    pub async fn next_emission_sequence(&self, stored: u64) -> Result<u64, IndexError> {
        self.call(request::NEXT_EMISSION_SEQUENCE, &stored).await
    }

    pub async fn state_info(&self) -> Result<ProcessingStateInfo, IndexError> {
        self.call(request::STATE_INFO, &()).await
    }

    /// Physical eligibility time supplied by the actual host, not logical event time.
    pub async fn physical_time(&self) -> Result<u64, IndexError> {
        self.call(request::PHYSICAL_TIME, &()).await
    }

    async fn call<I: Serialize, O: DeserializeOwned>(
        &self,
        code: u32,
        input: &I,
    ) -> Result<O, IndexError> {
        self.transaction
            .call(code, input)
            .await
            .map_err(index_call_error)
    }

    /// Rejects resources that do not share the owner's actual atomic result domain.
    pub async fn atomic_state_info(&self) -> Result<AtomicStateInfo, IndexError> {
        self.call(request::ATOMIC_STATE_INFO, &()).await
    }

    pub async fn prepare_input_progress(
        &self,
        input: &drasi_computation_contracts::InputEnvelope,
    ) -> Result<InputProgressDecision, IndexError> {
        self.input_progress(input, request::INPUT_PROGRESS_PREPARE)
            .await
    }

    /// Validate the existing input protocol without staging or reserving it.
    /// Non-atomic callers use this before committing an evaluation marker.
    pub async fn validate_input_progress(
        &self,
        input: &drasi_computation_contracts::InputEnvelope,
    ) -> Result<InputProgressDecision, IndexError> {
        self.input_progress(input, request::INPUT_PROGRESS_VALIDATE)
            .await
    }

    async fn input_progress(
        &self,
        input: &drasi_computation_contracts::InputEnvelope,
        request: u32,
    ) -> Result<InputProgressDecision, IndexError> {
        let codec = wire::codec(&[drasi_computation_contracts::GraphChangeCodec::schema()])
            .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        let input = wire::Envelope::input(input, &codec)
            .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        self.call(request, &input).await
    }

    pub async fn stage_prepared_input_progress(&self) -> Result<(), IndexError> {
        self.call(request::INPUT_PROGRESS_STAGE, &()).await
    }

    pub async fn bind_input_producer(
        &self,
        producer: &drasi_computation_contracts::GraphProducerIdentity,
        key: &SourceProgressKey,
    ) -> Result<(), IndexError> {
        self.call(request::INPUT_PRODUCER_BIND, &(producer, key))
            .await
    }

    pub async fn read_checkpoint(&self, key: &str) -> Result<Option<SourceCheckpoint>, IndexError> {
        let value: Option<(u64, Option<Vec<u8>>)> =
            self.call(request::CHECKPOINT_GET, &key).await?;
        Ok(value
            .map(|(sequence, position)| SourceCheckpoint::new(sequence, position.map(Into::into))))
    }

    pub async fn read_input_checkpoint(
        &self,
        stream: &StreamId,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.read_progress_checkpoint(&SourceProgressKey::Stream(stream.clone()))
            .await
    }

    pub async fn read_progress_checkpoint(
        &self,
        key: &SourceProgressKey,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        let value: Option<(u64, Option<Vec<u8>>)> =
            self.call(request::INPUT_CHECKPOINT_GET, key).await?;
        Ok(value
            .map(|(sequence, position)| SourceCheckpoint::new(sequence, position.map(Into::into))))
    }

    /// Stages the checkpoint in this transaction. Readers observe it only after
    /// the host confirms commit, never when this state request completes.
    pub async fn stage_input_checkpoint(
        &self,
        stream: &StreamId,
        sequence: u64,
        position: Option<&[u8]>,
    ) -> Result<(), IndexError> {
        self.stage_progress_checkpoint(
            &SourceProgressKey::Stream(stream.clone()),
            sequence,
            position,
            stream,
            sequence,
        )
        .await
    }

    pub async fn stage_progress_checkpoint(
        &self,
        key: &SourceProgressKey,
        sequence: u64,
        position: Option<&[u8]>,
        stream: &StreamId,
        transport_sequence: u64,
    ) -> Result<(), IndexError> {
        self.call(
            request::INPUT_CHECKPOINT_STAGE,
            &(key, sequence, position, Some((stream, transport_sequence))),
        )
        .await
    }

    pub async fn stage_bootstrap_watermark(
        &self,
        key: &SourceProgressKey,
        sequence: u64,
        position: Option<&[u8]>,
    ) -> Result<(), IndexError> {
        self.call(
            request::INPUT_CHECKPOINT_STAGE,
            &(key, sequence, position, None::<(&StreamId, u64)>),
        )
        .await
    }

    pub async fn stage_input_transport(
        &self,
        stream: &StreamId,
        sequence: u64,
    ) -> Result<(), IndexError> {
        self.call(request::INPUT_TRANSPORT_STAGE, &(stream, sequence))
            .await
    }

    /// Commits the initialization barrier with its source positions and rows.
    pub async fn stage_bootstrap_complete(&self, complete: bool) -> Result<(), IndexError> {
        self.call(request::BOOTSTRAP_COMPLETE_STAGE, &complete)
            .await
    }

    /// Clear this owner's indexes, projection and progress atomically before
    /// readiness. Provider intent is preserved for bootstrap retry, not deletion.
    pub async fn reset_state(&self, preserve_bootstrap: bool) -> Result<u64, IndexError> {
        self.call(request::ATOMIC_STATE_RESET, &preserve_bootstrap)
            .await
    }

    /// Keep the query's logical high-water mark while replacing its projection.
    /// Requires a successful bootstrap-preserving reset in this same operation.
    pub async fn initialize_reset_output(&self, sequence: u64) -> Result<(), IndexError> {
        self.call(request::RESET_OUTPUT_BASELINE, &sequence).await
    }

    /// Clear explicitly volatile state in place. Failure fences the owner;
    /// this operation never claims rollback or restart durability.
    pub async fn reset_volatile_state(&self, preserve_bootstrap: bool) -> Result<u64, IndexError> {
        self.call(request::VOLATILE_STATE_RESET, &preserve_bootstrap)
            .await
    }

    pub async fn reset_generation(&self) -> Result<Option<u64>, IndexError> {
        self.call(request::RESET_GENERATION_GET, &()).await
    }

    pub async fn read_delivery_progress(&self) -> Result<Option<u64>, IndexError> {
        self.call(request::DELIVERY_PROGRESS_GET, &()).await
    }

    pub async fn stage_delivery_progress(&self, sequence: u64) -> Result<(), IndexError> {
        self.call(request::DELIVERY_PROGRESS_STAGE, &sequence).await
    }

    /// Recover the bound input's actual committed checkpoints during startup.
    /// The host publishes recovered-but-not-ready progress after this operation
    /// commits; component readiness remains a separate lifecycle boundary.
    pub async fn recover_input_progress(&self) -> Result<(), IndexError> {
        self.call(request::INPUT_PROGRESS_RECOVER, &()).await
    }

    pub async fn stage_checkpoint(
        &self,
        key: &str,
        sequence: u64,
        position: Option<&[u8]>,
    ) -> Result<(), IndexError> {
        self.call(request::CHECKPOINT_STAGE, &(key, sequence, position))
            .await
    }

    pub async fn read_result_sequence(&self) -> Result<Option<u64>, IndexError> {
        self.call(request::RESULT_SEQUENCE_GET, &()).await
    }

    pub async fn stage_result_sequence(&self, sequence: u64) -> Result<(), IndexError> {
        self.call(request::RESULT_SEQUENCE_STAGE, &sequence).await
    }

    pub async fn read_outbox_page(
        &self,
        after_sequence: u64,
        limits: OutboxPageLimits,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.call(request::OUTBOX_READ_PAGE, &(after_sequence, limits))
            .await
    }

    pub async fn append_outbox_and_trim(
        &self,
        sequence: u64,
        data: &[u8],
        retain_from: u64,
    ) -> Result<usize, IndexError> {
        self.call(
            request::OUTBOX_APPEND_AND_TRIM,
            &(sequence, data, retain_from),
        )
        .await
    }

    pub async fn read_live_rows_page(
        &self,
        after_signature: Option<u64>,
        limits: OutboxPageLimits,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.call(request::LIVE_ROWS_READ_PAGE, &(after_signature, limits))
            .await
    }

    pub async fn apply_live_rows(&self, mutations: &[RowMutation<'_>]) -> Result<(), IndexError> {
        let mutations: Vec<_> = mutations
            .iter()
            .map(|mutation| (mutation.row_signature, mutation.data))
            .collect();
        self.call(request::LIVE_ROWS_APPLY, &mutations).await
    }

    pub async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Element>, IndexError> {
        self.call(request::GET_ELEMENT, reference).await
    }

    pub async fn set_element(&self, element: &Element, slots: &[usize]) -> Result<(), IndexError> {
        self.call(request::SET_ELEMENT, &(element, slots)).await
    }

    pub async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.call(request::DELETE_ELEMENT, reference).await
    }

    pub async fn get_slot_element(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Element>, IndexError> {
        self.call(request::GET_SLOT_ELEMENT, &(slot, reference))
            .await
    }

    pub async fn set_joins(&self, joins: &[IndexJoin]) -> Result<(), IndexError> {
        self.call(request::SET_JOINS, &joins).await
    }

    pub async fn clear_elements(&self) -> Result<(), IndexError> {
        self.call(request::CLEAR_ELEMENTS, &()).await
    }

    pub async fn get_element_as_at(
        &self,
        reference: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Element>, IndexError> {
        self.call(request::GET_ARCHIVE_ELEMENT, &(reference, time))
            .await
    }

    pub async fn clear_archive(&self) -> Result<(), IndexError> {
        self.call(request::CLEAR_ARCHIVE, &()).await
    }

    pub async fn open_archive_versions(
        &self,
        reference: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<u64, IndexError> {
        let (from, previous) = match range.from {
            TimestampBound::Included(from) => (from, false),
            TimestampBound::StartFromPrevious(from) => (from, true),
        };
        self.call(
            request::ARCHIVE_VERSIONS_OPEN,
            &(reference, from, range.to, previous),
        )
        .await
    }

    pub async fn future_push(
        &self,
        push_type: PushType,
        position: usize,
        signature: u64,
        reference: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        self.call(
            request::FUTURE_PUSH,
            &(
                push_type,
                position,
                signature,
                reference,
                original_time,
                due_time,
            ),
        )
        .await
    }

    pub async fn future_remove(&self, position: usize, signature: u64) -> Result<(), IndexError> {
        self.call(request::FUTURE_REMOVE, &(position, signature))
            .await
    }

    pub async fn future_pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        self.call(request::FUTURE_POP, &()).await
    }

    pub async fn future_peek(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        self.call(request::FUTURE_PEEK, &()).await
    }

    pub async fn future_clear(&self) -> Result<(), IndexError> {
        self.call(request::FUTURE_CLEAR, &()).await
    }

    pub async fn get_result(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        self.call(request::RESULT_GET, &(key, owner)).await
    }

    pub async fn set_result(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        self.call(request::RESULT_SET, &(key, owner, value)).await
    }

    pub async fn open_slot(&self, slot: usize) -> Result<u64, IndexError> {
        self.call(request::SLOT_OPEN, &slot).await
    }

    pub async fn open_slot_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<u64, IndexError> {
        self.call(request::SLOT_INBOUND_OPEN, &(slot, reference))
            .await
    }

    pub async fn open_slot_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<u64, IndexError> {
        self.call(request::SLOT_OUTBOUND_OPEN, &(slot, reference))
            .await
    }

    pub async fn next_slot(&self, stream: u64) -> Result<Option<Element>, IndexError> {
        self.call(request::SLOT_NEXT, &stream).await
    }

    pub async fn sorted_next(
        &self,
        set: u64,
        value_bits: Option<u64>,
    ) -> Result<Option<(u64, isize)>, IndexError> {
        self.call(request::SORTED_GET_NEXT, &(set, value_bits))
            .await
    }

    pub async fn sorted_count(&self, set: u64, value_bits: u64) -> Result<isize, IndexError> {
        self.call(request::SORTED_GET_COUNT, &(set, value_bits))
            .await
    }

    pub async fn sorted_increment(
        &self,
        set: u64,
        value_bits: u64,
        delta: isize,
    ) -> Result<(), IndexError> {
        self.call(request::SORTED_INCREMENT, &(set, value_bits, delta))
            .await
    }
}

/// Preserve index error categories without transferring a Rust error object
/// across the native boundary.
pub fn index_error_to_failure(error: &IndexError) -> Failure {
    let (code, message) = match error {
        IndexError::IOError => (index_error::IO, error.to_string()),
        IndexError::NotSupported => (index_error::NOT_SUPPORTED, error.to_string()),
        IndexError::ArchiveNotEnabled => (index_error::ARCHIVE_NOT_ENABLED, error.to_string()),
        IndexError::CorruptedData => (index_error::CORRUPTED_DATA, error.to_string()),
        IndexError::ConnectionFailed(source) => {
            (index_error::CONNECTION_FAILED, format!(":{source}"))
        }
        IndexError::UnknownStore(name) => (index_error::UNKNOWN_STORE, format!(":{name}")),
        IndexError::Other(source) => (index_error::OTHER, format!(":{source}")),
    };
    Failure::new(code, message)
}

/// Decode only errors defined by the indexed-processing extension.
pub fn index_error_from_failure(error: &Failure) -> Option<IndexError> {
    let detail = || error.message.strip_prefix(':');
    match error.code {
        index_error::IO => Some(IndexError::IOError),
        index_error::NOT_SUPPORTED => Some(IndexError::NotSupported),
        index_error::ARCHIVE_NOT_ENABLED => Some(IndexError::ArchiveNotEnabled),
        index_error::CORRUPTED_DATA => Some(IndexError::CorruptedData),
        index_error::CONNECTION_FAILED => Some(IndexError::connection_failed(
            std::io::Error::other(detail()?.to_owned()),
        )),
        index_error::UNKNOWN_STORE => Some(IndexError::UnknownStore(detail()?.to_owned())),
        index_error::OTHER => Some(IndexError::other(std::io::Error::other(
            detail()?.to_owned(),
        ))),
        _ => None,
    }
}

fn index_call_error(error: anyhow::Error) -> IndexError {
    if let Some(index) = error
        .downcast_ref::<Failure>()
        .and_then(index_error_from_failure)
    {
        return index;
    }
    IndexError::Other(error.into_boxed_dyn_error())
}

fn work_failure(error: anyhow::Error) -> Failure {
    if let Some(index) = error.downcast_ref::<IndexError>() {
        if let IndexError::Other(source) = index {
            if let Some(failure) = source.downcast_ref::<Failure>() {
                return failure.clone();
            }
        }
        return index_error_to_failure(index);
    }
    Failure::from(error)
}

struct Work<F>(Mutex<Option<F>>);

unsafe extern "C" fn retain<F>(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<Work<F>>()) });
}

unsafe extern "C" fn release<F>(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<Work<F>>()) });
}

unsafe extern "C" fn begin<F, Fut, T>(
    context: *mut c_void,
    transaction: *const abi::Transaction,
    out: *mut abi::OperationHandle,
) -> abi::Status
where
    F: FnOnce(NativeProcessingContext) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Serialize + DeserializeOwned + Send + 'static,
{
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol("null native processing work or output"));
        }
        let work = unsafe { &*context.cast::<Work<F>>() };
        let work = work
            .0
            .lock()
            .map_err(|_| Failure::failed("native processing work poisoned"))?
            .take()
            .ok_or_else(Failure::closed)?;
        let transaction = unsafe { RetainedTransaction::new(transaction)? };
        let operation = transport::export_operation(
            async move {
                let result = work(NativeProcessingContext { transaction })
                    .await
                    .map_err(work_failure)?;
                let bytes = wire::encode(&result).map_err(Failure::from)?;
                let _: T = wire::decode(&bytes).map_err(Failure::from)?;
                Ok(bytes)
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_failure_round_trips_error_categories() {
        let errors = [
            IndexError::IOError,
            IndexError::NotSupported,
            IndexError::ArchiveNotEnabled,
            IndexError::CorruptedData,
            IndexError::connection_failed(std::io::Error::other("connection detail")),
            IndexError::UnknownStore("missing store".into()),
            IndexError::other(std::io::Error::other("provider detail")),
            IndexError::UnknownStore(String::new()),
            IndexError::other(std::io::Error::other("")),
        ];
        for error in errors {
            assert_eq!(
                index_error_from_failure(&index_error_to_failure(&error)),
                Some(error)
            );
        }
        assert!(index_error_from_failure(&Failure::closed()).is_none());
    }
}
