// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_computation_contracts::{ConsumerProgressProvider, SourceProgressSnapshot};
use drasi_computation_plugin_sdk::{Factory, NativeComponent, NativeSource};
use drasi_core_types::interface::SourceCheckpoint;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use tokio::sync::watch;

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).unwrap()
}
fn stream() -> StreamId {
    StreamId::try_new("changes").unwrap()
}
fn members() -> BTreeSet<ComponentId> {
    BTreeSet::from([id("left"), id("right")])
}

fn position(lsn: u64, sequence: u64, offset: usize) -> RowPosition {
    RowPosition {
        version: 1,
        binding: [1; 32],
        commit_lsn: lsn,
        previous_lsn: lsn - 10,
        offset: Some(offset),
        count: 3,
        sequence,
        managed_slot: None,
    }
}

fn state(position: Option<RowPosition>) -> Arc<SourceProgressSnapshot> {
    Arc::new(SourceProgressSnapshot {
        ready: true,
        recovered: true,
        admitting: true,
        checkpoints: position
            .into_iter()
            .map(|position| {
                (
                    SourceProgressKey::Source("source".into()),
                    SourceCheckpoint::new(
                        position.sequence,
                        Some(serde_json::to_vec(&position).unwrap().into()),
                    ),
                )
            })
            .collect(),
        ..Default::default()
    })
}

fn snapshot(left: Option<RowPosition>, right: Option<RowPosition>) -> ConsumerProgressSnapshot {
    ConsumerProgressSnapshot {
        consumers: BTreeMap::from([(id("left"), state(left)), (id("right"), state(right))]),
    }
}

struct Provider {
    source: ComponentId,
    allowed: AtomicBool,
    sender: watch::Sender<ConsumerProgressSnapshot>,
}

impl ConsumerProgressProvider for Provider {
    fn graph_id(&self) -> &str {
        "graph"
    }
    fn producer_id(&self) -> &ComponentId {
        &self.source
    }
    fn snapshot(&self) -> Result<ConsumerProgressSnapshot> {
        assert!(
            self.allowed.load(AtomicOrdering::Relaxed),
            "constructor read progress"
        );
        Ok(self.sender.borrow().clone())
    }
    fn subscribe(&self) -> Result<Box<dyn ConsumerProgressUpdates>> {
        assert!(
            self.allowed.load(AtomicOrdering::Relaxed),
            "constructor subscribed to progress"
        );
        Ok(Box::new(Updates(self.sender.subscribe())))
    }
}

struct Updates(watch::Receiver<ConsumerProgressSnapshot>);
#[async_trait]
impl ConsumerProgressUpdates for Updates {
    fn snapshot(&mut self) -> Result<ConsumerProgressSnapshot> {
        Ok(self.0.borrow_and_update().clone())
    }
    async fn changed(&mut self) -> Result<()> {
        Ok(self.0.changed().await?)
    }
}

fn provider(snapshot: ConsumerProgressSnapshot) -> Arc<Provider> {
    Arc::new(Provider {
        source: id("source"),
        allowed: AtomicBool::new(false),
        sender: watch::channel(snapshot).0,
    })
}

#[test]
fn constructor_retains_capability_without_reading_and_policy_identity_is_stable() -> Result<()> {
    let provider = provider(snapshot(None, None));
    let view = ConsumerProgressView::new(provider.clone());
    let config = serde_json::from_value(serde_json::json!({
        "connection":{"database":"database","user":"user"},
        "start_lsn":"0/10", "transactions":{"max_changes":16,"max_bytes":4096,"max_duration_ms":1000},
        "max_protocol_bytes":4096,"io_timeout_ms":1000,"feedback_interval_ms":100
    }))?;
    let source = crate::PostgresTransactionSource::shared_rows(
        id("source"),
        stream(),
        config,
        view.clone(),
        &[id("left"), id("right")],
        Default::default(),
    )?;
    assert!(source
        .consumer_progress_reader()
        .unwrap()
        .same_binding(&view));
    assert!(source.recovery_reader().is_none());
    assert!(source.consumer_replay_retention().is_none());
    assert!(Arc::ptr_eq(
        &source.input_position_policy().unwrap(),
        &source.input_position_policy().unwrap()
    ));
    assert!(source
        .configuration()?
        .get("coordinated_snapshot")
        .is_none());
    assert!(crate::PostgresSharedRowsFactory.supports_consumer_progress());
    assert!(!crate::PostgresSharedRowsFactory.supports_source_progress());
    assert!(!crate::PostgresSharedRowsFactory
        .metadata()
        .configuration
        .fields
        .contains_key("coordinated_snapshot"));
    assert!(SharedBinding::new(&id("wrong"), view.clone(), &[id("left")]).is_err());
    assert!(SharedBinding::new(&id("source"), view, &[id("left"), id("left")]).is_err());
    Ok(())
}

#[test]
fn policy_orders_validated_wal_positions_not_logical_counters() -> Result<()> {
    let compare = |event: &RowPosition, handled: &RowPosition| {
        RowPositionComparator.is_after(&serde_json::to_vec(event)?, &serde_json::to_vec(handled)?)
    };
    let old = position(100, 9_000, 1);
    let new = position(100, 2, 2);
    assert!(compare(&new, &old)?);
    assert!(!compare(&old, &new)?);
    let boundary = RowPosition::snapshot([1; 32], "managed".into(), 100);
    assert!(!compare(&new, &boundary)?);
    assert!(!compare(&boundary, &new)?);
    assert!(compare(&position(101, 1, 0), &boundary)?);
    let mut invalid = old.clone();
    invalid.binding = [2; 32];
    assert!(compare(&invalid, &old).is_err());
    invalid = old.clone();
    invalid.count += 1;
    assert!(compare(&invalid, &old).is_err());
    assert!(RowPositionComparator
        .is_after(b"invalid", b"invalid")
        .is_err());
    Ok(())
}

#[test]
fn observation_keeps_physical_minimum_and_every_member_counter_floor() -> Result<()> {
    let mut snapshot = snapshot(Some(position(100, 8_000, 1)), Some(position(200, 3, 0)));
    Arc::make_mut(snapshot.consumers.get_mut(&id("right")).unwrap())
        .transport_sequences
        .insert(stream(), 10_000);
    let observed = observe(
        &snapshot,
        &members(),
        &id("source"),
        &stream(),
        [1; 32],
        false,
    )?;
    let minimum = observed.minimum.unwrap();
    assert_eq!(minimum.consumer, id("left"));
    assert_eq!(minimum.position.sequence, 8_000);
    assert_eq!(minimum.feedback_lsn, 90);
    assert_eq!(observed.max_counter, 10_000);
    assert_eq!(observed.max_lsn, 200);
    assert!(observe(
        &snapshot,
        &members(),
        &id("source"),
        &stream(),
        [1; 32],
        true
    )
    .is_err());
    Arc::make_mut(snapshot.consumers.get_mut(&id("right")).unwrap()).ready = false;
    let fenced = observe(
        &snapshot,
        &members(),
        &id("source"),
        &stream(),
        [1; 32],
        false,
    )?;
    assert!(!fenced.ready);
    assert!(fenced.minimum.is_none());
    snapshot.consumers.remove(&id("right"));
    assert!(observe(
        &snapshot,
        &members(),
        &id("source"),
        &stream(),
        [1; 32],
        false
    )
    .is_err());
    Ok(())
}

#[test]
fn mixed_or_bootstrapped_missing_initialization_is_not_a_new_streaming_start() -> Result<()> {
    let fresh = |snapshot: &ConsumerProgressSnapshot, origin| {
        fresh_group(
            snapshot,
            &members(),
            &id("source"),
            &stream(),
            false,
            origin,
        )
    };
    assert!(fresh(&snapshot(None, None), None)?);
    let mixed = snapshot(None, Some(position(100, 2, 2)));
    assert!(fresh(&mixed, None).is_err());
    assert!(fresh(&mixed, Some(50))?);
    assert!(!fresh(
        &snapshot(Some(position(100, 2, 2)), Some(position(200, 1, 1))),
        None
    )?);
    let mut missing = snapshot(None, None);
    Arc::make_mut(missing.consumers.get_mut(&id("right")).unwrap()).bootstrap_complete = true;
    assert!(fresh(&missing, None).is_err());
    assert!(fresh(&missing, Some(50)).is_err());
    missing = snapshot(None, None);
    Arc::make_mut(missing.consumers.get_mut(&id("right")).unwrap())
        .transport_sequences
        .insert(stream(), 1);
    assert!(fresh(&missing, None).is_err());
    let managed = RowPosition::snapshot([1; 32], "managed".into(), 100);
    assert!(fresh(&snapshot(Some(managed.clone()), Some(managed)), None).is_err());
    Ok(())
}

#[test]
fn an_unready_subscription_cannot_silently_resume_without_an_ordered_restart() -> Result<()> {
    let initial = snapshot(Some(position(100, 1, 2)), Some(position(150, 2, 2)));
    let provider = provider(initial.clone());
    let binding = SharedBinding::new(
        &id("source"),
        ConsumerProgressView::new(provider.clone()),
        &[id("left"), id("right")],
    )?;
    provider.allowed.store(true, AtomicOrdering::Relaxed);
    let mut updates = SharedUpdates::new(&binding, &initial, &id("source"), [1; 32], None)?;
    let mut unready = initial.clone();
    Arc::make_mut(unready.consumers.get_mut(&id("left")).unwrap()).ready = false;
    provider.sender.send_replace(unready);
    let observed = updates.snapshot(&id("source"), &stream(), [1; 32], false)?;
    assert!(!observed.ready && observed.minimum.is_none());
    provider.sender.send_replace(initial.clone());
    assert!(updates
        .snapshot(&id("source"), &stream(), [1; 32], false)
        .is_err());
    drop(updates);
    let mut restarted = SharedUpdates::new(&binding, &initial, &id("source"), [1; 32], None)?;
    assert!(
        restarted
            .snapshot(&id("source"), &stream(), [1; 32], false)?
            .ready
    );
    Ok(())
}

#[test]
fn only_a_completed_configured_snapshot_can_reenroll_a_reset_member_after_stop() -> Result<()> {
    let initial = snapshot(Some(position(100, 1, 2)), Some(position(150, 2, 2)));
    let provider = provider(initial.clone());
    let binding = SharedBinding::new(
        &id("source"),
        ConsumerProgressView::new(provider.clone()),
        &[id("left"), id("right")],
    )?;
    provider.allowed.store(true, AtomicOrdering::Relaxed);
    let mut updates = SharedUpdates::new(&binding, &initial, &id("source"), [1; 32], None)?;
    let mut renewed = snapshot(
        Some(RowPosition::configured_snapshot([1; 32], 200)),
        Some(position(150, 2, 2)),
    );
    Arc::make_mut(renewed.consumers.get_mut(&id("left")).unwrap()).reset_generation = 1;
    provider.sender.send_replace(renewed.clone());
    assert!(updates
        .snapshot(&id("source"), &stream(), [1; 32], false)
        .is_err());
    assert!(binding.retained_origin(&renewed, &id("source")).is_err());
    Arc::make_mut(renewed.consumers.get_mut(&id("left")).unwrap()).bootstrap_complete = true;
    provider.sender.send_replace(renewed.clone());
    assert!(
        updates
            .snapshot(&id("source"), &stream(), [1; 32], false)
            .is_err(),
        "an active reader must not adopt a new snapshot"
    );
    drop(updates);
    assert_eq!(binding.retained_origin(&renewed, &id("source"))?, None);
    let mut restarted = SharedUpdates::new(&binding, &renewed, &id("source"), [1; 32], None)?;
    let minimum = restarted
        .snapshot(&id("source"), &stream(), [1; 32], false)?
        .minimum
        .unwrap();
    assert_eq!(minimum.consumer, id("right"));
    assert_eq!(minimum.feedback_lsn, 150);
    Arc::make_mut(renewed.consumers.get_mut(&id("left")).unwrap()).reset_generation = 0;
    assert!(binding.retained_origin(&renewed, &id("source")).is_err());
    Ok(())
}

#[test]
fn failed_reset_and_lost_members_are_fenced_across_source_restarts() -> Result<()> {
    let initial = snapshot(None, None);
    let provider = provider(initial.clone());
    let binding = SharedBinding::new(
        &id("source"),
        ConsumerProgressView::new(provider.clone()),
        &[id("left"), id("right")],
    )?;
    provider.allowed.store(true, AtomicOrdering::Relaxed);
    let mut updates = SharedUpdates::new(&binding, &initial, &id("source"), [1; 32], Some(50))?;
    let mut handled = snapshot(Some(position(100, 1, 2)), None);
    provider.sender.send_replace(handled.clone());
    assert!(updates
        .snapshot(&id("source"), &stream(), [1; 32], false)?
        .minimum
        .is_none());
    assert_eq!(binding.retained_origin(&handled, &id("source"))?, Some(50));
    provider.sender.send_replace(initial.clone());
    assert!(updates
        .snapshot(&id("source"), &stream(), [1; 32], false)
        .is_err());
    assert!(binding.retained_origin(&initial, &id("source")).is_err());
    Arc::make_mut(handled.consumers.get_mut(&id("right")).unwrap()).reset_generation = 1;
    provider.sender.send_replace(handled.clone());
    assert!(updates
        .snapshot(&id("source"), &stream(), [1; 32], false)
        .is_err());
    assert!(binding.retained_origin(&handled, &id("source")).is_err());
    assert!(SharedUpdates::new(&binding, &handled, &id("source"), [1; 32], Some(50)).is_err());
    assert!(SharedUpdates::new(&binding, &initial, &id("source"), [2; 32], Some(50)).is_err());
    let mut failed = snapshot(None, None);
    Arc::make_mut(failed.consumers.get_mut(&id("left")).unwrap()).failure =
        Some("owner failed".into());
    assert!(observe(
        &failed,
        &members(),
        &id("source"),
        &stream(),
        [1; 32],
        false
    )
    .is_err());
    Ok(())
}

fn transaction_snapshot(
    left: Option<SharedTransactionPosition>,
    right: Option<SharedTransactionPosition>,
) -> ConsumerProgressSnapshot {
    let state = |position: Option<SharedTransactionPosition>| {
        Arc::new(SourceProgressSnapshot {
            ready: true,
            recovered: true,
            admitting: true,
            persistent: true,
            checkpoints: position
                .into_iter()
                .map(|position| {
                    (
                        SourceProgressKey::Source("source".into()),
                        SourceCheckpoint::new(
                            position.sequence,
                            Some(serde_json::to_vec(&position).unwrap().into()),
                        ),
                    )
                })
                .collect(),
            ..Default::default()
        })
    };
    ConsumerProgressSnapshot {
        consumers: BTreeMap::from([(id("left"), state(left)), (id("right"), state(right))]),
    }
}

#[test]
fn transaction_binding_is_constructor_safe_and_has_no_single_owner_or_row_options() -> Result<()> {
    let provider = provider(transaction_snapshot(None, None));
    let reader = ConsumerProgressView::new(provider);
    let binding = SharedBinding::with_mode(
        &id("source"),
        reader.clone(),
        &[id("left"), id("right")],
        SharedMode::Transactions,
    )?;
    assert!(binding.reader.same_binding(&reader));
    assert_eq!(binding.expected, members());
    assert_eq!(
        binding.policy.key(),
        &SourceProgressKey::Source("source".into())
    );
    assert!(Arc::ptr_eq(&binding.policy, &binding.policy.clone()));
    let metadata = crate::PostgresSharedTransactionsFactory.metadata();
    assert_eq!(
        metadata.implementation.name.as_ref(),
        crate::POSTGRES_SHARED_TRANSACTIONS
    );
    assert_eq!(
        metadata.ports[0].schema(),
        drasi_computation_contracts::GraphChangeCodec::schema().descriptor()
    );
    assert_eq!(
        metadata
            .configuration
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["settings", "stream"]
    );
    assert!(crate::PostgresSharedTransactionsFactory.supports_consumer_progress());
    assert!(!crate::PostgresSharedTransactionsFactory.supports_source_progress());
    Ok(())
}

#[test]
fn transaction_observation_and_rewind_keep_physical_minimum_and_all_allocation_floors() -> Result<()>
{
    let mut snapshot = transaction_snapshot(
        Some(SharedTransactionPosition::commit([1; 32], 100, 9_000)?),
        Some(SharedTransactionPosition::configured_snapshot(
            [1; 32], 200,
        )?),
    );
    Arc::make_mut(snapshot.consumers.get_mut(&id("right")).unwrap())
        .transport_sequences
        .insert(stream(), 20_000);
    let observed = observe_transactions(&snapshot, &members(), &id("source"), &stream(), [1; 32])?;
    let minimum = observed.minimum.unwrap();
    assert_eq!(minimum.consumer, id("left"));
    assert_eq!(minimum.position.commit_lsn, 100);
    assert_eq!(observed.max_counter, 20_000);
    assert_eq!(observed.max_lsn, 200);
    let counters = Arc::new(SharedCounters::default());
    let transport = Arc::new(AtomicU64::default());
    let emission = TransactionEmission::new(
        counters.clone(),
        transport.clone(),
        observed.max_counter,
        observed.max_lsn,
    );
    let (bytes, sequence) = emission.position(300, [1; 32], None)?;
    assert_eq!(sequence, 20_001);
    let cursor: SharedTransactionPosition = serde_json::from_slice(&bytes)?;
    assert_eq!(cursor.commit_lsn, 300);
    assert_eq!(cursor.sequence, sequence);
    assert!(
        emission.validate_observation(sequence, 300).is_err(),
        "allocation is not admission"
    );
    emission.admit(300);
    emission.validate_observation(sequence, 300)?;
    assert!(emission.validate_observation(sequence + 1, 300).is_err());
    assert!(emission.validate_observation(sequence, 301).is_err());
    assert!(emission.position(300, [1; 32], Some("managed")).is_err());
    let replacement = TransactionEmission::new(counters.clone(), transport, 10, 100);
    assert!(replacement.position(300, [1; 32], None)?.1 > sequence);
    counters.logical.store(u64::MAX, AtomicOrdering::Relaxed);
    assert!(replacement.position(400, [1; 32], None).is_err());
    for consumer in ["left", "right"] {
        let mut invalid = snapshot.clone();
        Arc::make_mut(invalid.consumers.get_mut(&id(consumer)).unwrap()).persistent = false;
        assert!(
            observe_transactions(&invalid, &members(), &id("source"), &stream(), [1; 32]).is_err()
        );
    }
    Ok(())
}

#[test]
fn transaction_membership_readiness_failure_and_active_reset_are_independent_fences() -> Result<()>
{
    let initial = transaction_snapshot(
        Some(SharedTransactionPosition::commit([1; 32], 100, 9000)?),
        Some(SharedTransactionPosition::commit([1; 32], 150, 2)?),
    );
    let provider = provider(initial.clone());
    let binding = SharedBinding::with_mode(
        &id("source"),
        ConsumerProgressView::new(provider.clone()),
        &[id("left"), id("right")],
        SharedMode::Transactions,
    )?;
    provider.allowed.store(true, AtomicOrdering::Relaxed);
    let mut updates = SharedUpdates::new(&binding, &initial, &id("source"), [1; 32], None)?;
    let mut renewed = transaction_snapshot(
        Some(SharedTransactionPosition::configured_snapshot(
            [1; 32], 200,
        )?),
        Some(SharedTransactionPosition::commit([1; 32], 150, 2)?),
    );
    Arc::make_mut(renewed.consumers.get_mut(&id("left")).unwrap()).reset_generation = 1;
    provider.sender.send_replace(renewed.clone());
    assert!(updates
        .transaction_snapshot(&id("source"), &stream(), [1; 32])
        .is_err());
    assert!(binding.retained_origin(&renewed, &id("source")).is_err());
    Arc::make_mut(renewed.consumers.get_mut(&id("left")).unwrap()).bootstrap_complete = true;
    provider.sender.send_replace(renewed.clone());
    assert!(updates
        .transaction_snapshot(&id("source"), &stream(), [1; 32])
        .is_err());
    drop(updates);
    assert_eq!(binding.retained_origin(&renewed, &id("source"))?, None);
    let mut updates = SharedUpdates::new(&binding, &renewed, &id("source"), [1; 32], None)?;
    assert_eq!(
        updates
            .transaction_snapshot(&id("source"), &stream(), [1; 32])?
            .minimum
            .unwrap()
            .position
            .commit_lsn,
        150
    );
    for consumer in ["left", "right"] {
        let mut failed = renewed.clone();
        Arc::make_mut(failed.consumers.get_mut(&id(consumer)).unwrap()).failure =
            Some("failed".into());
        provider.sender.send_replace(failed);
        assert!(updates
            .transaction_snapshot(&id("source"), &stream(), [1; 32])
            .is_err());
        let mut missing = renewed.clone();
        missing.consumers.remove(&id(consumer));
        provider.sender.send_replace(missing);
        assert!(updates
            .transaction_snapshot(&id("source"), &stream(), [1; 32])
            .is_err());
    }
    let mut unready = renewed.clone();
    Arc::make_mut(unready.consumers.get_mut(&id("right")).unwrap()).ready = false;
    provider.sender.send_replace(unready);
    assert!(
        !updates
            .transaction_snapshot(&id("source"), &stream(), [1; 32])?
            .ready
    );
    provider.sender.send_replace(renewed);
    assert!(updates
        .transaction_snapshot(&id("source"), &stream(), [1; 32])
        .is_err());
    Ok(())
}
