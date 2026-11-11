// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::processing::ProcessingBinding;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResourceCleanup,
        ComputationTransaction, InMemoryComputationProvider,
    },
    interface::{
        ElementIndex, ElementStream, FailureMode, IndexError, IndexJoin, IndexSet,
        OutboxPageLimits, PushType, RowMutation,
    },
};
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

#[path = "processing_progress_tests.rs"]
mod input_progress;
#[path = "processing_non_atomic_tests.rs"]
mod non_atomic;

fn element(id: &str) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1,
        },
        properties: ElementPropertyMap::from(json!({"value": id})),
    }
}

fn state_page() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: std::num::NonZeroUsize::new(1).expect("one record"),
        max_bytes: std::num::NonZeroUsize::new(1024).expect("positive bound"),
    }
}

async fn transaction_case() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let mut binding =
        ProcessingBinding::new(provider, "native-consumer".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    assert!(service.run(|_| async { Ok(()) }).await.is_err());
    binding.open().await?;

    let initial = element("first");
    let expected = initial.clone();
    service
        .run(move |context| async move {
            let state = context.atomic_state_info().await?;
            state.durability.require(FailureMode::ProcessRestart)?;
            assert!(state.persistent_checkpoints);
            context.set_element(&initial, &[0]).await?;
            context
                .stage_checkpoint("source", 1, Some(b"first-position"))
                .await?;
            context.stage_result_sequence(1).await?;
            context
                .append_outbox_and_trim(1, b"first-output", 1)
                .await?;
            context
                .apply_live_rows(&[
                    RowMutation {
                        row_signature: 0,
                        data: Some(b"first-row"),
                    },
                    RowMutation {
                        row_signature: 2,
                        data: Some(b"second-row"),
                    },
                    RowMutation {
                        row_signature: u64::MAX,
                        data: Some(b"last-row"),
                    },
                ])
                .await?;
            assert_eq!(
                context
                    .get_element(&ElementReference::new("source", "first"))
                    .await?,
                Some(initial)
            );
            Ok(())
        })
        .await?;

    let failed = element("failed");
    let failed_result: anyhow::Result<()> = service
        .run(move |context| async move {
            context.set_element(&failed, &[0]).await?;
            context
                .stage_checkpoint("source", 2, Some(b"rolled-back-position"))
                .await?;
            context.stage_result_sequence(2).await?;
            context
                .append_outbox_and_trim(2, b"rolled-back-output", 2)
                .await?;
            context
                .apply_live_rows(&[
                    RowMutation {
                        row_signature: 0,
                        data: None,
                    },
                    RowMutation {
                        row_signature: 1,
                        data: Some(b"rolled-back-row"),
                    },
                    RowMutation {
                        row_signature: 2,
                        data: Some(b"rolled-back-update"),
                    },
                ])
                .await?;
            anyhow::bail!("intentional rollback")
        })
        .await;
    assert!(failed_result.is_err());

    let retained = Arc::new(std::sync::Mutex::new(None));
    let capture = retained.clone();
    service
        .run(move |context| async move {
            assert_eq!(
                context
                    .get_element(&ElementReference::new("source", "first"))
                    .await?,
                Some(expected)
            );
            assert!(context
                .get_element(&ElementReference::new("source", "failed"))
                .await?
                .is_none());
            let checkpoint = context
                .read_checkpoint("source")
                .await?
                .expect("committed input");
            assert_eq!(checkpoint.sequence, 1);
            assert_eq!(
                checkpoint.source_position.as_deref(),
                Some(b"first-position".as_slice())
            );
            assert_eq!(context.read_result_sequence().await?, Some(1));
            assert_eq!(
                context.read_outbox_page(0, state_page()).await?,
                vec![(1, b"first-output".to_vec())]
            );
            assert_eq!(
                context.read_live_rows_page(None, state_page()).await?,
                vec![(0, b"first-row".to_vec())]
            );
            assert_eq!(
                context.read_live_rows_page(Some(0), state_page()).await?,
                vec![(2, b"second-row".to_vec())]
            );
            assert_eq!(
                context.read_live_rows_page(Some(2), state_page()).await?,
                vec![(u64::MAX, b"last-row".to_vec())]
            );
            assert!(context
                .read_live_rows_page(Some(u64::MAX), state_page())
                .await?
                .is_empty());
            *capture.lock().unwrap() = Some(context);
            Ok(())
        })
        .await?;
    let context = retained.lock().unwrap().take().unwrap();
    assert!(context
        .get_element(&ElementReference::new("source", "first"))
        .await
        .is_err());
    assert!(context.read_checkpoint("source").await.is_err());
    assert!(context
        .read_live_rows_page(None, state_page())
        .await
        .is_err());
    binding.close().await?;
    assert!(service.run(|_| async { Ok(()) }).await.is_err());
    binding.open().await?;
    let current = service
        .run(|context| async move {
            assert_eq!(
                context
                    .read_checkpoint("source")
                    .await?
                    .expect("reopened input")
                    .sequence,
                1
            );
            assert_eq!(context.read_result_sequence().await?, Some(1));
            assert_eq!(
                context.read_outbox_page(0, state_page()).await?,
                vec![(1, b"first-output".to_vec())]
            );
            assert_eq!(
                context.read_live_rows_page(None, state_page()).await?,
                vec![(0, b"first-row".to_vec())]
            );
            Ok(context
                .get_element(&ElementReference::new("source", "first"))
                .await?)
        })
        .await?;
    assert_eq!(current, Some(element("first")));
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_trims_large_output_after_projection_without_invalidating_iteration(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        "processor".into(),
    );
    binding.open().await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    service
        .run(|context| async move {
            let payload = vec![b'x'; 128 * 1024];
            let rows: Vec<_> = (0..260)
                .map(|signature| RowMutation {
                    row_signature: signature,
                    data: Some(b"retained row".as_slice()),
                })
                .collect();
            context.apply_live_rows(&rows).await?;
            context.append_outbox_and_trim(1, &payload, 1).await?;
            context.stage_result_sequence(1).await?;
            Ok(())
        })
        .await?;
    for sequence in 2..=4 {
        service
            .run(move |context| async move {
                context
                    .apply_live_rows(&[RowMutation {
                        row_signature: 0,
                        data: Some(b"updated row"),
                    }])
                    .await?;
                let payload = vec![b'y'; 128 * 1024];
                assert_eq!(
                    context
                        .append_outbox_and_trim(sequence, &payload, sequence)
                        .await?,
                    1
                );
                context.stage_result_sequence(sequence).await?;
                Ok(())
            })
            .await?;
    }
    service
        .run(|context| async move {
            assert_eq!(context.read_result_sequence().await?, Some(4));
            let entries = context.read_outbox_page(0, state_page()).await?;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0], (4, vec![b'y'; 128 * 1024]));
            assert_eq!(
                context.read_live_rows_page(None, state_page()).await?,
                vec![(0, b"updated row".to_vec())]
            );
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_rejects_atomic_state_on_an_unqualified_bundle(
) -> anyhow::Result<()> {
    let mut binding = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "graph".into(),
        "processor".into(),
    );
    binding.open().await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    assert!(service
        .run(|context| async move {
            context.stage_checkpoint("source", 1, None).await?;
            Ok(())
        })
        .await
        .is_err());
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_rejects_atomic_state_pages_over_budget() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        "processor".into(),
    );
    binding.open().await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    for (live_rows, records, bytes) in [
        (false, abi::processing::MAX_OUTBOX_PAGE_RECORDS + 1, 1024),
        (false, 1, abi::MAX_MESSAGE_BYTES + 1),
        (
            true,
            abi::processing::MAX_LIVE_RESULT_PAGE_RECORDS + 1,
            1024,
        ),
        (true, 1, abi::MAX_MESSAGE_BYTES + 1),
    ] {
        let limits = OutboxPageLimits {
            max_records: std::num::NonZeroUsize::new(records).expect("positive records"),
            max_bytes: std::num::NonZeroUsize::new(bytes).expect("positive bytes"),
        };
        let value = element("rejected-page");
        let outcome = service
            .run(move |context| async move {
                context.set_element(&value, &[0]).await?;
                if live_rows {
                    assert!(context.read_live_rows_page(None, limits).await.is_err());
                } else {
                    assert!(context.read_outbox_page(0, limits).await.is_err());
                }
                Ok(())
            })
            .await;
        assert!(
            outcome.is_err(),
            "ignoring a rejected state request must not commit"
        );
        service
            .run(|context| async move {
                assert!(context
                    .get_element(&ElementReference::new("source", "rejected-page"))
                    .await?
                    .is_none());
                Ok(())
            })
            .await?;
    }
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_commits_rolls_back_and_revokes_current_thread(
) -> anyhow::Result<()> {
    transaction_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_indexed_processing_commits_rolls_back_and_revokes_multi_thread(
) -> anyhow::Result<()> {
    transaction_case().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_exposes_slots_joins_archive_and_future_queue(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let mut binding =
        ProcessingBinding::new(provider, "native-consumer".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    service
        .run(|context| async move {
            let value = element("indexed");
            let reference = value.get_reference();
            context.set_joins(&[]).await?;
            context.set_element(&value, &[1]).await?;
            assert_eq!(
                context.get_slot_element(1, reference).await?,
                Some(value.clone())
            );
            assert!(context.get_slot_element(0, reference).await?.is_none());
            assert!(
                context
                    .future_push(PushType::Always, 7, 42, reference, 100, 200)
                    .await?
            );
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            let reference = ElementReference::new("source", "indexed");
            assert_eq!(context.future_peek().await?, Some(200));
            let future = context.future_pop().await?.expect("queued future");
            assert_eq!(future.element_ref, reference);
            assert_eq!(
                (
                    future.original_time,
                    future.due_time,
                    future.group_signature
                ),
                (100, 200, 42)
            );
            assert!(
                context
                    .future_push(PushType::Always, 8, 43, &reference, 101, 201)
                    .await?
            );
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            assert_eq!(context.future_peek().await?, Some(201));
            context.future_remove(8, 43).await?;
            context.clear_elements().await?;
            context.clear_archive().await?;
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            assert!(context.future_peek().await?.is_none());
            assert!(context
                .get_element(&ElementReference::new("source", "indexed"))
                .await?
                .is_none());
            context.future_clear().await?;
            Ok(())
        })
        .await?;
    let archive_error = service
        .run(|context| async move {
            context
                .get_element_as_at(&ElementReference::new("source", "indexed"), 100)
                .await?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(
        archive_error.downcast_ref::<IndexError>(),
        Some(IndexError::ArchiveNotEnabled)
    ));
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_failed_join_configuration_fences_owner() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let mut binding =
        ProcessingBinding::new(provider, "native-consumer".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    let result: anyhow::Result<()> = service
        .run(|context| async move {
            context.set_joins(&[]).await?;
            anyhow::bail!("failure after nontransactional join configuration")
        })
        .await;
    assert!(result.is_err());
    assert!(service.run(|_| async { Ok(()) }).await.is_err());
    binding.close().await?;
    binding.open().await?;
    service.run(|_| async { Ok(()) }).await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_uses_and_fences_the_actual_shared_group() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let indexes = provider.create_indexes("graph", "processor").await?;
    let group = SharedStorageGroup::new("graph", ComponentId::try_new("processor")?, indexes)?;
    let journal = group
        .transaction_group()
        .expect("shared transaction group")
        .journal_transaction("output")?;
    let mut binding = ProcessingBinding::new(group.clone(), "graph".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    service
        .run(|context| async move {
            context.set_element(&element("shared"), &[0]).await?;
            Ok(())
        })
        .await?;
    let seen = journal
        .run(async {
            Ok(journal
                .resources()
                .indexes()
                .element_index
                .get_element(&ElementReference::new("source", "shared"))
                .await?)
        })
        .await?;
    assert!(seen.is_some());
    let result: anyhow::Result<()> = service
        .run(|context| async move {
            context.set_joins(&[]).await?;
            anyhow::bail!("failure after join setup")
        })
        .await;
    assert!(result.is_err());
    assert!(group
        .transaction_group()
        .expect("shared transaction group")
        .recovery_required());
    assert!(journal.run(async { Ok(()) }).await.is_err());
    binding.close().await?;
    journal.shutdown().await?;
    ResourceCleanup::shutdown(group.as_ref()).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_cancelled_work_fences_its_owner() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let mut binding =
        ProcessingBinding::new(provider, "native-consumer".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let worker = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .run(move |context| async move {
                    context.set_element(&element("cancelled"), &[0]).await?;
                    signal.notify_one();
                    std::future::pending::<anyhow::Result<()>>().await
                })
                .await
        })
    };
    entered.notified().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(service.run(|_| async { Ok(()) }).await.is_err());
    binding.close().await?;
    binding.open().await?;
    assert!(service
        .run(|context| async move {
            Ok(context
                .get_element(&ElementReference::new("source", "cancelled"))
                .await?)
        })
        .await?
        .is_none());
    binding.close().await?;
    Ok(())
}

struct UnpolledWork(AtomicUsize);

unsafe extern "C" fn retain_unpolled(context: *mut std::ffi::c_void) {
    unsafe { Arc::increment_strong_count(context.cast::<UnpolledWork>()) };
}
unsafe extern "C" fn release_unpolled(context: *mut std::ffi::c_void) {
    unsafe { Arc::decrement_strong_count(context.cast::<UnpolledWork>()) };
}
unsafe extern "C" fn begin_unpolled(
    context: *mut std::ffi::c_void,
    _: *const abi::Transaction,
    _: *mut abi::OperationHandle,
) -> abi::Status {
    unsafe { &*context.cast::<UnpolledWork>() }
        .0
        .fetch_add(1, Ordering::SeqCst);
    sdk::transport::status_result(Err(sdk::transport::Failure::failed(
        "unpolled work must not start",
    )))
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_closes_with_an_unpolled_foreign_operation() -> anyhow::Result<()>
{
    let mut binding = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "graph".into(),
        "processor".into(),
    );
    binding.open().await?;
    let work = Arc::new(UnpolledWork(AtomicUsize::new(0)));
    let work_table = abi::processing::ProcessingWorkV1 {
        header: abi::Header::new::<abi::processing::ProcessingWorkV1>(),
        context: Arc::as_ptr(&work).cast_mut().cast(),
        retain: Some(retain_unpolled),
        release: Some(release_unpolled),
        begin: Some(begin_unpolled),
    };
    let service = binding.table();
    let mut raw = abi::OperationHandle::null();
    unsafe {
        sdk::transport::take_status(service.run.unwrap()(service.context, &work_table, &mut raw))?;
    }
    let operation = unsafe { sdk::transport::OperationFuture::new(raw)? };
    tokio::time::timeout(Duration::from_secs(5), binding.close()).await??;
    assert_eq!(work.0.load(Ordering::SeqCst), 0);
    let failure = match operation.await {
        Ok(_) => anyhow::bail!("unpolled work unexpectedly succeeded"),
        Err(failure) => failure,
    };
    assert_eq!(failure.code, abi::status::CANCELLED);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_close_cancels_started_work() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let mut binding =
        ProcessingBinding::new(provider, "native-consumer".into(), "processor".into());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let mut operation = Box::pin(service.run(move |context| async move {
        context.set_element(&element("interrupted"), &[0]).await?;
        signal.notify_one();
        std::future::pending::<anyhow::Result<()>>().await
    }));
    assert!(futures_util::poll!(operation.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await?;
    tokio::time::timeout(Duration::from_secs(5), binding.close()).await??;
    assert_eq!(
        operation
            .await
            .unwrap_err()
            .downcast_ref::<sdk::transport::Failure>()
            .map(|error| error.code),
        Some(abi::status::CANCELLED)
    );
    binding.open().await?;
    assert!(service
        .run(|context| async move {
            Ok(context
                .get_element(&ElementReference::new("source", "interrupted"))
                .await?)
        })
        .await?
        .is_none());
    binding.close().await?;
    Ok(())
}

type IndexResult<T> = std::result::Result<T, IndexError>;

struct UnsupportedElementIndex;
#[async_trait]
impl ElementIndex for UnsupportedElementIndex {
    async fn get_element(&self, _: &ElementReference) -> IndexResult<Option<Arc<Element>>> {
        Err(IndexError::NotSupported)
    }
    async fn set_element(&self, _: &Element, _: &Vec<usize>) -> IndexResult<()> {
        Err(IndexError::NotSupported)
    }
    async fn delete_element(&self, _: &ElementReference) -> IndexResult<()> {
        Err(IndexError::NotSupported)
    }
    async fn get_slot_element_by_ref(
        &self,
        _: usize,
        _: &ElementReference,
    ) -> IndexResult<Option<Arc<Element>>> {
        Err(IndexError::NotSupported)
    }
    async fn get_slot_elements_by_inbound(
        &self,
        _: usize,
        _: &ElementReference,
    ) -> IndexResult<ElementStream> {
        Err(IndexError::NotSupported)
    }
    async fn get_slot_elements_by_outbound(
        &self,
        _: usize,
        _: &ElementReference,
    ) -> IndexResult<ElementStream> {
        Err(IndexError::NotSupported)
    }
    async fn clear(&self) -> IndexResult<()> {
        Err(IndexError::NotSupported)
    }
    async fn set_joins(&self, _: &[IndexJoin]) -> IndexResult<()> {
        Err(IndexError::NotSupported)
    }
}

struct UnsupportedProvider;
#[async_trait]
impl ComputationIndexProvider for UnsupportedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> IndexResult<ComputationIndexes> {
        let indexes = InMemoryComputationProvider
            .create_indexes(graph, component)
            .await?;
        let original = indexes.indexes();
        let set = IndexSet {
            element_index: Arc::new(UnsupportedElementIndex),
            archive_index: original.archive_index.clone(),
            result_index: original.result_index.clone(),
            future_queue: original.future_queue.clone(),
            session_control: original.session_control.clone(),
        };
        ComputationIndexes::try_new(set, None, None, None, None).map_err(IndexError::other)
    }
    fn is_volatile(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_preserves_index_errors_and_rejects_ignored_errors(
) -> anyhow::Result<()> {
    for ignore in [false, true] {
        let mut binding = ProcessingBinding::new(
            Arc::new(UnsupportedProvider),
            "graph".into(),
            "processor".into(),
        );
        let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
        binding.open().await?;
        let error = service
            .run(move |context| async move {
                let result = context
                    .get_element(&ElementReference::new("source", "unsupported"))
                    .await;
                if ignore {
                    assert!(matches!(result, Err(IndexError::NotSupported)));
                } else {
                    result?;
                }
                Ok(())
            })
            .await
            .unwrap_err();
        if ignore {
            assert!(error
                .to_string()
                .contains("failed, cancelled or unfinished"));
        } else {
            assert!(matches!(
                error.downcast_ref::<IndexError>(),
                Some(IndexError::NotSupported)
            ));
        }
        assert!(service.run(|_| async { Ok(()) }).await.is_err());
        binding.close().await?;
    }
    Ok(())
}

#[derive(Default)]
struct Delayed {
    opened: Notify,
    release: Notify,
    closed: Notify,
    cancelled: AtomicBool,
}

#[async_trait]
impl ComputationResourceCleanup for Delayed {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    async fn shutdown(&self) -> std::result::Result<(), IndexError> {
        self.closed.notify_one();
        Ok(())
    }
}

#[async_trait]
impl ComputationIndexProvider for Delayed {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        self.opened.notify_one();
        self.release.notified().await;
        // The returned resource uses the same cleanup owner observed by this test.
        let indexes = InMemoryComputationProvider
            .create_indexes(graph, component)
            .await?;
        Ok(indexes)
    }
    fn is_volatile(&self) -> bool {
        true
    }
}

struct DelayedProvider(Arc<Delayed>);
#[async_trait]
impl ComputationIndexProvider for DelayedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let indexes = self.0.create_indexes(graph, component).await?;
        Ok(indexes.with_cleanup(self.0.clone()))
    }
    fn is_volatile(&self) -> bool {
        true
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_cancelled_open_is_owned_until_stop() -> anyhow::Result<()> {
    let delayed = Arc::new(Delayed::default());
    let provider = Arc::new(DelayedProvider(delayed.clone()));
    let mut binding = ProcessingBinding::new(provider, "graph".into(), "processor".into());
    let mut opening = Box::pin(binding.open());
    assert!(futures_util::poll!(opening.as_mut()).is_pending());
    delayed.opened.notified().await;
    drop(opening);
    let mut closing = Box::pin(binding.close());
    assert!(futures_util::poll!(closing.as_mut()).is_pending());
    delayed.release.notify_one();
    closing.await?;
    assert!(delayed.cancelled.load(Ordering::SeqCst));
    delayed.closed.notified().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_dropped_open_keeps_cleanup_ownership() -> anyhow::Result<()> {
    let delayed = Arc::new(Delayed::default());
    let provider = Arc::new(DelayedProvider(delayed.clone()));
    let mut binding = ProcessingBinding::new(provider, "graph".into(), "processor".into());
    let mut opening = Box::pin(binding.open());
    assert!(futures_util::poll!(opening.as_mut()).is_pending());
    delayed.opened.notified().await;
    drop(opening);
    drop(binding);
    delayed.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), delayed.closed.notified()).await?;
    assert!(delayed.cancelled.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn native_indexed_processing_drop_uses_its_host_runtime_outside_a_runtime() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let delayed = Arc::new(Delayed::default());
    delayed.release.notify_one();
    let mut binding = ProcessingBinding::new(
        Arc::new(DelayedProvider(delayed.clone())),
        "graph".into(),
        "processor".into(),
    );
    runtime.block_on(binding.open())?;
    drop(binding);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), delayed.closed.notified()).await
    })?;
    assert!(delayed.cancelled.load(Ordering::SeqCst));
    Ok(())
}

async fn loaded_plugin_case(plugin: &NativePlugin) -> anyhow::Result<()> {
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "fixture/indexed-processing"
        })
        .expect("processing fixture");
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    assert!(factory
        .create_component(ComponentId::try_new("processor")?, json!({"fail":false}))
        .is_err());
    for (sequence, fail) in [(1, false), (2, true)] {
        let mut component = factory.create_component_with_processing(
            ComponentId::try_new("processor")?,
            json!({"fail":fail}),
            super::consumer_tests::scope(),
            provider.clone(),
        )?;
        component.start().await?;
        let input = InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: super::envelope(&ComponentId::try_new("source")?, sequence),
            execution_context: Default::default(),
        };
        let result = component.transform(input).await;
        assert_eq!(result.is_err(), fail);
        component.stop().await?;
    }
    let owner = ComputationTransaction::try_new(
        provider
            .create_indexes("native-consumer", "processor")
            .await?,
    )?;
    let (first, failed) = owner
        .run(async {
            let index = &owner.resources().indexes().element_index;
            Ok((
                index
                    .get_element(&ElementReference::new("source", "1"))
                    .await?,
                index
                    .get_element(&ElementReference::new("source", "2"))
                    .await?,
            ))
        })
        .await?;
    assert!(first.is_some());
    assert!(failed.is_none());
    owner.shutdown().await?;
    drop(owner);
    let mut component = factory.create_component_with_processing(
        ComponentId::try_new("processor")?,
        json!({"fail":false}),
        super::consumer_tests::scope(),
        provider.clone(),
    )?;
    component.start().await?;
    component.stop().await?;
    component.delete_owned_resources().await?;
    component.delete_owned_resources().await?;
    assert!(component.start().await.is_err());
    let owner = ComputationTransaction::try_new(
        provider
            .create_indexes("native-consumer", "processor")
            .await?,
    )?;
    let deleted = owner
        .run(async {
            Ok(owner
                .resources()
                .indexes()
                .element_index
                .get_element(&ElementReference::new("source", "1"))
                .await?)
        })
        .await?;
    assert!(deleted.is_none());
    owner.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_linked_plugin_uses_host_transaction() -> anyhow::Result<()> {
    let plugin = NativePlugin::from_definition(sdk::PluginDefinition::new(
        "processing-fixture",
        "1",
        vec![Arc::new(super::processing_fixture::Factory)],
        vec![GraphChangeCodec::schema()],
    )?)?;
    loaded_plugin_case(&plugin).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_indexed_processing_separately_loaded_plugin_uses_host_transaction(
) -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    loaded_plugin_case(&plugin).await
}

struct OneSource {
    descriptor: ComponentDescriptor,
    envelope: Option<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for OneSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for OneSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.envelope.take().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("fixture port"),
            envelope,
            execution_context: Default::default(),
        }))
    }
}

struct OneSink {
    descriptor: ComponentDescriptor,
    received: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for OneSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for OneSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.received.notify_one();
        Ok(())
    }
}

async fn graph_case() -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "fixture/indexed-processing"
        })
        .expect("processing fixture")
        .clone();
    let directory = tempfile::tempdir()?;
    let provider = super::consumer_tests::provider(directory.path())?;
    let processor = ComponentId::try_new("processor")?;
    let source = ComponentId::try_new("source")?;
    let sink = ComponentId::try_new("sink")?;
    let mut specification = factory.specification(processor.clone(), json!({"fail":false}))?;
    assert!(factory.validate(&specification).is_err());
    let resource = ResourceId::try_new("indexes")?;
    specification
        .dependencies
        .insert("processing".into(), vec![resource.clone()]);
    factory.validate(&specification)?;
    let received = Arc::new(Notify::new());
    let mut graph = ComputationGraph::builder("native-consumer")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "processing".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(NativeProcessingResource::new(provider.clone())),
            ),
        )?
        .source(
            ComponentName::try_new("source")?,
            Box::new(OneSource {
                descriptor: ComponentDescriptor::try_new(
                    source.clone(),
                    vec![port(PortDirection::Output)],
                )?,
                envelope: Some(envelope(&source, 1)),
            }),
        )
        .component(ComponentName::try_new("processor")?, specification, factory)
        .sink(
            ComponentName::try_new("sink")?,
            Box::new(OneSink {
                descriptor: ComponentDescriptor::try_new(
                    sink.clone(),
                    vec![port(PortDirection::Input)],
                )?,
                received: received.clone(),
            }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source.clone(), PortId::try_new("out")?),
                Endpoint::new(processor.clone(), PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(processor.clone(), PortId::try_new("out")?),
                Endpoint::new(sink, PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(
            Endpoint::new(source, PortId::try_new("out")?),
            StreamId::try_new("source/out")?,
        )
        .bind_stream(
            Endpoint::new(processor, PortId::try_new("out")?),
            StreamId::try_new("processor/out")?,
        )
        .build()?;
    let run_result = tokio::time::timeout(Duration::from_secs(10), graph.start()?).await;
    if run_result.is_err() {
        let observed = graph.observed();
        tokio::time::timeout(Duration::from_secs(10), graph.shutdown()).await??;
        anyhow::bail!("native processing graph did not finish: {observed:?}");
    }
    run_result??;
    tokio::time::timeout(Duration::from_secs(10), received.notified()).await?;
    graph.shutdown().await?;
    let owner = ComputationTransaction::try_new(
        provider
            .create_indexes("native-consumer", "processor")
            .await?,
    )?;
    let stored = owner
        .run(async {
            Ok(owner
                .resources()
                .indexes()
                .element_index
                .get_element(&ElementReference::new("source", "1"))
                .await?)
        })
        .await?;
    assert!(stored.is_some());
    owner.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_graph_resolves_real_provider_current_thread(
) -> anyhow::Result<()> {
    graph_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_indexed_processing_graph_resolves_real_provider_multi_thread() -> anyhow::Result<()>
{
    graph_case().await
}
