// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! End-to-end tests for checkpoint and recovery features.
//!
//! These tests exercise the **public `DrasiLib` API** (`DrasiLib::builder()`,
//! `start_query()`, `stop_query()`, `remove_query()`, etc.) with the real
//! RocksDB index backend to validate the full checkpoint/recovery stack.
//!
//! Compared to the unit tests in `checkpoint_tests.rs` (which test internal
//! `QueryManager` wiring), these tests verify that config propagation, index
//! factory routing, and component lifecycle work end-to-end.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serial_test::serial;
use tokio::sync::{mpsc, RwLock};

use crate::channels::events::SourceEventWrapper;
use crate::channels::{ChannelChangeReceiver, ComponentStatus, SubscriptionResponse};
use crate::config::{QueryConfig, SourceSubscriptionSettings};
use crate::context::SourceRuntimeContext;
use crate::indexes::{IndexBackendPlugin, StorageBackendRef};
use crate::sources::{Source, SourceBase, SourceBaseParams, SourceError};
use crate::{DrasiLib, Query, RecoveryPolicy};

use drasi_index_rocksdb::RocksDbIndexProvider;

// ============================================================================
// E2E test source
// ============================================================================

type BootstrapCompletion = Arc<
    tokio::sync::Mutex<
        Option<tokio::sync::oneshot::Sender<anyhow::Result<crate::bootstrap::BootstrapResult>>>,
    >,
>;

mod durable_bootstrap_tests {
    use super::*;
    use drasi_core::interface::{
        CheckpointStore, CreatedIndexes, IndexError, LiveResultsWriter, RowMutation,
        SessionControl, SourceCheckpoint,
    };

    #[derive(Default)]
    struct Fault {
        stage: Arc<AtomicU32>,
        checkpoint_calls: AtomicU32,
        rollbacks: AtomicU32,
    }

    impl Fault {
        fn check(&self, stage: u32) -> Result<(), IndexError> {
            if self.stage.load(Ordering::SeqCst) == stage {
                Err(IndexError::other(std::io::Error::other(format!(
                    "injected bootstrap failure {stage}"
                ))))
            } else {
                Ok(())
            }
        }
    }

    struct FaultSession {
        inner: Arc<dyn SessionControl>,
        fault: Arc<Fault>,
    }

    #[async_trait]
    impl SessionControl for FaultSession {
        async fn begin(&self) -> Result<(), IndexError> {
            self.fault.check(1)?;
            self.inner.begin().await
        }
        async fn commit(&self) -> Result<(), IndexError> {
            self.fault.check(3)?;
            self.inner.commit().await
        }
        fn rollback(&self) -> Result<(), IndexError> {
            self.fault.rollbacks.fetch_add(1, Ordering::SeqCst);
            self.inner.rollback()
        }
    }

    struct FaultStore {
        inner: Arc<dyn CheckpointStore>,
        fault: Arc<Fault>,
    }

    #[async_trait]
    impl CheckpointStore for FaultStore {
        fn is_persistent(&self) -> bool {
            true
        }
        async fn stage_checkpoint(
            &self,
            source: &str,
            sequence: u64,
            position: Option<&Bytes>,
        ) -> Result<(), IndexError> {
            if self.fault.stage.load(Ordering::SeqCst) == 2
                && self.fault.checkpoint_calls.fetch_add(1, Ordering::SeqCst) == 1
            {
                self.fault.check(2)?;
            }
            self.inner
                .stage_checkpoint(source, sequence, position)
                .await
        }
        async fn read_checkpoint(
            &self,
            source: &str,
        ) -> Result<Option<SourceCheckpoint>, IndexError> {
            self.inner.read_checkpoint(source).await
        }
        async fn read_all_checkpoints(
            &self,
        ) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
            self.inner.read_all_checkpoints().await
        }
        async fn clear_checkpoints(&self) -> Result<(), IndexError> {
            self.inner.clear_checkpoints().await
        }
        async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
            self.inner.write_config_hash(hash).await
        }
        async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
            self.inner.read_config_hash().await
        }
        async fn stage_result_sequence(
            &self,
            query: &str,
            sequence: u64,
        ) -> Result<(), IndexError> {
            self.fault.check(5)?;
            self.inner.stage_result_sequence(query, sequence).await
        }
        async fn write_result_sequence(
            &self,
            query: &str,
            sequence: u64,
        ) -> Result<(), IndexError> {
            self.inner.write_result_sequence(query, sequence).await
        }
        async fn read_result_sequence(&self, query: &str) -> Result<Option<u64>, IndexError> {
            self.inner.read_result_sequence(query).await
        }
        async fn write_output_generation(
            &self,
            query: &str,
            generation: u64,
        ) -> Result<(), IndexError> {
            self.inner.write_output_generation(query, generation).await
        }
        async fn read_output_generation(&self, query: &str) -> Result<Option<u64>, IndexError> {
            self.inner.read_output_generation(query).await
        }
    }

    struct FaultProvider {
        inner: RocksDbIndexProvider,
        fault: Arc<Fault>,
    }

    struct FaultSnapshot {
        inner: Arc<dyn LiveResultsWriter>,
        fault: Arc<Fault>,
    }

    #[async_trait]
    impl LiveResultsWriter for FaultSnapshot {
        async fn apply_mutations(
            &self,
            query: &str,
            mutations: &[RowMutation<'_>],
        ) -> Result<(), IndexError> {
            self.fault.check(9)?;
            self.inner.apply_mutations(query, mutations).await
        }
        async fn read_snapshot(&self, query: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
            self.inner.read_snapshot(query).await
        }
        async fn clear(&self, query: &str) -> Result<(), IndexError> {
            self.inner.clear(query).await
        }
        async fn row_count(&self, query: &str) -> Result<usize, IndexError> {
            self.inner.row_count(query).await
        }
    }

    #[async_trait]
    impl IndexBackendPlugin for FaultProvider {
        async fn create_indexes(&self, query: &str) -> Result<CreatedIndexes, IndexError> {
            let mut indexes = self.inner.create_indexes(query).await?;
            indexes.set.session_control = Arc::new(FaultSession {
                inner: indexes.set.session_control,
                fault: self.fault.clone(),
            });
            indexes.checkpoint_store = Some(Arc::new(FaultStore {
                inner: indexes.checkpoint_store.unwrap(),
                fault: self.fault.clone(),
            }));
            indexes.live_results_writer = Some(Arc::new(FaultSnapshot {
                inner: indexes.live_results_writer.unwrap(),
                fault: self.fault.clone(),
            }));
            Ok(indexes)
        }
        fn is_volatile(&self) -> bool {
            false
        }
    }

    async fn wait_for_bootstrap_rows(query: &Arc<dyn super::super::manager::Query>, count: usize) {
        let query = query
            .as_any()
            .downcast_ref::<super::super::manager::DrasiQuery>()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while query.get_current_results().await.len() != count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bootstrap index writes must finish before injecting the fault");
    }

    #[rstest::rstest]
    #[case::begin(1)]
    #[case::second_checkpoint(2)]
    #[case::commit(3)]
    #[case::result_sequence(5)]
    #[case::missing_boundary(6)]
    #[case::provider_failure(7)]
    #[case::dropped_provider(8)]
    #[case::snapshot_write(9)]
    #[tokio::test]
    #[serial]
    async fn test_bootstrap_handover_failure_cancels_tail_and_rolls_back(#[case] stage: u32) {
        let directory = tempfile::tempdir_in(".").unwrap();
        let fault = Arc::new(Fault::default());
        let provider = Arc::new(FaultProvider {
            inner: RocksDbIndexProvider::new(directory.path(), false, false),
            fault: fault.clone(),
        });
        let core = DrasiLib::builder()
            .with_index_provider("persistent", provider)
            .build()
            .await
            .unwrap();
        let mut completions = Vec::new();
        let mut senders = Vec::new();
        for id in ["left", "right"] {
            let mut source = E2eTestSource::new(id, true).unwrap();
            source.bootstrap = true;
            let completion = Arc::new(tokio::sync::Mutex::new(None));
            source.bootstrap_completion = Some(completion.clone());
            completions.push(completion);
            senders.push(source.event_sender());
            core.add_source(source).await.unwrap();
        }
        core.add_query(
            Query::cypher("handover")
                .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
                .from_source("left")
                .from_source("right")
                .enable_bootstrap(true)
                .auto_start(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .build(),
        )
        .await
        .unwrap();
        core.start().await.unwrap();
        for id in ["left", "right"] {
            core.start_source(id).await.unwrap();
        }
        core.start_query("handover").await.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("handover")
            .await
            .unwrap();
        wait_for_bootstrap_rows(&query, 2).await;
        // The tail is already queued when handover fails.
        send_event(&senders[0], "left", 1, b"live-1").await;
        fault.stage.store(stage, Ordering::SeqCst);
        for (completion, id) in completions.iter().zip(["left", "right"]) {
            if (7..=8).contains(&stage) {
                if id == "left" {
                    let sender = completion.lock().await.take().unwrap();
                    if stage == 7 {
                        sender
                            .send(Err(anyhow::anyhow!("injected provider failure")))
                            .unwrap();
                    }
                }
                // The other provider never finishes; the supervisor must cancel it.
                continue;
            }
            completion
                .lock()
                .await
                .take()
                .unwrap()
                .send(Ok(crate::bootstrap::BootstrapResult {
                    event_count: 1,
                    source_position: (stage != 6).then(|| Bytes::from(format!("snapshot-{id}"))),
                }))
                .unwrap();
        }
        wait_for_status(&core, "handover", ComponentStatus::Error).await;
        assert!(query.fetch_snapshot().await.is_err());
        let concrete = query
            .as_any()
            .downcast_ref::<super::super::manager::DrasiQuery>()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while concrete.subscription_task_count().await != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let store = concrete.get_checkpoint_store().await.unwrap();
        assert_eq!(store.read_result_sequence("handover").await.unwrap(), None);
        assert!(
            store.read_all_checkpoints().await.unwrap().is_empty(),
            "all sources must roll back together"
        );
        assert_eq!(
            concrete.get_current_results().await.len(),
            2,
            "tail must not run after failure"
        );
        if (2..=5).contains(&stage) || stage == 9 {
            assert_eq!(fault.rollbacks.load(Ordering::SeqCst), 1);
        }
        for sender in &senders {
            assert!(
                sender.read().await.as_ref().unwrap().is_closed(),
                "forwarders must be cancelled"
            );
        }
        fault.stage.store(0, Ordering::SeqCst);
        core.shutdown().await.unwrap();
        drop(store);
        drop(query);
        drop(core);
        let reopened = RocksDbIndexProvider::new(directory.path(), false, false)
            .create_indexes("handover")
            .await
            .unwrap();
        assert_eq!(
            reopened
                .checkpoint_store
                .as_ref()
                .unwrap()
                .read_result_sequence("handover")
                .await
                .unwrap(),
            None
        );
        assert!(reopened
            .live_results_writer
            .as_ref()
            .unwrap()
            .read_snapshot("handover")
            .await
            .unwrap()
            .is_empty());
        assert!(reopened
            .outbox_writer
            .as_ref()
            .unwrap()
            .read_from("handover", 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[derive(Clone, Copy)]
    enum ReplacementCause {
        ConfigChange,
        IncompleteBootstrap,
        SourceGap,
    }

    #[rstest::rstest]
    #[case::config_change_strict(ReplacementCause::ConfigChange, RecoveryPolicy::Strict)]
    #[case::config_change_auto_reset(ReplacementCause::ConfigChange, RecoveryPolicy::AutoReset)]
    #[case::incomplete_strict(ReplacementCause::IncompleteBootstrap, RecoveryPolicy::Strict)]
    #[case::incomplete_auto_reset(ReplacementCause::IncompleteBootstrap, RecoveryPolicy::AutoReset)]
    #[case::source_gap_strict(ReplacementCause::SourceGap, RecoveryPolicy::Strict)]
    #[case::source_gap_auto_reset(ReplacementCause::SourceGap, RecoveryPolicy::AutoReset)]
    #[tokio::test]
    #[serial]
    async fn interrupted_replacement_bootstrap_obeys_recovery_policy(
        #[case] cause: ReplacementCause,
        #[case] policy: RecoveryPolicy,
    ) {
        let directory = tempfile::tempdir_in(".").unwrap();
        let config = |original: bool, policy| {
            Query::cypher("replacement")
                .query(
                    if original && matches!(cause, ReplacementCause::ConfigChange) {
                        "MATCH (n:Node) RETURN count(n) AS previous_count"
                    } else {
                        "MATCH (n:Node) RETURN count(n) AS count"
                    },
                )
                .from_source("source")
                .enable_bootstrap(true)
                .auto_start(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .with_recovery_policy(policy)
                .build()
        };
        for original in [true, false] {
            let core = build_e2e_lib("replacement-instance", &directory, None)
                .await
                .unwrap();
            let mut source = E2eTestSource::new("source", true).unwrap();
            source.bootstrap = true;
            source.bootstrap_node_id = if original { "original" } else { "interrupted" };
            let incomplete = !original || matches!(cause, ReplacementCause::IncompleteBootstrap);
            if incomplete {
                source.bootstrap_completion = Some(Arc::new(tokio::sync::Mutex::new(None)));
            }
            if !original && matches!(cause, ReplacementCause::SourceGap) {
                source.set_fail_count(1);
            }
            core.add_source(source).await.unwrap();
            core.add_query(config(original, RecoveryPolicy::AutoReset))
                .await
                .unwrap();
            core.start().await.unwrap();
            core.start_source("source").await.unwrap();
            core.start_query("replacement").await.unwrap();
            let query = core
                .query_manager()
                .get_query_instance("replacement")
                .await
                .unwrap();
            if incomplete {
                wait_for_bootstrap_rows(&query, 1).await;
            } else {
                query.fetch_snapshot().await.unwrap();
            }
            core.shutdown().await.unwrap();
        }

        let core = build_e2e_lib("replacement-instance", &directory, None)
            .await
            .unwrap();
        let mut source = E2eTestSource::new("source", true).unwrap();
        source.bootstrap = true;
        source.bootstrap_node_id = "complete";
        let subscriptions = source.subscribe_count_handle();
        let resumed = source.last_resume_sequence();
        core.add_source(source).await.unwrap();
        core.add_query(config(false, policy)).await.unwrap();
        core.start().await.unwrap();
        core.start_source("source").await.unwrap();
        let started = core.start_query("replacement").await;
        match policy {
            RecoveryPolicy::Strict => {
                assert!(
                    format!("{:#}", started.unwrap_err()).contains("incomplete durable bootstrap")
                );
                assert_eq!(subscriptions.load(Ordering::SeqCst), 0);
            }
            RecoveryPolicy::AutoReset => {
                started.unwrap();
                let query = core
                    .query_manager()
                    .get_query_instance("replacement")
                    .await
                    .unwrap();
                let snapshot = query.fetch_snapshot().await.unwrap();
                assert_eq!(
                    snapshot.to_vec(),
                    vec![serde_json::json!({"count": 1})],
                    "replacement must discard partially populated indexes"
                );
                assert_eq!(snapshot.output_generation, 2);
                assert_eq!(*resumed.read().await, None);
                let store = query
                    .as_any()
                    .downcast_ref::<super::super::manager::DrasiQuery>()
                    .unwrap()
                    .get_checkpoint_store()
                    .await
                    .unwrap();
                assert_eq!(
                    store.read_config_hash().await.unwrap(),
                    Some(super::super::compute_config_hash(&config(false, policy)))
                );
            }
        }
        core.shutdown().await.unwrap();
    }

    #[rstest::rstest]
    #[case::strict(RecoveryPolicy::Strict, None)]
    #[case::auto_reset(RecoveryPolicy::AutoReset, None)]
    #[case::legacy_partial_strict(RecoveryPolicy::Strict, Some(false))]
    #[case::legacy_partial_auto_reset(RecoveryPolicy::AutoReset, Some(false))]
    #[case::legacy_checkpoint_strict(RecoveryPolicy::Strict, Some(true))]
    #[case::legacy_checkpoint_auto_reset(RecoveryPolicy::AutoReset, Some(true))]
    #[tokio::test]
    #[serial]
    async fn test_bootstrap_partial_restart_obeys_recovery_policy(
        #[case] policy: RecoveryPolicy,
        #[case] legacy_checkpoint: Option<bool>,
    ) {
        let directory = tempfile::tempdir_in(".").unwrap();
        let config = || {
            Query::cypher("partial")
                .query("MATCH (n:Node) RETURN count(n) AS count")
                .from_source("source")
                .enable_bootstrap(true)
                .auto_start(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .with_recovery_policy(policy)
                .build()
        };
        {
            let core = build_e2e_lib("partial-instance", &directory, None)
                .await
                .unwrap();
            let mut source = E2eTestSource::new("source", true).unwrap();
            source.bootstrap = true;
            source.bootstrap_node_id = "orphan-from-interrupted-snapshot";
            source.bootstrap_completion = Some(Arc::new(tokio::sync::Mutex::new(None)));
            core.add_source(source).await.unwrap();
            core.add_query(config()).await.unwrap();
            core.start().await.unwrap();
            core.start_source("source").await.unwrap();
            core.start_query("partial").await.unwrap();
            let query = core
                .query_manager()
                .get_query_instance("partial")
                .await
                .unwrap();
            wait_for_bootstrap_rows(&query, 1).await;
            // Stop while the provider is still incomplete. This leaves the exact
            // durable state of a crash after an indexed bootstrap row commit.
            core.shutdown().await.unwrap();
        }
        if let Some(with_checkpoint) = legacy_checkpoint {
            let indexes = RocksDbIndexProvider::new(directory.path(), false, false)
                .create_indexes("partial")
                .await
                .unwrap();
            let store = indexes.checkpoint_store.as_ref().unwrap();
            let hash = store.read_config_hash().await.unwrap().unwrap();
            store.clear_checkpoints().await.unwrap();
            store.write_config_hash(hash).await.unwrap();
            if with_checkpoint {
                indexes.set.session_control.begin().await.unwrap();
                store
                    .stage_checkpoint("source", 0, Some(&Bytes::from_static(b"legacy-boundary")))
                    .await
                    .unwrap();
                indexes.set.session_control.commit().await.unwrap();
            }
        }
        let core = build_e2e_lib("partial-instance", &directory, None)
            .await
            .unwrap();
        let mut source = E2eTestSource::new("source", true).unwrap();
        source.bootstrap = true;
        let subscriptions = source.subscribe_count_handle();
        let resumed = source.last_resume_from();
        core.add_source(source).await.unwrap();
        core.add_query(config()).await.unwrap();
        core.start().await.unwrap();
        core.start_source("source").await.unwrap();
        let started = core.start_query("partial").await;
        match policy {
            RecoveryPolicy::Strict => {
                let expected = if legacy_checkpoint.is_some() {
                    "legacy bootstrap"
                } else {
                    "incomplete durable bootstrap"
                };
                assert!(format!("{:#}", started.unwrap_err()).contains(expected));
                assert_eq!(subscriptions.load(Ordering::SeqCst), 0);
            }
            RecoveryPolicy::AutoReset => {
                started.unwrap();
                let query = core
                    .query_manager()
                    .get_query_instance("partial")
                    .await
                    .unwrap();
                let snapshot = query.fetch_snapshot().await.unwrap();
                assert_eq!(
                    snapshot.to_vec(),
                    vec![serde_json::json!({"count": 1})],
                    "both orphaned graph rows and aggregation state must be cleared"
                );
                assert_eq!(snapshot.output_generation, 1);
                assert_eq!(*resumed.read().await, None);
            }
        }
        core.shutdown().await.unwrap();
        drop(core);
        let indexes = RocksDbIndexProvider::new(directory.path(), false, false)
            .create_indexes("partial")
            .await
            .unwrap();
        let state = indexes
            .checkpoint_store
            .as_ref()
            .unwrap()
            .read_result_sequence("partial")
            .await
            .unwrap();
        assert_eq!(
            state,
            match policy {
                RecoveryPolicy::Strict => None,
                RecoveryPolicy::AutoReset => Some(0),
            }
        );
    }
}

struct E2eTestSource {
    base: SourceBase,
    replay_capable: bool,
    /// Records the most recent `resume_from` received in subscribe().
    last_resume_from: Arc<RwLock<Option<Bytes>>>,
    /// Records the most recent `resume_sequence` received in subscribe().
    last_resume_sequence: Arc<RwLock<Option<u64>>>,
    /// Number of times subscribe was called.
    subscribe_count: Arc<AtomicU32>,
    /// Number of remaining subscribe failures (decremented each call).
    remaining_failures: Arc<AtomicU32>,
    /// Number of times remove_position_handle was called.
    position_handle_removed: Arc<AtomicU32>,
    /// Sender for injecting events. Recreated in subscribe() for each subscription.
    event_tx: Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
    bootstrap: bool,
    bootstrap_completion: Option<BootstrapCompletion>,
    bootstrap_node_id: &'static str,
    bootstrap_empty: bool,
    bootstrap_sequence: u64,
}

impl E2eTestSource {
    fn new(id: &str, replay_capable: bool) -> anyhow::Result<Self> {
        Ok(Self {
            base: SourceBase::new(SourceBaseParams::new(id))?,
            replay_capable,
            last_resume_from: Arc::new(RwLock::new(None)),
            last_resume_sequence: Arc::new(RwLock::new(None)),
            subscribe_count: Arc::new(AtomicU32::new(0)),
            remaining_failures: Arc::new(AtomicU32::new(0)),
            position_handle_removed: Arc::new(AtomicU32::new(0)),
            event_tx: Arc::new(RwLock::new(None)),
            bootstrap: false,
            bootstrap_completion: None,
            bootstrap_node_id: "snapshot",
            bootstrap_empty: false,
            bootstrap_sequence: 0,
        })
    }

    fn last_resume_from(&self) -> Arc<RwLock<Option<Bytes>>> {
        self.last_resume_from.clone()
    }

    fn last_resume_sequence(&self) -> Arc<RwLock<Option<u64>>> {
        self.last_resume_sequence.clone()
    }

    fn subscribe_count_handle(&self) -> Arc<AtomicU32> {
        self.subscribe_count.clone()
    }

    fn position_handle_removed_handle(&self) -> Arc<AtomicU32> {
        self.position_handle_removed.clone()
    }

    fn remaining_failures_handle(&self) -> Arc<AtomicU32> {
        self.remaining_failures.clone()
    }

    fn event_sender(&self) -> Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>> {
        self.event_tx.clone()
    }

    fn set_fail_count(&self, count: u32) {
        self.remaining_failures.store(count, Ordering::Release);
    }
}

#[async_trait]
impl Source for E2eTestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "e2e-test"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn supports_replay(&self) -> bool {
        self.replay_capable
    }

    fn auto_start(&self) -> bool {
        false
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Starting, Some("Starting".into()))
            .await;
        self.base
            .set_status(ComponentStatus::Running, Some("Running".into()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("Stopping".into()))
            .await;
        self.base
            .set_status(ComponentStatus::Stopped, Some("Stopped".into()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.subscribe_count.fetch_add(1, Ordering::AcqRel);

        // Record what the query sent us
        *self.last_resume_from.write().await = settings.resume_from.clone();
        *self.last_resume_sequence.write().await = settings.resume_sequence;

        // Check for injected failures
        if self
            .remaining_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n > 0 {
                    Some(n - 1)
                } else {
                    None
                }
            })
            .is_ok()
        {
            return Err(SourceError::PositionUnavailable {
                source_id: self.id().to_string(),
                requested: settings.resume_from.unwrap_or_default(),
                earliest_available: None,
            }
            .into());
        }

        // Create a new channel pair for this subscription
        let (tx, rx) = mpsc::channel(1000);
        *self.event_tx.write().await = Some(tx);

        // Position handle
        let position_handle = if settings.request_position_handle {
            Some(Arc::new(AtomicU64::new(u64::MAX)))
        } else {
            None
        };

        let (bootstrap_receiver, bootstrap_result_receiver) = if self.bootstrap
            && settings.enable_bootstrap
            && settings.resume_from.is_none()
            && settings.resume_sequence.is_none()
        {
            use drasi_core::models::{
                Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
                SourceChange,
            };
            let (events, rx) = mpsc::channel(1);
            let (result, result_rx) = tokio::sync::oneshot::channel();
            let mut properties = ElementPropertyMap::new();
            properties.insert("id", ElementValue::String(self.id().into()));
            properties.insert("value", ElementValue::Integer(42));
            if !self.bootstrap_empty {
                events
                    .send(crate::channels::BootstrapEvent {
                        source_id: self.id().to_string(),
                        change: SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        self.id(),
                                        self.bootstrap_node_id,
                                    ),
                                    labels: Arc::from(vec![Arc::from("Node")]),
                                    effective_from: 0,
                                },
                                properties,
                            },
                        },
                        timestamp: chrono::Utc::now(),
                        sequence: self.bootstrap_sequence,
                    })
                    .await
                    .unwrap();
            }
            if let Some(completion) = &self.bootstrap_completion {
                *completion.lock().await = Some(result);
            } else {
                result
                    .send(Ok(crate::bootstrap::BootstrapResult {
                        event_count: usize::from(!self.bootstrap_empty),
                        source_position: Some(Bytes::from(format!("snapshot-{}", self.id()))),
                    }))
                    .unwrap();
            }
            (Some(rx), Some(result_rx))
        } else {
            (None, None)
        };

        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id().to_string(),
            receiver: Box::new(ChannelChangeReceiver::new(rx)),
            bootstrap_receiver,
            position_handle,
            bootstrap_result_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn remove_position_handle(&self, _query_id: &str) {
        self.position_handle_removed.fetch_add(1, Ordering::AcqRel);
    }
}

// ============================================================================
// Helper: build a DrasiLib with persistent plugin
// ============================================================================

async fn build_e2e_lib(
    id: &str,
    tmp_dir: &tempfile::TempDir,
    default_recovery_policy: Option<RecoveryPolicy>,
) -> anyhow::Result<Arc<DrasiLib>> {
    let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);
    let mut builder = DrasiLib::builder().with_id(id).with_index_provider(
        "persistent",
        Arc::new(provider) as Arc<dyn IndexBackendPlugin>,
    );

    if let Some(policy) = default_recovery_policy {
        builder = builder.with_default_recovery_policy(policy);
    }

    let lib = builder.build().await?;
    Ok(Arc::new(lib))
}

fn make_persistent_query(
    id: &str,
    source_id: &str,
    recovery_policy: Option<RecoveryPolicy>,
) -> QueryConfig {
    let mut q = Query::cypher(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()));

    if let Some(policy) = recovery_policy {
        q = q.with_recovery_policy(policy);
    }

    q.build()
}

fn make_volatile_query(id: &str, source_id: &str) -> QueryConfig {
    Query::cypher(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .build()
}

/// Send a source change event and wait briefly for processing.
async fn send_event(
    tx: &Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
    source_id: &str,
    sequence: u64,
    position: &[u8],
) {
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };

    let node_id = format!("node-{sequence}");
    let mut props = ElementPropertyMap::new();
    props.insert(
        "id",
        drasi_core::models::ElementValue::String(node_id.clone().into()),
    );
    props.insert(
        "value",
        drasi_core::models::ElementValue::Integer(sequence as i64),
    );

    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &node_id),
            labels: Arc::from(vec![Arc::from("Node")]),
            effective_from: 0,
        },
        properties: props,
    };

    let change = SourceChange::Insert { element };

    let event = SourceEventWrapper::new(
        source_id.to_string(),
        crate::channels::events::SourceEvent::Change(change),
        chrono::Utc::now(),
        sequence,
    )
    .with_source_position(Bytes::from(position.to_vec()));

    if let Some(sender) = tx.read().await.as_ref() {
        sender.send(Arc::new(event)).await.unwrap();
    }
    // Brief pause for processing
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

async fn wait_for_status(core: &DrasiLib, component_id: &str, expected: ComponentStatus) {
    let graph = core.component_graph();
    crate::component_graph::wait_for_status(
        &graph,
        component_id,
        &[expected],
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|e| panic!("wait_for_status({component_id}, {expected:?}) failed: {e}"));
}

async fn wait_for_outbox_sequence(
    core: &DrasiLib,
    query_id: &str,
    sequence: u64,
) -> super::OutboxResponse {
    let query = core
        .query_manager()
        .get_query_instance(query_id)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let outbox = query.fetch_outbox(0).await.unwrap();
            if outbox.latest_sequence >= sequence {
                return outbox;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("query must commit the expected output sequence")
}

// ============================================================================
// E2E Tests
// ============================================================================

#[rstest::rstest]
#[case::rows(false, 2, false)]
#[case::empty_snapshot(true, 2, false)]
#[case::mixed_sources(false, 1, false)]
#[case::no_bootstrap_providers(false, 0, false)]
#[case::replacement_rows(false, 2, true)]
#[case::replacement_empty_snapshot(true, 2, true)]
#[case::replacement_mixed_sources(false, 1, true)]
#[case::replacement_no_bootstrap_providers(false, 0, true)]
#[tokio::test]
#[serial]
async fn test_e2e_bootstrap_snapshot_reopens_without_live_events(
    #[case] empty: bool,
    #[case] bootstrap_sources: usize,
    #[case] replacement: bool,
) {
    let directory = tempfile::tempdir_in(".").unwrap();
    let last_run = if replacement { 2 } else { 1 };
    for run in 0..=last_run {
        let seed = replacement && run == 0;
        let restart = run == last_run;
        let core = build_e2e_lib("bootstrap-reopen", &directory, Some(RecoveryPolicy::Strict))
            .await
            .unwrap();
        let mut resumes = Vec::new();
        for (index, id) in ["left", "right"].iter().enumerate() {
            let mut source = E2eTestSource::new(id, true).unwrap();
            source.bootstrap = index < bootstrap_sources;
            source.bootstrap_empty = empty;
            resumes.push((source.last_resume_from(), source.last_resume_sequence()));
            core.add_source(source).await.unwrap();
        }
        let config = Query::cypher("bootstrap-q")
            .query(if seed {
                "MATCH (n:Node) RETURN n.id AS id, n.value AS previous_value"
            } else {
                "MATCH (n:Node) RETURN n.id AS id, n.value AS value"
            })
            .from_source("left")
            .from_source("right")
            .enable_bootstrap(true)
            .auto_start(false)
            .with_storage_backend(StorageBackendRef::Named("persistent".into()))
            .build();
        core.add_query(config).await.unwrap();
        core.start().await.unwrap();
        for id in ["left", "right"] {
            core.start_source(id).await.unwrap();
        }
        core.start_query("bootstrap-q").await.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.as_of_sequence, 0);
        if seed {
            core.shutdown().await.unwrap();
            continue;
        }
        assert_eq!(snapshot.output_generation, u64::from(replacement));
        let mut rows = snapshot.to_vec();
        rows.sort_by_key(|row| row["id"].as_str().unwrap().to_string());
        let expected = if empty {
            vec![]
        } else {
            vec![
                serde_json::json!({"id": "left", "value": 42}),
                serde_json::json!({"id": "right", "value": 42}),
            ]
            .into_iter()
            .take(bootstrap_sources)
            .collect()
        };
        assert_eq!(rows, expected, "restart={restart}");
        assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
        for (index, ((resume, sequence), id)) in resumes.iter().zip(["left", "right"]).enumerate() {
            assert_eq!(
                *resume.read().await,
                (restart && index < bootstrap_sources)
                    .then(|| Bytes::from(format!("snapshot-{id}")))
            );
            assert_eq!(*sequence.read().await, restart.then_some(0));
        }
        core.shutdown().await.unwrap();
    }
}

#[rstest::rstest]
#[case::cursor_only(false, RecoveryPolicy::Strict)]
#[case::sequence_only_strict(true, RecoveryPolicy::Strict)]
#[case::sequence_only_auto_reset(true, RecoveryPolicy::AutoReset)]
#[tokio::test]
#[serial]
async fn test_e2e_bootstrap_snapshot_reopens_after_filtered_event(
    #[case] sequence_only: bool,
    #[case] policy: RecoveryPolicy,
) {
    let directory = tempfile::tempdir_in(".").unwrap();
    for restart in [false, true] {
        let core = build_e2e_lib("bootstrap-filtered", &directory, Some(policy))
            .await
            .unwrap();
        let mut source = E2eTestSource::new("source", true).unwrap();
        source.bootstrap = true;
        let resumed_cursor = source.last_resume_from();
        let resumed_sequence = source.last_resume_sequence();
        let events = source.event_sender();
        core.add_source(source).await.unwrap();
        let mut config = Query::cypher("filtered")
            .query("MATCH (n:Node) WHERE n.value = 42 RETURN n.id AS id, n.value AS value")
            .from_source("source")
            .enable_bootstrap(true)
            .auto_start(false)
            .with_storage_backend(StorageBackendRef::Named("persistent".into()));
        let sequence_source = E2eTestSource::new("sequence-only", true).unwrap();
        let sequence_events = sequence_source.event_sender();
        let sequence_resumed = sequence_source.last_resume_sequence();
        if sequence_only {
            config = config.from_source("sequence-only");
            core.add_source(sequence_source).await.unwrap();
        }
        core.add_query(config.build()).await.unwrap();
        core.start().await.unwrap();
        core.start_source("source").await.unwrap();
        if sequence_only {
            core.start_source("sequence-only").await.unwrap();
        }
        core.start_query("filtered").await.unwrap();
        assert_eq!(
            *resumed_cursor.read().await,
            restart.then(|| Bytes::from_static(b"filtered-1"))
        );
        assert_eq!(*resumed_sequence.read().await, restart.then_some(1));
        if sequence_only {
            assert_eq!(*sequence_resumed.read().await, restart.then_some(7));
        }
        let query = core
            .query_manager()
            .get_query_instance("filtered")
            .await
            .unwrap();
        let store = query
            .as_any()
            .downcast_ref::<super::manager::DrasiQuery>()
            .unwrap()
            .get_checkpoint_store()
            .await
            .unwrap();
        if !restart {
            send_event(&events, "source", 1, b"filtered-1").await;
            if sequence_only {
                use drasi_core::models::{
                    Element, ElementMetadata, ElementReference, SourceChange,
                };
                let event = SourceEventWrapper::new(
                    "sequence-only".into(),
                    crate::channels::SourceEvent::Change(SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new("sequence-only", "filtered"),
                                labels: Arc::from(vec![Arc::from("Excluded")]),
                                effective_from: 0,
                            },
                            properties: Default::default(),
                        },
                    }),
                    chrono::Utc::now(),
                    7,
                );
                sequence_events
                    .read()
                    .await
                    .as_ref()
                    .unwrap()
                    .send(Arc::new(event))
                    .await
                    .unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let checkpoint = store.read_checkpoint("source").await.unwrap();
                    let sequence_checkpoint_ready = !sequence_only
                        || store
                            .read_checkpoint("sequence-only")
                            .await
                            .unwrap()
                            .is_some_and(|checkpoint| {
                                checkpoint.sequence == 7 && checkpoint.source_position.is_none()
                            });
                    if checkpoint.is_some_and(|checkpoint| {
                        checkpoint.sequence == 1
                            && checkpoint.source_position == Some(Bytes::from_static(b"filtered-1"))
                    }) && sequence_checkpoint_ready
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("filtered event must advance the durable input checkpoint");
        }
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.as_of_sequence, 0);
        assert_eq!(snapshot.output_generation, 0);
        assert_eq!(
            snapshot.to_vec(),
            vec![serde_json::json!({"id": "source", "value": 42})]
        );
        assert_eq!(
            store.read_result_sequence("filtered").await.unwrap(),
            Some(0)
        );
        assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
        if restart {
            send_event(&events, "source", 42, b"matching-42").await;
            let outbox = wait_for_outbox_sequence(&core, "filtered", 1).await;
            assert_eq!(outbox.latest_sequence, 1);
            let snapshot = query.fetch_snapshot().await.unwrap();
            assert_eq!(snapshot.as_of_sequence, 1);
            let rows = snapshot.to_vec();
            assert_eq!(rows.len(), 2);
            assert!(rows.contains(&serde_json::json!({"id": "source", "value": 42})));
            assert!(rows.contains(&serde_json::json!({"id": "node-42", "value": 42})));
        }
        core.shutdown().await.unwrap();
    }
}

#[tokio::test]
#[serial]
async fn bootstrap_and_live_sequences_remain_independent_of_source_cursor_on_restart() {
    let directory = tempfile::tempdir_in(".").unwrap();
    const BOOTSTRAP_SEQUENCE: u64 = 1_000_000;
    const FIRST_CURSOR: &[u8] = b"local-epoch:7";
    const SECOND_CURSOR: &[u8] = b"local-epoch:8";
    for restart in [false, true] {
        let core = build_e2e_lib(
            "sequence-instance",
            &directory,
            Some(RecoveryPolicy::Strict),
        )
        .await
        .unwrap();
        let mut source = E2eTestSource::new("cursor-source", true).unwrap();
        source.bootstrap = true;
        source.bootstrap_sequence = BOOTSTRAP_SEQUENCE;
        let resumed_cursor = source.last_resume_from();
        let resumed_sequence = source.last_resume_sequence();
        let events = source.event_sender();
        core.add_source(source).await.unwrap();
        let mut config = make_persistent_query("cursor-query", "cursor-source", None);
        config.enable_bootstrap = true;
        core.add_query(config).await.unwrap();
        core.start().await.unwrap();
        core.start_source("cursor-source").await.unwrap();
        core.start_query("cursor-query").await.unwrap();
        assert_eq!(
            *resumed_cursor.read().await,
            restart.then(|| Bytes::from_static(FIRST_CURSOR))
        );
        assert_eq!(
            *resumed_sequence.read().await,
            restart.then_some(BOOTSTRAP_SEQUENCE + 1)
        );
        let query = core
            .query_manager()
            .get_query_instance("cursor-query")
            .await
            .unwrap();
        if restart {
            let restored = query.fetch_snapshot().await.unwrap();
            assert_eq!(restored.as_of_sequence, 1);
            assert_eq!(restored.to_vec().len(), 2);
        }

        let result_sequence = if restart { 2 } else { 1 };
        let event_sequence = BOOTSTRAP_SEQUENCE + result_sequence;
        let cursor = if restart { SECOND_CURSOR } else { FIRST_CURSOR };
        // On the first start this arrives immediately, potentially while the
        // bootstrap gate is still closed.
        send_event(&events, "cursor-source", event_sequence, cursor).await;
        let outbox = wait_for_outbox_sequence(&core, "cursor-query", result_sequence).await;
        assert_eq!(outbox.latest_sequence, result_sequence);
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.as_of_sequence, result_sequence);
        let rows = snapshot.to_vec();
        assert_eq!(rows.len(), result_sequence as usize + 1);
        assert!(rows.contains(&serde_json::json!({"id": "cursor-source", "value": 42})));
        for live in 1..=result_sequence {
            assert!(rows.contains(&serde_json::json!({
                "id": format!("node-{}", BOOTSTRAP_SEQUENCE + live),
                "value": BOOTSTRAP_SEQUENCE + live,
            })));
        }
        let store = query
            .as_any()
            .downcast_ref::<super::manager::DrasiQuery>()
            .unwrap()
            .get_checkpoint_store()
            .await
            .unwrap();
        let checkpoint = store
            .read_checkpoint("cursor-source")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.sequence, event_sequence);
        assert_eq!(
            checkpoint.source_position,
            Some(Bytes::copy_from_slice(cursor))
        );
        core.shutdown().await.unwrap();
    }
}

#[rstest::rstest]
#[case::insert_only(1, false)]
#[case::named_update(2, false)]
#[case::malformed_legacy_update(2, true)]
#[tokio::test]
#[serial]
async fn test_e2e_outbox_persistent_reopen(#[case] sequence: u64, #[case] legacy_compact: bool) {
    use crate::channels::{QueryResult, ResultDiff, SourceEvent};
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use serde_json::json;

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("outbox-reopen", &tmp_dir, Some(RecoveryPolicy::Strict))
        .await
        .unwrap();
    let source = E2eTestSource::new("outbox-src", true).unwrap();
    let event_tx = source.event_sender();
    core.add_source(source).await.unwrap();
    core.add_query(make_persistent_query("outbox-q", "outbox-src", None))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("outbox-src").await.unwrap();
    core.start_query("outbox-q").await.unwrap();
    send_event(&event_tx, "outbox-src", 1, b"pos-1").await;
    if sequence == 2 {
        let mut properties = ElementPropertyMap::new();
        properties.insert("id", ElementValue::String("node-1".into()));
        properties.insert("value", ElementValue::Integer(2));
        let mut event = SourceEventWrapper::new(
            "outbox-src".to_string(),
            SourceEvent::Change(SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("outbox-src", "node-1"),
                        labels: Arc::from(vec![Arc::from("Node")]),
                        effective_from: 1,
                    },
                    properties,
                },
            }),
            chrono::Utc::now(),
            2,
        );
        event.source_position = Some(Bytes::from_static(b"pos-2"));
        event_tx
            .read()
            .await
            .as_ref()
            .expect("subscribed source")
            .send(Arc::new(event))
            .await
            .unwrap();
    }
    let before = wait_for_outbox_sequence(&core, "outbox-q", sequence).await;
    assert_eq!(before.latest_sequence, sequence);
    assert_eq!(before.results.len(), sequence as usize);
    let signature = match &before.results[0].results[0] {
        ResultDiff::Add {
            data,
            row_signature,
        } => {
            assert_eq!(*data, json!({"id": "node-1", "value": 1}));
            *row_signature
        }
        other => panic!("expected initial insert, got {other:?}"),
    };
    if sequence == 2 {
        assert_eq!(
            before.results[1].results,
            vec![ResultDiff::Update {
                data: json!({"id": "node-1", "value": 2}),
                before: json!({"id": "node-1", "value": 1}),
                after: json!({"id": "node-1", "value": 2}),
                grouping_keys: None,
                row_signature: signature,
            }]
        );
    }
    core.shutdown().await.unwrap();
    drop(core);

    let legacy_entries = if legacy_compact {
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);
        let indexes = provider.create_indexes("outbox-q").await.unwrap();
        let writer = indexes.outbox_writer.as_ref().unwrap();
        let mut entries = writer.read_from("outbox-q", 0).await.unwrap();
        for (sequence, bytes) in &mut entries {
            let result: QueryResult = rmp_serde::from_slice(bytes).unwrap();
            *bytes = rmp_serde::to_vec(&result).unwrap();
            writer.append("outbox-q", *sequence, bytes).await.unwrap();
        }
        Some(entries)
    } else {
        None
    };

    let reopened = build_e2e_lib("outbox-reopen", &tmp_dir, Some(RecoveryPolicy::Strict))
        .await
        .unwrap();
    let source = E2eTestSource::new("outbox-src", true).unwrap();
    let resume_sequence = source.last_resume_sequence();
    let event_tx = source.event_sender();
    reopened.add_source(source).await.unwrap();
    reopened
        .add_query(make_persistent_query("outbox-q", "outbox-src", None))
        .await
        .unwrap();
    reopened.start().await.unwrap();
    reopened.start_source("outbox-src").await.unwrap();
    let started = reopened.start_query("outbox-q").await;
    if let Some(legacy_entries) = legacy_entries {
        let error = started.expect_err("malformed legacy output must fail Strict recovery");
        let message = format!("{error:#}");
        assert!(message.contains("sequence 2"), "{message}");
        assert!(message.contains("expected a sequence"), "{message}");
        assert!(message.contains("Strict recovery policy"), "{message}");
        wait_for_status(&reopened, "outbox-q", ComponentStatus::Error).await;
        reopened.shutdown().await.unwrap();
        drop(reopened);
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);
        let indexes = provider.create_indexes("outbox-q").await.unwrap();
        assert_eq!(
            indexes
                .outbox_writer
                .as_ref()
                .unwrap()
                .read_from("outbox-q", 0)
                .await
                .unwrap(),
            legacy_entries,
            "Strict recovery must preserve the malformed records"
        );
        return;
    }
    started.expect("fresh persistent reopen must hydrate ordinary query updates");
    assert_eq!(*resume_sequence.read().await, Some(sequence));
    let query = reopened
        .query_manager()
        .get_query_instance("outbox-q")
        .await
        .unwrap();
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, sequence);
    assert_eq!(
        snapshot.to_vec(),
        vec![json!({"id": "node-1", "value": sequence})]
    );
    let restored = query.fetch_outbox(0).await.unwrap();
    assert_eq!(restored.latest_sequence, sequence);
    assert_eq!(restored.config_hash, before.config_hash);
    assert_eq!(restored.output_generation, before.output_generation);
    assert_eq!(restored.results.len(), before.results.len());
    for (index, (restored, original)) in restored.results.iter().zip(&before.results).enumerate() {
        assert_eq!(restored.sequence, index as u64 + 1);
        assert_eq!(restored.query_id, original.query_id);
        assert_eq!(restored.timestamp, original.timestamp);
        assert_eq!(restored.results, original.results);
        assert_eq!(restored.metadata, original.metadata);
        assert!(restored.profiling.is_some());
    }
    assert_eq!(
        query
            .fetch_outbox(sequence - 1)
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    send_event(&event_tx, "outbox-src", sequence + 1, b"pos-next").await;
    let continued = wait_for_outbox_sequence(&reopened, "outbox-q", sequence + 1).await;
    assert_eq!(continued.latest_sequence, sequence + 1);
    assert_eq!(continued.results.last().unwrap().sequence, sequence + 1);
    reopened.shutdown().await.unwrap();
}

/// Full lifecycle: build → start → feed events → stop → restart → verify resume_from.
#[tokio::test]
#[serial]
async fn test_e2e_checkpoint_round_trip() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rt", &tmp_dir, None).await.unwrap();

    // Create source and grab shared handles before moving
    let source = E2eTestSource::new("e2e-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    // Add and start source
    core.add_source(source).await.unwrap();
    core.start_source("e2e-src").await.unwrap();
    wait_for_status(&core, "e2e-src", ComponentStatus::Running).await;

    // Add and start persistent query
    let query = make_persistent_query("e2e-q", "e2e-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Running).await;

    // First subscribe: no resume_from (first start)
    assert!(
        resume_from.read().await.is_none(),
        "First start should have no resume_from"
    );
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Feed events with positions
    send_event(&event_tx, "e2e-src", 1, b"pos-1").await;
    send_event(&event_tx, "e2e-src", 2, b"pos-2").await;
    send_event(&event_tx, "e2e-src", 3, b"pos-3").await;

    // Wait for checkpoint processing
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop the query
    core.stop_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Stopped).await;

    // Restart the query
    core.start_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Running).await;

    // Second subscribe: should have resume_from with the last checkpointed position
    assert_eq!(
        sub_count.load(Ordering::Acquire),
        2,
        "Should have subscribed twice"
    );

    let resumed = resume_from.read().await;
    assert!(
        resumed.is_some(),
        "After restart, resume_from should be set from checkpoint"
    );

    core.stop_query("e2e-q").await.unwrap();
}

/// Restart monotonicity (issue #827): after a checkpoint at sequence N, the query
/// manager must carry `resume_sequence = Some(N)` back to the source on resubscribe
/// so a native-cursor source can raise its sequence counter above the dedup
/// high-water. This verifies the QueryManager → source wiring end-to-end.
#[tokio::test]
#[serial]
async fn test_e2e_resume_sequence_carried_back() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rs", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("e2e-rs-src", true).unwrap();
    let resume_sequence = source.last_resume_sequence();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    core.add_source(source).await.unwrap();
    core.start_source("e2e-rs-src").await.unwrap();
    wait_for_status(&core, "e2e-rs-src", ComponentStatus::Running).await;

    let query = make_persistent_query("e2e-rs-q", "e2e-rs-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("e2e-rs-q").await.unwrap();
    wait_for_status(&core, "e2e-rs-q", ComponentStatus::Running).await;

    // First start: no resume_sequence.
    assert!(
        resume_sequence.read().await.is_none(),
        "First start should have no resume_sequence"
    );
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Feed events; the last checkpointed sequence should be 3.
    send_event(&event_tx, "e2e-rs-src", 1, b"pos-1").await;
    send_event(&event_tx, "e2e-rs-src", 2, b"pos-2").await;
    send_event(&event_tx, "e2e-rs-src", 3, b"pos-3").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop and restart the query.
    core.stop_query("e2e-rs-q").await.unwrap();
    wait_for_status(&core, "e2e-rs-q", ComponentStatus::Stopped).await;
    core.start_query("e2e-rs-q").await.unwrap();
    wait_for_status(&core, "e2e-rs-q", ComponentStatus::Running).await;

    assert_eq!(
        sub_count.load(Ordering::Acquire),
        2,
        "Should have subscribed twice"
    );

    // After restart, the source must receive the checkpointed sequence so it can
    // raise its counter above the dedup high-water.
    let resumed_seq = *resume_sequence.read().await;
    assert_eq!(
        resumed_seq,
        Some(3),
        "After restart, resume_sequence must carry the last checkpointed sequence"
    );

    core.stop_query("e2e-rs-q").await.unwrap();
}

/// Auto-reset must clear `resume_sequence` (not just `resume_from`): after a
/// checkpoint exists, a forced auto-reset re-subscribe should hand the source a
/// fresh-start `resume_sequence = None`, matching the cleared `resume_from`.
#[tokio::test]
#[serial]
async fn test_e2e_auto_reset_clears_resume_sequence() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rs-reset", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("rs-reset-src", true).unwrap();
    let resume_sequence = source.last_resume_sequence();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();
    let remaining_failures = source.remaining_failures_handle();

    core.add_source(source).await.unwrap();
    core.start_source("rs-reset-src").await.unwrap();
    wait_for_status(&core, "rs-reset-src", ComponentStatus::Running).await;

    // First run: establish a checkpoint at sequence 3.
    let query = make_persistent_query(
        "rs-reset-q",
        "rs-reset-src",
        Some(RecoveryPolicy::AutoReset),
    );
    core.add_query(query).await.unwrap();
    core.start_query("rs-reset-q").await.unwrap();
    wait_for_status(&core, "rs-reset-q", ComponentStatus::Running).await;

    send_event(&event_tx, "rs-reset-src", 1, b"pos-1").await;
    send_event(&event_tx, "rs-reset-src", 2, b"pos-2").await;
    send_event(&event_tx, "rs-reset-src", 3, b"pos-3").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query("rs-reset-q").await.unwrap();
    wait_for_status(&core, "rs-reset-q", ComponentStatus::Stopped).await;

    // Force the next resubscribe to fail with PositionUnavailable so AutoReset
    // clears persistent state and retries from a fresh bootstrap.
    remaining_failures.store(1, std::sync::atomic::Ordering::Release);
    let count_before = sub_count.load(Ordering::Acquire);

    core.start_query("rs-reset-q").await.unwrap();
    wait_for_status(&core, "rs-reset-q", ComponentStatus::Running).await;

    // Two more subscribes: the failing resume attempt, then the auto-reset retry.
    assert_eq!(
        sub_count.load(Ordering::Acquire),
        count_before + 2,
        "AutoReset should retry after the failed resume"
    );

    // The retry is a fresh bootstrap: both resume signals cleared.
    assert!(
        resume_from.read().await.is_none(),
        "AutoReset retry must clear resume_from"
    );
    assert!(
        resume_sequence.read().await.is_none(),
        "AutoReset retry must clear resume_sequence"
    );

    core.stop_query("rs-reset-q").await.unwrap();
}

#[tokio::test]
#[serial]
async fn test_e2e_volatile_query_no_checkpoints() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-vol", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("vol-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    core.add_source(source).await.unwrap();
    core.start_source("vol-src").await.unwrap();
    wait_for_status(&core, "vol-src", ComponentStatus::Running).await;

    // Volatile query — no storage backend
    let query = make_volatile_query("vol-q", "vol-src");
    core.add_query(query).await.unwrap();
    core.start_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Running).await;

    send_event(&event_tx, "vol-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    core.stop_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Stopped).await;
    core.start_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Running).await;

    // Volatile: no resume_from even after restart
    assert!(
        resume_from.read().await.is_none(),
        "Volatile query should never set resume_from"
    );

    core.stop_query("vol-q").await.unwrap();
}

/// Persistent query + volatile source (supports_replay=false) → rejected.
#[tokio::test]
#[serial]
async fn test_e2e_persistent_query_volatile_source_rejected() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-compat", &tmp_dir, None).await.unwrap();

    // Source that does NOT support replay
    let source = E2eTestSource::new("compat-src", false).unwrap();
    core.add_source(source).await.unwrap();
    core.start_source("compat-src").await.unwrap();
    wait_for_status(&core, "compat-src", ComponentStatus::Running).await;

    let query = make_persistent_query("compat-q", "compat-src", None);
    core.add_query(query).await.unwrap();

    let result = core.start_query("compat-q").await;
    assert!(
        result.is_err(),
        "Persistent query + volatile source should be rejected"
    );
}

/// Strict policy + PositionUnavailable → query goes to Error state.
#[tokio::test]
#[serial]
async fn test_e2e_strict_policy_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-strict", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("strict-src", true).unwrap();
    source.set_fail_count(u32::MAX); // always fail
    core.add_source(source).await.unwrap();
    core.start_source("strict-src").await.unwrap();
    wait_for_status(&core, "strict-src", ComponentStatus::Running).await;

    let query = make_persistent_query("strict-q", "strict-src", Some(RecoveryPolicy::Strict));
    core.add_query(query).await.unwrap();

    let result = core.start_query("strict-q").await;
    assert!(
        result.is_err(),
        "Strict policy + PositionUnavailable should fail"
    );
}

/// AutoReset + PositionUnavailable → wipes state and re-bootstraps.
#[tokio::test]
#[serial]
async fn test_e2e_auto_reset_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-reset", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("reset-src", true).unwrap();
    source.set_fail_count(1); // fail once, succeed on retry
    let sub_count = source.subscribe_count_handle();
    let resume_from = source.last_resume_from();

    core.add_source(source).await.unwrap();
    core.start_source("reset-src").await.unwrap();
    wait_for_status(&core, "reset-src", ComponentStatus::Running).await;

    let query = make_persistent_query("reset-q", "reset-src", Some(RecoveryPolicy::AutoReset));
    core.add_query(query).await.unwrap();

    let result = core.start_query("reset-q").await;
    assert!(
        result.is_ok(),
        "AutoReset should retry and succeed: {result:?}"
    );

    // Subscribed twice: first fail, then retry
    assert_eq!(sub_count.load(Ordering::Acquire), 2);

    // On retry, resume_from should be None (fresh bootstrap)
    assert!(
        resume_from.read().await.is_none(),
        "AutoReset retry should clear resume_from"
    );

    wait_for_status(&core, "reset-q", ComponentStatus::Running).await;
    core.stop_query("reset-q").await.unwrap();
}

/// remove_query() clears persistent state so re-adding starts fresh.
#[tokio::test]
#[serial]
async fn test_e2e_remove_query_clears_persistent_state() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-remove", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("rm-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    core.add_source(source).await.unwrap();
    core.start_source("rm-src").await.unwrap();
    wait_for_status(&core, "rm-src", ComponentStatus::Running).await;

    // First lifecycle: add → start → feed events → stop
    let query = make_persistent_query("rm-q", "rm-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Running).await;

    send_event(&event_tx, "rm-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Stopped).await;

    // Remove the query (should clear persistent state)
    core.remove_query("rm-q").await.unwrap();

    // Re-add the same query and start
    let query2 = make_persistent_query("rm-q", "rm-src", None);
    core.add_query(query2).await.unwrap();
    core.start_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Running).await;

    // After removal + re-add, resume_from should be None (state was cleared)
    assert!(
        resume_from.read().await.is_none(),
        "After remove + re-add, resume_from should be None (persistent state cleared)"
    );

    core.stop_query("rm-q").await.unwrap();
}

/// stop() releases position handles on the source.
#[tokio::test]
#[serial]
async fn test_e2e_stop_releases_position_handles() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-handles", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("handle-src", true).unwrap();
    let removed_count = source.position_handle_removed_handle();

    core.add_source(source).await.unwrap();
    core.start_source("handle-src").await.unwrap();
    wait_for_status(&core, "handle-src", ComponentStatus::Running).await;

    let query = make_persistent_query("handle-q", "handle-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("handle-q").await.unwrap();
    wait_for_status(&core, "handle-q", ComponentStatus::Running).await;

    assert_eq!(
        removed_count.load(Ordering::Acquire),
        0,
        "No position handle removal yet"
    );

    // Stop should release position handles
    core.stop_query("handle-q").await.unwrap();

    assert!(
        removed_count.load(Ordering::Acquire) >= 1,
        "Stop should have released position handle"
    );
}

/// Config change (different query string) triggers full re-bootstrap instead of resume.
#[tokio::test]
#[serial]
async fn test_e2e_config_change_triggers_rebootstrap() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-cfghash", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("cfg-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    core.add_source(source).await.unwrap();
    core.start_source("cfg-src").await.unwrap();
    wait_for_status(&core, "cfg-src", ComponentStatus::Running).await;

    // Start with initial query
    let query1 = Query::cypher("cfg-q")
        .query("MATCH (n:Node) RETURN n.id AS id")
        .from_source("cfg-src")
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
        .build();
    core.add_query(query1).await.unwrap();
    core.start_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Running).await;

    // Feed events to create checkpoints
    send_event(&event_tx, "cfg-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Stopped).await;
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Update query with different query string (config hash changes)
    let query2 = Query::cypher("cfg-q")
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source("cfg-src")
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
        .build();
    core.update_query("cfg-q", query2).await.unwrap();

    // Restart — config hash changed, so should NOT resume from checkpoint
    core.start_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Running).await;

    assert_eq!(sub_count.load(Ordering::Acquire), 2);

    // resume_from should be None because config changed → full re-bootstrap
    assert!(
        resume_from.read().await.is_none(),
        "Config change should trigger fresh start (no resume_from)"
    );

    core.stop_query("cfg-q").await.unwrap();
}

mod aggregate_snapshot_tests {
    use super::*;
    use crate::channels::{QuerySubscriptionResponse, ResultDiff};
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    const SOURCE: &str = "aggregate-src";
    const TIMEOUT: Duration = Duration::from_secs(10);

    struct PersistentAggregate {
        core: Arc<DrasiLib>,
        query: Arc<dyn crate::queries::Query>,
        event_tx: Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
        subscription: QuerySubscriptionResponse,
    }

    impl PersistentAggregate {
        async fn open(tmp: &tempfile::TempDir, query_id: &str) -> Self {
            Self::open_query(
                tmp,
                query_id,
                "MATCH (n:Node)
                 WITH sum(n.value) AS totalValue, sum(n.cost) AS totalCost,
                      count(n) AS positionCount
                 RETURN totalValue, totalCost, positionCount",
            )
            .await
        }

        async fn open_query(tmp: &tempfile::TempDir, query_id: &str, query_text: &str) -> Self {
            let core = build_e2e_lib("aggregate-persistence", tmp, None)
                .await
                .unwrap();
            core.start().await.unwrap();
            let source = E2eTestSource::new(SOURCE, true).unwrap();
            let event_tx = source.event_sender();
            core.add_source(source).await.unwrap();
            core.start_source(SOURCE).await.unwrap();
            wait_for_status(&core, SOURCE, ComponentStatus::Running).await;

            let config = Query::cypher(query_id)
                .query(query_text)
                .from_source(SOURCE)
                .auto_start(false)
                .enable_bootstrap(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
                .build();
            core.add_query(config).await.unwrap();
            core.start_query(query_id).await.unwrap();
            wait_for_status(&core, query_id, ComponentStatus::Running).await;
            let query = core
                .query_manager()
                .get_query_instance(query_id)
                .await
                .unwrap();
            let subscription = query
                .subscribe("persistence-regression".into())
                .await
                .unwrap();
            Self {
                core,
                query,
                event_tx,
                subscription,
            }
        }

        async fn send(&mut self, change: SourceChange, sequence: u64) -> Vec<ResultDiff> {
            let event = SourceEventWrapper::new(
                SOURCE.to_string(),
                crate::channels::SourceEvent::Change(change),
                chrono::Utc::now(),
                sequence,
            )
            .with_source_position(Bytes::copy_from_slice(&sequence.to_be_bytes()));
            let sender = self.event_tx.read().await.as_ref().unwrap().clone();
            sender.send(Arc::new(event)).await.unwrap();
            timeout(TIMEOUT, self.subscription.receiver.recv())
                .await
                .expect("the source change must produce a live delta")
                .unwrap()
                .results
                .clone()
        }

        async fn assert_snapshot(&self, expected: HashMap<u64, Value>) {
            let snapshot = timeout(TIMEOUT, self.query.fetch_snapshot())
                .await
                .unwrap()
                .unwrap();
            let keyed: HashMap<_, _> = snapshot.stream_keyed().collect().await;
            assert_eq!(keyed, expected);
            let mut actual = self
                .core
                .get_query_results(&self.query.get_config().id)
                .await
                .unwrap();
            let mut expected: Vec<_> = expected.into_values().collect();
            actual.sort_by_key(Value::to_string);
            expected.sort_by_key(Value::to_string);
            assert_eq!(actual, expected);
        }

        async fn shutdown(self) {
            self.core.shutdown().await.unwrap();
        }
    }

    fn position(id: &str, value: i64, cost: i64, time: u64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, id),
                labels: Arc::from([Arc::from("Node")]),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(json!({"value": value, "cost": cost})),
        }
    }

    fn summary(value: f64, cost: f64, count: i64) -> Value {
        json!({"totalValue": value, "totalCost": cost, "positionCount": count})
    }

    #[tokio::test]
    #[serial]
    async fn grouping_numeric_persistent_compound_key_reopens_and_drains() {
        const QUERY: &str = "MATCH (n:Node)
            WITH n.groupKey AS groupKey, sum(n.value) AS totalValue,
                 sum(n.cost) AS totalCost, count(n) AS positionCount
            RETURN totalValue, totalCost, positionCount";
        fn grouped(id: &str, group: Value, value: i64, cost: i64, time: u64) -> Element {
            let mut node = position(id, value, cost, time);
            if let Element::Node { properties, .. } = &mut node {
                *properties = ElementPropertyMap::from(json!({
                    "groupKey": group, "value": value, "cost": cost,
                }));
            }
            node
        }
        let integer = json!([1, {"bucket": 0}]);
        let float = json!([1.0, {"bucket": -0.0}]);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open_query(&tmp, "numeric-summary", QUERY).await;
        let added = subject
            .send(
                SourceChange::Insert {
                    element: grouped("p", integer, 10, 8, 1),
                },
                1,
            )
            .await;
        let signature = match added.as_slice() {
            [ResultDiff::Add {
                row_signature,
                data,
            }] => {
                assert_eq!(data, &summary(10.0, 8.0, 1));
                *row_signature
            }
            other => panic!("expected a group addition, got {other:?}"),
        };
        assert_eq!(
            subject
                .send(
                    SourceChange::Insert {
                        element: grouped("q", float.clone(), 5, 4, 2),
                    },
                    2
                )
                .await,
            update(signature, summary(10.0, 8.0, 1), summary(15.0, 12.0, 2))
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(15.0, 12.0, 2))]))
            .await;
        subject.shutdown().await;

        let mut subject = PersistentAggregate::open_query(&tmp, "numeric-summary", QUERY).await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(15.0, 12.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        element: grouped("p", float, 20, 8, 3),
                    },
                    3
                )
                .await,
            update(signature, summary(15.0, 12.0, 2), summary(25.0, 12.0, 2))
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(25.0, 12.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("q", 5, 4, 4).get_metadata().clone(),
                    },
                    4
                )
                .await,
            update(signature, summary(25.0, 12.0, 2), summary(20.0, 8.0, 1))
        );
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("p", 20, 8, 5).get_metadata().clone(),
                    },
                    5
                )
                .await,
            vec![ResultDiff::Delete {
                data: summary(20.0, 8.0, 1),
                row_signature: signature
            }]
        );
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
        let subject = PersistentAggregate::open_query(&tmp, "numeric-summary", QUERY).await;
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
    }

    fn update(signature: u64, before: Value, after: Value) -> Vec<ResultDiff> {
        vec![ResultDiff::Update {
            data: after.clone(),
            before,
            after,
            grouping_keys: None,
            row_signature: signature,
        }]
    }

    async fn seed(subject: &mut PersistentAggregate, aapl_value: i64) -> u64 {
        let first = subject
            .send(
                SourceChange::Insert {
                    element: position("aapl", aapl_value, 800, 1),
                },
                1,
            )
            .await;
        let signature = match first.as_slice() {
            [ResultDiff::Add {
                data,
                row_signature,
            }] => {
                assert_eq!(data, &summary(aapl_value as f64, 800.0, 1));
                *row_signature
            }
            _ => panic!("expected one initial aggregate Add, got {first:?}"),
        };
        subject
            .assert_snapshot(HashMap::from([(
                signature,
                summary(aapl_value as f64, 800.0, 1),
            )]))
            .await;
        let second = subject
            .send(
                SourceChange::Insert {
                    element: position("msft", 900, 1000, 2),
                },
                2,
            )
            .await;
        assert_eq!(
            second,
            update(
                signature,
                summary(aapl_value as f64, 800.0, 1),
                summary((aapl_value + 900) as f64, 1800.0, 2),
            )
        );
        subject
            .assert_snapshot(HashMap::from([(
                signature,
                summary((aapl_value + 900) as f64, 1800.0, 2),
            )]))
            .await;
        signature
    }

    #[tokio::test]
    #[serial]
    async fn aggregate_snapshot_persistent_reopen_keeps_group_identity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open(&tmp, "persistent-summary").await;
        let signature = seed(&mut subject, 1100).await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        element: position("aapl", 1150, 800, 3),
                    },
                    3,
                )
                .await,
            update(
                signature,
                summary(2000.0, 1800.0, 2),
                summary(2050.0, 1800.0, 2)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2050.0, 1800.0, 2))]))
            .await;
        subject.shutdown().await;

        let mut subject = PersistentAggregate::open(&tmp, "persistent-summary").await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2050.0, 1800.0, 2))]))
            .await;
        // A different contributor updates the same persisted group after reopen.
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        element: position("msft", 1000, 1000, 4),
                    },
                    4,
                )
                .await,
            update(
                signature,
                summary(2050.0, 1800.0, 2),
                summary(2150.0, 1800.0, 2)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2150.0, 1800.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("msft", 1000, 1000, 5).get_metadata().clone(),
                    },
                    5,
                )
                .await,
            update(
                signature,
                summary(2150.0, 1800.0, 2),
                summary(1150.0, 800.0, 1)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(1150.0, 800.0, 1))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("aapl", 1150, 800, 6).get_metadata().clone(),
                    },
                    6,
                )
                .await,
            vec![ResultDiff::Delete {
                data: summary(1150.0, 800.0, 1),
                row_signature: signature,
            }]
        );
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;

        let subject = PersistentAggregate::open(&tmp, "persistent-summary").await;
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
    }

    #[tokio::test]
    #[serial]
    async fn aggregate_snapshot_legacy_contributor_keys_require_explicit_rebuild() {
        use drasi_core::{
            evaluation::functions::FunctionRegistry, interface::RowMutation, query::QueryBuilder,
        };
        use drasi_query_cypher::CypherParser;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open(&tmp, "legacy-summary").await;
        let signature = seed(&mut subject, 1100).await;
        subject.shutdown().await;

        let ordinary = QueryBuilder::new(
            "MATCH (n:Node) RETURN n.value",
            Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
        )
        .build()
        .await;
        let mut contributor_signatures = Vec::new();
        for element in [position("aapl", 1100, 800, 1), position("msft", 900, 1000, 2)] {
            let diffs = ordinary
                .process_source_change(SourceChange::Insert { element })
                .await
                .unwrap();
            assert_eq!(diffs.len(), 1);
            contributor_signatures.push(diffs[0].row_signature());
        }
        let aapl = contributor_signatures[0];
        let msft = contributor_signatures[1];
        assert_ne!(aapl, msft);
        assert_ne!(signature, aapl);
        assert_ne!(signature, msft);

        // Model a pre-fix live-results table without changing the accumulator or
        // checkpoint format: intermediate and current rows use MATCH identities.
        let stale = rmp_serde::to_vec(&summary(1100.0, 800.0, 1)).unwrap();
        let current = rmp_serde::to_vec(&summary(2000.0, 1800.0, 2)).unwrap();
        {
            let indexes = RocksDbIndexProvider::new(tmp.path(), false, false)
                .create_indexes("legacy-summary")
                .await
                .unwrap();
            indexes
                .live_results_writer
                .as_ref()
                .unwrap()
                .apply_mutations(
                    "legacy-summary",
                    &[
                        RowMutation {
                            row_signature: signature,
                            data: None,
                        },
                        RowMutation {
                            row_signature: aapl,
                            data: Some(&stale),
                        },
                        RowMutation {
                            row_signature: msft,
                            data: Some(&current),
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let mut legacy =
            HashMap::from([(aapl, summary(1100.0, 800.0, 1)), (msft, summary(2000.0, 1800.0, 2))]);
        let mut subject = PersistentAggregate::open(&tmp, "legacy-summary").await;
        subject.assert_snapshot(legacy.clone()).await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        element: position("aapl", 1150, 800, 3),
                    },
                    3
                )
                .await,
            update(
                signature,
                summary(2000.0, 1800.0, 2),
                summary(2050.0, 1800.0, 2)
            )
        );
        subject.shutdown().await;

        // A corrected upsert cannot identify or remove old contributor keys.
        // Reopening reads all of them; this is a migration limit, not recovery.
        legacy.insert(signature, summary(2050.0, 1800.0, 2));
        let subject = PersistentAggregate::open(&tmp, "legacy-summary").await;
        subject.assert_snapshot(legacy).await;
        subject.shutdown().await;

        // A new query namespace and complete input reconstruction are an explicit
        // rebuild, leaving the old namespace intact rather than guessing by value.
        let mut rebuilt = PersistentAggregate::open(&tmp, "rebuilt-summary").await;
        rebuilt.assert_snapshot(HashMap::new()).await;
        assert_eq!(seed(&mut rebuilt, 1150).await, signature);
        rebuilt.shutdown().await;
        let indexes = RocksDbIndexProvider::new(tmp.path(), false, false)
            .create_indexes("legacy-summary")
            .await
            .unwrap();
        assert_eq!(
            indexes
                .live_results_writer
                .as_ref()
                .unwrap()
                .read_snapshot("legacy-summary")
                .await
                .unwrap()
                .len(),
            3
        );
    }
}
