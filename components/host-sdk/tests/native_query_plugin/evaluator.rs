// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{ElementIndex, ElementStream, IndexError, IndexJoin, IndexSet, StorageDurability},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Calls {
    setups: AtomicUsize,
    fail_setup: AtomicBool,
    fail_read: AtomicBool,
}

struct ObservedElements {
    inner: Arc<dyn ElementIndex>,
    calls: Arc<Calls>,
}

#[async_trait]
impl ElementIndex for ObservedElements {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        if self.calls.fail_read.swap(false, Ordering::AcqRel) {
            return Err(IndexError::other(std::io::Error::other(
                "injected evaluator read failure",
            )));
        }
        self.inner.get_element(reference).await
    }

    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.inner.set_element(element, slots).await
    }

    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.inner.delete_element(reference).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, reference).await
    }

    async fn get_slot_elements(&self, slot: usize) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements(slot).await
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_inbound(slot, reference)
            .await
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_outbound(slot, reference)
            .await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.inner.clear().await
    }

    async fn set_joins(&self, joins: &[IndexJoin]) -> Result<(), IndexError> {
        self.calls.setups.fetch_add(1, Ordering::AcqRel);
        if self.calls.fail_setup.swap(false, Ordering::AcqRel) {
            return Err(IndexError::other(std::io::Error::other(
                "injected evaluator setup failure",
            )));
        }
        self.inner.set_joins(joins).await
    }
}

struct ObservedProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    calls: Arc<Calls>,
}

#[async_trait]
impl ComputationIndexProvider for ObservedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &drasi_core_types::ComponentId,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        original
            .atomic_result_transaction()
            .map_err(IndexError::other)?;
        let set = original.indexes();
        let domain = TransactionDomain::new(set.session_control.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: Arc::new(ObservedElements {
                    inner: set.element_index.clone(),
                    calls: self.calls.clone(),
                }),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: set.session_control.clone(),
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original
                    .checkpoint_store()
                    .expect("actual checkpoint store")
                    .clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("actual outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("actual live results")
                    .clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(indexes
            .with_cleanup(original.cleanup().expect("actual cleanup owner").clone())
            .with_durability(original.durability()))
    }

    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }

    fn durability(&self) -> StorageDurability {
        self.inner.durability()
    }

    fn provider_dependency(&self) -> Option<&Arc<dyn ComputationIndexProvider>> {
        Some(&self.inner)
    }
}

fn query(
    bootstrap: bool,
    language: &str,
) -> Result<(tempfile::TempDir, NativeComponentProxy, Arc<Calls>)> {
    let directory = tempfile::tempdir()?;
    let calls = Arc::new(Calls::default());
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({
            "query": if bootstrap { TEXT } else { QUERY },
            "bootstrap": bootstrap,
            "language": language,
        }),
        scope(1),
        Arc::new(ObservedProvider {
            inner: super::super::provider(&directory.path().join("indexes"))?,
            calls: calls.clone(),
        }),
    )?;
    if bootstrap {
        let database = directory.path().join("source.sqlite");
        sqlite::create_database(&database)?;
        query.bind_bootstrap(Arc::new(NativeBootstrapResource(file_provider(
            &database, 1,
        )?)))?;
    }
    Ok((directory, query, calls))
}

fn live_item(amount: i64, effective_from: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "live"),
            labels: Arc::from([Arc::from("items")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(json!({"amount": amount})),
    }
}

async fn reused_across_bootstrap_live_and_restart() -> Result<()> {
    for language in ["cypher", "gql"] {
        let (_directory, mut query, calls) = query(true, language)?;
        assert_eq!(calls.setups.load(Ordering::Acquire), 0);
        query.initialize().await?;
        query.start().await?;
        assert_eq!(
            calls.setups.load(Ordering::Acquire),
            1,
            "Core evaluator setup must run once, not once per bootstrap row"
        );
        assert_eq!(total(&reader(&query)?.snapshot().await?)?, 30.0);
        assert!(query.continue_transform().await?.is_empty());
        for (sequence, change, before, after) in [
            (
                1,
                SourceChange::Insert {
                    element: live_item(10, 1),
                },
                30.0,
                40.0,
            ),
            (
                2,
                SourceChange::Replace {
                    element: live_item(12, 2),
                },
                40.0,
                42.0,
            ),
            (
                3,
                SourceChange::Delete {
                    metadata: live_item(12, 3).get_metadata().clone(),
                },
                42.0,
                30.0,
            ),
        ] {
            let output = query.transform(input(change, sequence)?).await?;
            aggregate(&output, Some(before), after, sequence)?;
            query.delivery_completed(&output).await?;
            assert_eq!(calls.setups.load(Ordering::Acquire), 1);
        }
        query.stop().await?;
        query.start().await?;
        assert_eq!(calls.setups.load(Ordering::Acquire), 2);
        assert_eq!(total(&reader(&query)?.snapshot().await?)?, 30.0);
        assert!(query.continue_transform().await?.is_empty());
        let output = query
            .transform(input(
                SourceChange::Insert {
                    element: live_item(5, 4),
                },
                4,
            )?)
            .await?;
        aggregate(&output, Some(30.0), 35.0, 4)?;
        query.delivery_completed(&output).await?;
        assert_eq!(calls.setups.load(Ordering::Acquire), 2);
        query.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_evaluator_bootstrap_live_and_restart_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(45),
        reused_across_bootstrap_live_and_restart(),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_evaluator_bootstrap_live_and_restart_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(45),
        reused_across_bootstrap_live_and_restart(),
    )
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn retained_evaluator_reused_for_prepared_bootstrap_reset() -> Result<()> {
    let (directory, mut original, calls) = query(true, "cypher")?;
    original.initialize().await?;
    original.start().await?;
    let producer = reader(&original)?
        .snapshot()
        .await?
        .cursor()
        .producer()
        .clone();
    original.stop().await?;
    drop(original);
    let database = directory.path().join("source.sqlite");
    rusqlite::Connection::open(&database)?.execute("UPDATE items SET amount=11 WHERE id=1", [])?;
    let gate = lifecycle::Gate::new(false);
    gate.release();
    gate.fresh_snapshot.store(true, Ordering::Release);
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({
            "query": TEXT,
            "bootstrap": true,
            "language": "cypher",
            "recovery": "auto_reset",
        }),
        scope(2),
        Arc::new(ObservedProvider {
            inner: super::super::provider(&directory.path().join("indexes"))?,
            calls: calls.clone(),
        }),
    )?;
    query.initialize().await?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(Arc::new(
        lifecycle::PausedProvider {
            inner: file_provider(&database, 2)?,
            gate,
        },
    ))))?;
    query.start().await?;
    assert_eq!(
        calls.setups.load(Ordering::Acquire),
        2,
        "prepared bootstrap refresh must reuse this activation's evaluator"
    );
    let snapshot = reader(&query)?.snapshot().await?;
    assert_eq!(total(&snapshot)?, 31.0);
    assert_ne!(snapshot.cursor().producer(), &producer);
    assert!(query.continue_transform().await?.is_empty());
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: live_item(5, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(31.0), 36.0, 1)?;
    query.delivery_completed(&output).await?;
    assert_eq!(calls.setups.load(Ordering::Acquire), 2);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_evaluator_recovers_after_setup_and_processing_failures() -> Result<()> {
    let (_directory, mut query, calls) = query(false, "cypher")?;
    calls.fail_setup.store(true, Ordering::Release);
    query.initialize().await?;
    query
        .start()
        .await
        .expect_err("injected Core setup failure must fail activation");
    query.stop().await?;
    query.start().await?;
    assert_eq!(calls.setups.load(Ordering::Acquire), 2);
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(0.0), 10.0, 1)?;
    query.delivery_completed(&output).await?;
    calls.fail_read.store(true, Ordering::Release);
    query
        .transform(input(
            SourceChange::Replace {
                element: item(99, 2),
            },
            2,
        )?)
        .await
        .expect_err("injected Core evaluation failure must fence the query");
    assert_eq!(calls.setups.load(Ordering::Acquire), 2);
    query.stop().await?;
    query.start().await?;
    assert_eq!(calls.setups.load(Ordering::Acquire), 3);
    assert!(query.continue_transform().await?.is_empty());
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(12, 2),
            },
            2,
        )?)
        .await?;
    aggregate(&output, Some(10.0), 12.0, 2)?;
    query.delivery_completed(&output).await?;
    assert_eq!(calls.setups.load(Ordering::Acquire), 3);
    query.stop().await?;
    Ok(())
}
