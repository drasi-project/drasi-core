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

use super::*;
use drasi_core::interface::{LiveResultsWriter, RowMutation, SessionControl};
use futures::StreamExt;
use std::sync::{Mutex, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    FenceWrite,
    FenceWriteAfter,
    FenceRead,
    Begin,
    Rows,
    RowsNoop,
    Checkpoint,
    CheckpointNoop,
    Sequence,
    SequenceNoop,
    Commit,
    CommitAfter,
    CommitNoop,
    RowsPanic,
    ReadCheckpoints,
    ReadRows,
    ReadSequence,
    ReadGeneration,
    Publish,
    PublishAfter,
    PublishNoop,
    PublishRead,
}

#[derive(Clone, Copy)]
enum Pause {
    Commit,
    Publish,
}

struct PublicationGate {
    point: Pause,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Default)]
struct Trace {
    fenced: bool,
    begins: usize,
    commits: usize,
    hash_reads: usize,
    hit: bool,
}

struct Probe {
    fault: Option<Fault>,
    hash: u64,
    trace: Mutex<Trace>,
    gate: Option<Arc<PublicationGate>>,
    store: Mutex<Option<Weak<dyn CheckpointStore>>>,
    rows: Mutex<Option<Weak<dyn LiveResultsWriter>>>,
}

impl Probe {
    fn new(fault: Option<Fault>) -> Self {
        Self {
            fault,
            hash: crate::queries::compute_config_hash(&bootstrap_query(RecoveryPolicy::Strict)),
            trace: Mutex::new(Trace::default()),
            gate: None,
            store: Mutex::new(None),
            rows: Mutex::new(None),
        }
    }

    fn hit(&self, point: Fault) -> bool {
        if self.fault == Some(point) {
            self.trace.lock().unwrap().hit = true;
            true
        } else {
            false
        }
    }

    fn fail(&self, point: Fault) -> Result<(), IndexError> {
        if self.hit(point) {
            Err(IndexError::other(std::io::Error::other(format!(
                "injected {point:?}"
            ))))
        } else {
            Ok(())
        }
    }

    fn fenced(&self) -> bool {
        self.trace.lock().unwrap().fenced
    }

    async fn pause(&self, point: Pause) {
        if let Some(gate) = &self.gate {
            if matches!(
                (gate.point, point),
                (Pause::Commit, Pause::Commit) | (Pause::Publish, Pause::Publish)
            ) {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
    }
}

struct PublicationProvider {
    inner: RocksDbIndexProvider,
    probe: Arc<Probe>,
}

#[async_trait]
impl IndexBackendPlugin for PublicationProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let mut indexes = self.inner.create_indexes(query_id).await?;
        let checkpoints = indexes.checkpoint_store.take().unwrap();
        let rows = indexes.live_results_writer.take().unwrap();
        *self.probe.store.lock().unwrap() = Some(Arc::downgrade(&checkpoints));
        *self.probe.rows.lock().unwrap() = Some(Arc::downgrade(&rows));
        indexes.checkpoint_store = Some(Arc::new(PublicationCheckpoints {
            inner: checkpoints,
            probe: self.probe.clone(),
        }));
        indexes.live_results_writer = Some(Arc::new(PublicationRows {
            inner: rows,
            probe: self.probe.clone(),
        }));
        indexes.set.session_control = Arc::new(PublicationSession {
            inner: indexes.set.session_control,
            probe: self.probe.clone(),
        });
        Ok(indexes)
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

struct PublicationCheckpoints {
    inner: Arc<dyn CheckpointStore>,
    probe: Arc<Probe>,
}

#[async_trait]
impl CheckpointStore for PublicationCheckpoints {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::Checkpoint)?;
            if self.probe.hit(Fault::CheckpointNoop) {
                return Ok(());
            }
        }
        self.inner
            .stage_checkpoint(source_id, sequence, position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::ReadCheckpoints)?;
        }
        self.inner.read_all_checkpoints().await
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let pending = self.probe.hash.wrapping_add(4);
        let published = self.probe.hash.wrapping_add(2);
        if hash == pending {
            self.probe.fail(Fault::FenceWrite)?;
        }
        if hash == published {
            self.probe.pause(Pause::Publish).await;
            self.probe.fail(Fault::Publish)?;
            if self.probe.hit(Fault::PublishNoop) {
                return Ok(());
            }
        }
        self.inner.write_config_hash(hash).await?;
        if hash == pending {
            self.probe.trace.lock().unwrap().fenced = true;
            self.probe.fail(Fault::FenceWriteAfter)?;
        }
        if hash == published {
            self.probe.fail(Fault::PublishAfter)?;
        }
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        if self.probe.fenced() {
            let count = {
                let mut trace = self.probe.trace.lock().unwrap();
                trace.hash_reads += 1;
                trace.hash_reads
            };
            if count == 1 {
                self.probe.fail(Fault::FenceRead)?;
            } else {
                self.probe.fail(Fault::PublishRead)?;
            }
        }
        self.inner.read_config_hash().await
    }

    async fn stage_result_sequence(&self, id: &str, sequence: u64) -> Result<(), IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::Sequence)?;
            if self.probe.hit(Fault::SequenceNoop) {
                return Ok(());
            }
        }
        self.inner.stage_result_sequence(id, sequence).await
    }

    async fn write_result_sequence(&self, id: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.write_result_sequence(id, sequence).await
    }

    async fn read_result_sequence(&self, id: &str) -> Result<Option<u64>, IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::ReadSequence)?;
        }
        self.inner.read_result_sequence(id).await
    }

    async fn write_output_generation(&self, id: &str, generation: u64) -> Result<(), IndexError> {
        self.inner.write_output_generation(id, generation).await
    }

    async fn read_output_generation(&self, id: &str) -> Result<Option<u64>, IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::ReadGeneration)?;
        }
        self.inner.read_output_generation(id).await
    }
}

struct PublicationSession {
    inner: Arc<dyn SessionControl>,
    probe: Arc<Probe>,
}

#[async_trait]
impl SessionControl for PublicationSession {
    async fn begin(&self) -> Result<(), IndexError> {
        let final_image = {
            let mut trace = self.probe.trace.lock().unwrap();
            if trace.fenced {
                trace.begins += 1;
            }
            trace.fenced && trace.begins == 2
        };
        if final_image {
            self.probe.fail(Fault::Begin)?;
        }
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let final_image = {
            let mut trace = self.probe.trace.lock().unwrap();
            if trace.fenced {
                trace.commits += 1;
            }
            trace.fenced && trace.commits == 2
        };
        if final_image {
            self.probe.pause(Pause::Commit).await;
            self.probe.fail(Fault::Commit)?;
            if self.probe.hit(Fault::CommitNoop) {
                return Ok(());
            }
        }
        self.inner.commit().await?;
        if final_image {
            self.probe.fail(Fault::CommitAfter)?;
        }
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct PublicationRows {
    inner: Arc<dyn LiveResultsWriter>,
    probe: Arc<Probe>,
}

#[async_trait]
impl LiveResultsWriter for PublicationRows {
    async fn apply_mutations(
        &self,
        id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        if self.probe.hit(Fault::RowsPanic) {
            panic!("controlled bootstrap writer panic");
        }
        self.probe.fail(Fault::Rows)?;
        if self.probe.hit(Fault::RowsNoop) {
            return Ok(());
        }
        self.inner.apply_mutations(id, mutations).await
    }

    async fn read_snapshot(&self, id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        if self.probe.fenced() {
            self.probe.fail(Fault::ReadRows)?;
        }
        self.inner.read_snapshot(id).await
    }

    async fn clear(&self, id: &str) -> Result<(), IndexError> {
        self.inner.clear(id).await
    }

    async fn row_count(&self, id: &str) -> Result<usize, IndexError> {
        self.inner.row_count(id).await
    }
}

async fn publication_core(tmp: &tempfile::TempDir, probe: Arc<Probe>) -> DrasiLib {
    DrasiLib::builder()
        .with_bootstrap_certification(true)
        .with_id("bootstrap-publication")
        .with_index_provider(
            "persistent",
            Arc::new(PublicationProvider {
                inner: RocksDbIndexProvider::new(tmp.path(), false, false),
                probe,
            }),
        )
        .build()
        .await
        .unwrap()
}

async fn add_bootstrap_source(core: &DrasiLib) -> Arc<AtomicU32> {
    let mut source = E2eTestSource::new("bootstrap-src", true).unwrap();
    source.bootstrap = Some(vec![bootstrap_node()]);
    let count = source.subscribe_count_handle();
    core.add_source(source).await.unwrap();
    count
}

#[rstest::rstest]
#[case::config_change(false)]
#[case::output_corruption(true)]
#[tokio::test]
#[serial]
async fn review_failed_replacement_remains_fenced(#[case] corrupt_output: bool) {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = bootstrap_query(RecoveryPolicy::AutoReset);
    let hash = crate::queries::compute_config_hash(&config);
    let core = build_compatible_e2e_lib("replacement", &tmp, None)
        .await
        .unwrap();
    add_bootstrap_source(&core).await;
    core.add_query(config.clone()).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    query.fetch_snapshot().await.unwrap();
    drop(query);
    core.shutdown().await.unwrap();
    drop(core);
    {
        let provider = RocksDbIndexProvider::new(tmp.path(), false, false);
        let indexes = provider.create_indexes("bootstrap-q").await.unwrap();
        if corrupt_output {
            indexes
                .outbox_writer
                .as_ref()
                .unwrap()
                .append("bootstrap-q", 1, b"corrupt")
                .await
                .unwrap();
        } else {
            indexes
                .checkpoint_store
                .as_ref()
                .unwrap()
                .write_config_hash(hash ^ 1)
                .await
                .unwrap();
        }
    }

    let core = build_compatible_e2e_lib("replacement", &tmp, None)
        .await
        .unwrap();
    core.add_source(E2eTestSource::new("bootstrap-src", false).unwrap())
        .await
        .unwrap();
    core.add_query(config.clone()).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let error = core.start_query("bootstrap-q").await.unwrap_err();
    assert!(
        error.to_string().contains("do not support replay"),
        "{error}"
    );
    core.shutdown().await.unwrap();
    drop(core);

    let before = logical_rocks_state(tmp.path(), "bootstrap-q");
    let core = build_compatible_e2e_lib("replacement", &tmp, None)
        .await
        .unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    let mut strict_config = config;
    strict_config.recovery_policy = Some(RecoveryPolicy::Strict);
    core.add_query(strict_config).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let start = core.start_query("bootstrap-q").await;
    core.shutdown().await.unwrap();
    drop(core);
    assert!(
        start.is_err(),
        "Strict accepted a replacement without its initial image"
    );
    assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    assert_eq!(logical_rocks_state(tmp.path(), "bootstrap-q"), before);
}

#[rstest::rstest]
#[case::compatible_reset(false, false)]
#[case::certified_reset(true, false)]
#[case::compatible_publication(false, true)]
#[case::certified_publication(true, true)]
#[tokio::test]
#[serial]
async fn review_interrupted_bootstrap_reset_requires_auto_reset(
    #[case] certified: bool,
    #[case] publication_pending: bool,
    #[values(RecoveryPolicy::Strict, RecoveryPolicy::AutoReset)] policy: RecoveryPolicy,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = bootstrap_query(RecoveryPolicy::Strict);
    let core = build_e2e_lib_with_certification("reset", &tmp, None, certified)
        .await
        .unwrap();
    add_bootstrap_source(&core).await;
    core.add_query(config.clone()).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    query.fetch_snapshot().await.unwrap();
    drop(query);
    core.shutdown().await.unwrap();
    drop(core);
    {
        let provider = RocksDbIndexProvider::new(tmp.path(), false, false);
        let indexes = provider.create_indexes("bootstrap-q").await.unwrap();
        let hash = crate::queries::compute_config_hash(&config);
        indexes
            .checkpoint_store
            .as_ref()
            .unwrap()
            .write_config_hash(if publication_pending {
                hash.wrapping_add(4)
            } else {
                !hash
            })
            .await
            .unwrap();
    }
    let before = logical_rocks_state(tmp.path(), "bootstrap-q");
    let core = build_e2e_lib_with_certification("reset", &tmp, None, certified)
        .await
        .unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    let mut config = config;
    config.recovery_policy = Some(policy);
    core.add_query(config).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let start = core.start_query("bootstrap-q").await;
    if policy == RecoveryPolicy::AutoReset {
        start.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(
            snapshot.to_vec(),
            vec![serde_json::json!({"id": "seed", "value": 1})]
        );
        assert_eq!(snapshot.output_generation, 1);
        assert_eq!(subscriptions.load(Ordering::Acquire), 1);
    } else {
        assert!(
            start.is_err(),
            "Strict completed an interrupted bootstrap reset"
        );
        assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    }
    core.shutdown().await.unwrap();
    drop(core);
    if policy == RecoveryPolicy::Strict {
        assert_eq!(logical_rocks_state(tmp.path(), "bootstrap-q"), before);
    }
}

#[rstest::rstest]
#[case::legacy_fallback(false, false)]
#[case::requested_certification(false, true)]
#[case::published_certification(true, false)]
#[case::published_and_requested(true, true)]
#[tokio::test]
#[serial]
async fn review_checkpoint_read_failure_respects_certification(
    #[case] published: bool,
    #[case] required: bool,
    #[values(RecoveryPolicy::Strict, RecoveryPolicy::AutoReset)] policy: RecoveryPolicy,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = make_persistent_query("stream", "source", Some(policy));
    let core = build_e2e_lib_with_certification("checkpoint-read", &tmp, None, published)
        .await
        .unwrap();
    let source = E2eTestSource::new("source", true).unwrap();
    let events = source.event_sender();
    core.add_source(source).await.unwrap();
    core.add_query(config.clone()).await.unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("stream").await.unwrap();
    send_event(&events, "source", 1, b"retained-position").await;
    wait_for_outbox_sequence(&core, "stream", 1).await;
    core.shutdown().await.unwrap();
    drop(core);
    let before = logical_rocks_state(tmp.path(), "stream");
    let core = DrasiLib::builder()
        .with_bootstrap_certification(required)
        .with_index_provider(
            "persistent",
            Arc::new(RecoveryTestProvider {
                inner: RocksDbIndexProvider::new(tmp.path(), false, false),
                failure: Arc::new(Mutex::new(RecoveryReadFailure::Checkpoints)),
            }),
        )
        .build()
        .await
        .unwrap();
    let source = E2eTestSource::new("source", true).unwrap();
    let subscriptions = source.subscribe_count_handle();
    core.add_source(source).await.unwrap();
    core.add_query(config).await.unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    let start = core.start_query("stream").await;
    core.shutdown().await.unwrap();
    drop(core);
    if published || required {
        assert!(
            start.is_err(),
            "certified recovery ignored an unreadable checkpoint"
        );
        assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    } else {
        start.unwrap();
        assert_eq!(subscriptions.load(Ordering::Acquire), 1);
    }
    assert_eq!(logical_rocks_state(tmp.path(), "stream"), before);
}

#[tokio::test]
#[serial]
async fn review_live_delivery_shutdown_preserves_committed_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("blocked-delivery", &tmp, None).await.unwrap();
    let source = E2eTestSource::new("source", true).unwrap();
    let events = source.event_sender();
    core.add_source(source).await.unwrap();
    let mut config = make_persistent_query("stream", "source", None);
    config.dispatch_mode = Some(crate::DispatchMode::Channel);
    config.dispatch_buffer_capacity = Some(1);
    core.add_query(config).await.unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("stream").await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance("stream")
        .await
        .unwrap();
    let subscription = query.subscribe("retained".into()).await.unwrap();
    send_event(&events, "source", 1, b"position-1").await;
    send_event(&events, "source", 2, b"position-2").await;
    wait_for_outbox_sequence(&core, "stream", 2).await;
    let stopped =
        tokio::time::timeout(std::time::Duration::from_secs(2), core.stop_query("stream")).await;
    drop(subscription);
    core.shutdown().await.unwrap();
    assert!(
        stopped.is_ok(),
        "an undrained result channel blocked shutdown"
    );
    stopped.unwrap().unwrap();
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    drop(query);
    drop(core);
    let core = build_e2e_lib("blocked-delivery", &tmp, None).await.unwrap();
    core.add_source(E2eTestSource::new("source", true).unwrap())
        .await
        .unwrap();
    core.add_query(make_persistent_query(
        "stream",
        "source",
        Some(RecoveryPolicy::ExistingOnly),
    ))
    .await
    .unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("stream").await.unwrap();
    let recovered = wait_for_outbox_sequence(&core, "stream", 2).await;
    assert_eq!(recovered.results.len(), 2);
    let query = core
        .query_manager()
        .get_query_instance("stream")
        .await
        .unwrap();
    assert_eq!(query.fetch_snapshot().await.unwrap().len(), 2);
    core.shutdown().await.unwrap();
}

#[rstest::rstest]
#[case::missing_stream(BootstrapFailure::MissingStream)]
#[case::missing_completion(BootstrapFailure::MissingCompletion)]
#[case::dropped_completion(BootstrapFailure::DroppedCompletion)]
#[case::provider_error(BootstrapFailure::Provider)]
#[case::event_count(BootstrapFailure::EventCount)]
#[case::missing_position(BootstrapFailure::MissingPosition)]
#[case::oversized_position(BootstrapFailure::OversizedPosition)]
#[case::foreign_source(BootstrapFailure::ForeignSource)]
#[tokio::test]
#[serial]
async fn test_bootstrap_source_failure_never_exposes_ready(#[case] failure: BootstrapFailure) {
    let tmp = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("bootstrap-source-error", &tmp, None)
        .await
        .unwrap();
    let mut source = E2eTestSource::new("bootstrap-src", true).unwrap();
    source.bootstrap = Some(vec![bootstrap_node()]);
    source.bootstrap_failure = Some(failure);
    core.add_source(source).await.unwrap();
    core.add_query(bootstrap_query(RecoveryPolicy::Strict))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let start = core.start_query("bootstrap-q").await;
    if failure == BootstrapFailure::MissingStream {
        assert!(start.is_err());
    } else {
        start.unwrap();
    }
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), query.fetch_snapshot(),)
            .await
            .unwrap()
            .is_err(),
        "{failure:?}"
    );
    assert_eq!(query.status().await, ComponentStatus::Error);
    drop(query);
    core.shutdown().await.unwrap();
    drop(core);
    let before = logical_rocks_state(tmp.path(), "bootstrap-q");
    let core = build_e2e_lib("bootstrap-source-reopen", &tmp, None)
        .await
        .unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(RecoveryPolicy::ExistingOnly))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    assert!(
        core.start_query("bootstrap-q").await.is_err(),
        "{failure:?}"
    );
    assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
    drop(core);
    assert_eq!(logical_rocks_state(tmp.path(), "bootstrap-q"), before);
}

#[rstest::rstest]
#[case::fence_write(Fault::FenceWrite)]
#[case::fence_write_then_error(Fault::FenceWriteAfter)]
#[case::fence_read(Fault::FenceRead)]
#[case::begin(Fault::Begin)]
#[case::rows(Fault::Rows)]
#[case::rows_success_noop(Fault::RowsNoop)]
#[case::checkpoint(Fault::Checkpoint)]
#[case::checkpoint_success_noop(Fault::CheckpointNoop)]
#[case::sequence(Fault::Sequence)]
#[case::sequence_success_noop(Fault::SequenceNoop)]
#[case::commit(Fault::Commit)]
#[case::commit_then_error(Fault::CommitAfter)]
#[case::commit_success_noop(Fault::CommitNoop)]
#[case::readback_checkpoints(Fault::ReadCheckpoints)]
#[case::readback_rows(Fault::ReadRows)]
#[case::readback_sequence(Fault::ReadSequence)]
#[case::readback_generation(Fault::ReadGeneration)]
#[case::publish(Fault::Publish)]
#[case::publish_then_error(Fault::PublishAfter)]
#[case::publish_success_noop(Fault::PublishNoop)]
#[case::published_read(Fault::PublishRead)]
#[tokio::test]
#[serial]
async fn test_bootstrap_publication_failure_never_exposes_ready(#[case] fault: Fault) {
    let tmp = tempfile::TempDir::new().unwrap();
    let probe = Arc::new(Probe::new(Some(fault)));
    let core = publication_core(&tmp, probe.clone()).await;
    add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(RecoveryPolicy::Strict))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), query.fetch_snapshot())
        .await
        .unwrap();
    assert!(snapshot.is_err(), "{fault:?}");
    assert_eq!(query.status().await, ComponentStatus::Error, "{fault:?}");
    assert!(
        probe.trace.lock().unwrap().hit,
        "fault was not reached: {fault:?}"
    );
    drop(query);
    core.shutdown().await.unwrap();
    drop(core);
    let before = logical_rocks_state(tmp.path(), "bootstrap-q");

    let core = build_e2e_lib("bootstrap-reopen", &tmp, None).await.unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(RecoveryPolicy::ExistingOnly))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let start = core.start_query("bootstrap-q").await;
    if matches!(fault, Fault::PublishAfter | Fault::PublishRead) {
        start.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let recovered = query.fetch_snapshot().await.unwrap();
        assert_eq!(recovered.as_of_sequence, 0);
        assert_eq!(recovered.output_generation, 0);
        assert_eq!(
            recovered.to_vec(),
            vec![serde_json::json!({"id": "seed", "value": 1})]
        );
        assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
        drop(query);
        assert_eq!(subscriptions.load(Ordering::Acquire), 1);
    } else {
        assert!(start.is_err(), "incomplete image was recovered: {fault:?}");
        assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    }
    core.shutdown().await.unwrap();
    drop(core);
    assert_eq!(
        logical_rocks_state(tmp.path(), "bootstrap-q"),
        before,
        "{fault:?}"
    );
}

#[tokio::test]
#[serial]
async fn test_bootstrap_panic_reports_error_and_never_reports_a_successful_stop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let probe = Arc::new(Probe::new(Some(Fault::RowsPanic)));
    let core = publication_core(&tmp, probe.clone()).await;
    add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(RecoveryPolicy::Strict))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), query.fetch_snapshot(),)
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(query.status().await, ComponentStatus::Error);
    assert!(probe.trace.lock().unwrap().hit);
    assert!(core.stop_query("bootstrap-q").await.is_err());
    assert_ne!(query.status().await, ComponentStatus::Stopped);
    assert!(query.stop().await.is_err());
    assert_ne!(query.status().await, ComponentStatus::Stopped);
    drop(query);
    assert!(core.shutdown().await.is_err());
    assert!(core.graph_update_handle.lock().await.is_some());
    assert!(probe
        .rows
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upgrade()
        .is_some());
    assert!(core.shutdown().await.is_err());
}

#[rstest::rstest]
#[case::cancelled(false, false)]
#[case::timed_out(true, false)]
#[case::manager_cancelled(false, true)]
#[tokio::test]
#[serial]
async fn test_shutdown_retries_unfinished_publication_without_releasing_handles(
    #[case] time_out: bool,
    #[case] use_manager: bool,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let gate = Arc::new(PublicationGate {
        point: Pause::Commit,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut probe = Probe::new(None);
    probe.gate = Some(gate.clone());
    let probe = Arc::new(probe);
    let core = publication_core(&tmp, probe.clone()).await;
    let mut source = E2eTestSource::new("bootstrap-src", true).unwrap();
    source.bootstrap = Some(vec![bootstrap_node()]);
    let removed = source.position_handle_removed_handle();
    core.add_source(source).await.unwrap();
    core.add_query(bootstrap_query(RecoveryPolicy::Strict))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    {
        let shutdown = core.shutdown();
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        tokio::select! {
            result = &mut shutdown => panic!("shutdown finished before joining publication: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while query.status().await != ComponentStatus::Stopping {
                    tokio::task::yield_now().await;
                }
            }) => {
                result.unwrap();
            }
        }
        if time_out {
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
            assert!(shutdown.await.is_err());
            tokio::time::resume();
        }
    }
    assert_eq!(removed.load(Ordering::Acquire), 0);
    if time_out {
        assert!(core.graph_update_handle.lock().await.is_some());
    } else {
        // Main's owned shutdown continues after its caller is cancelled.
        assert!(core.graph_update_handle.try_lock().is_err());
    }
    assert!(probe
        .rows
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upgrade()
        .is_some());
    {
        let retry = async {
            if use_manager {
                core.query_manager().stop_all().await
            } else {
                core.shutdown().await.map_err(anyhow::Error::from)
            }
        };
        tokio::pin!(retry);
        assert!(futures::poll!(&mut retry).is_pending());
        gate.release.notify_one();
        retry.await.unwrap();
    }
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(removed.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
    assert!(core.graph_update_handle.lock().await.is_none());
    core.shutdown().await.unwrap();
}

#[rstest::rstest]
#[case::commit(Pause::Commit)]
#[case::publish(Pause::Publish)]
#[tokio::test]
#[serial]
async fn test_stop_joins_publication_before_releasing_source_ownership(#[case] point: Pause) {
    let tmp = tempfile::TempDir::new().unwrap();
    let gate = Arc::new(PublicationGate {
        point,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut probe = Probe::new(None);
    probe.gate = Some(gate.clone());
    let probe = Arc::new(probe);
    let core = publication_core(&tmp, probe.clone()).await;
    let mut source = E2eTestSource::new("bootstrap-src", true).unwrap();
    source.bootstrap = Some(vec![bootstrap_node()]);
    let removed = source.position_handle_removed_handle();
    core.add_source(source).await.unwrap();
    core.add_query(bootstrap_query(RecoveryPolicy::Strict))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    core.start_query("bootstrap-q").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    let query = core
        .query_manager()
        .get_query_instance("bootstrap-q")
        .await
        .unwrap();
    {
        let stop = core.stop_query("bootstrap-q");
        tokio::pin!(stop);
        assert!(futures::poll!(&mut stop).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while query.status().await != ComponentStatus::Stopping {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(futures::poll!(&mut stop).is_pending());
        assert_eq!(removed.load(Ordering::Acquire), 0);
        assert!(query.fetch_snapshot().await.is_err());
        let store = probe
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let rows = probe
            .rows
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        if matches!(point, Pause::Commit) {
            assert_eq!(
                store.read_result_sequence("bootstrap-q").await.unwrap(),
                None
            );
            // Checkpoint reads include this active transaction's staged handover.
            assert_eq!(
                store.read_all_checkpoints().await.unwrap(),
                HashMap::from([(
                    "bootstrap-src".to_string(),
                    SourceCheckpoint::new(0, Some(Bytes::from_static(b"accepted-cut"))),
                )])
            );
            assert!(rows.read_snapshot("bootstrap-q").await.unwrap().is_empty());
        }
        drop(rows);
        drop(store);
        gate.release.notify_one();
        stop.await.unwrap();
    }
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(removed.load(Ordering::Acquire), 1);
    drop(query);
    core.shutdown().await.unwrap();
    drop(core);

    let core = build_e2e_lib("bootstrap-stopped", &tmp, None)
        .await
        .unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(RecoveryPolicy::ExistingOnly))
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let start = core.start_query("bootstrap-q").await;
    if matches!(point, Pause::Publish) {
        start.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        assert_eq!(
            query.fetch_snapshot().await.unwrap().to_vec(),
            vec![serde_json::json!({"id": "seed", "value": 1})]
        );
        drop(query);
        assert_eq!(subscriptions.load(Ordering::Acquire), 1);
    } else {
        assert!(start.is_err());
        assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    }
    core.shutdown().await.unwrap();
}

#[rstest::rstest]
#[case::strict_opt_in(true, RecoveryPolicy::Strict)]
#[case::existing_only_default(false, RecoveryPolicy::ExistingOnly)]
#[case::existing_only_opt_in(true, RecoveryPolicy::ExistingOnly)]
#[tokio::test]
#[serial]
async fn test_legacy_empty_zero_image_is_not_a_published_snapshot(
    #[case] certification: bool,
    #[case] policy: RecoveryPolicy,
    #[values(false, true)] explicit_zero: bool,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    {
        let provider = RocksDbIndexProvider::new(tmp.path(), false, false);
        let indexes = provider.create_indexes("bootstrap-q").await.unwrap();
        let checkpoints = indexes.checkpoint_store.as_ref().unwrap();
        let config = bootstrap_query(RecoveryPolicy::Strict);
        checkpoints
            .write_config_hash(crate::queries::compute_config_hash(&config))
            .await
            .unwrap();
        if explicit_zero {
            checkpoints
                .write_result_sequence("bootstrap-q", 0)
                .await
                .unwrap();
        }
        indexes.set.session_control.begin().await.unwrap();
        checkpoints
            .stage_checkpoint(
                "bootstrap-src",
                0,
                Some(&Bytes::from_static(b"accepted-cut")),
            )
            .await
            .unwrap();
        indexes.set.session_control.commit().await.unwrap();
    }
    let before = logical_rocks_state(tmp.path(), "bootstrap-q");
    let core = build_e2e_lib_with_certification("legacy-bootstrap", &tmp, None, certification)
        .await
        .unwrap();
    let subscriptions = add_bootstrap_source(&core).await;
    core.add_query(bootstrap_query(policy)).await.unwrap();
    core.start().await.unwrap();
    core.start_source("bootstrap-src").await.unwrap();
    let error = core.start_query("bootstrap-q").await.unwrap_err();
    assert!(format!("{error:#}").contains("no published durable initial snapshot"));
    assert_eq!(subscriptions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
    drop(core);
    assert_eq!(logical_rocks_state(tmp.path(), "bootstrap-q"), before);
}

#[rstest::rstest]
#[tokio::test]
#[serial]
async fn test_complete_bootstrap_admission_is_explicit(
    #[values(false, true)] certification: bool,
    #[values(false, true)] mixed: bool,
    #[values(RecoveryPolicy::Strict, RecoveryPolicy::AutoReset)] policy: RecoveryPolicy,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib_with_certification("completeness-policy", &tmp, None, certification)
        .await
        .unwrap();
    core.add_source(E2eTestSource::new("missing-stream", true).unwrap())
        .await
        .unwrap();
    let mut config = bootstrap_query(policy);
    config.sources = vec![source_config("missing-stream")];
    if mixed {
        add_bootstrap_source(&core).await;
        config.sources.push(source_config("bootstrap-src"));
    }
    let hash = crate::queries::compute_config_hash(&config);
    core.add_query(config).await.unwrap();
    core.start().await.unwrap();
    core.start_source("missing-stream").await.unwrap();
    if mixed {
        core.start_source("bootstrap-src").await.unwrap();
    }
    let start = core.start_query("bootstrap-q").await;
    if certification {
        assert!(start.is_err(), "native admission must refuse (None, None)");
    } else {
        start.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot =
            tokio::time::timeout(std::time::Duration::from_secs(5), query.fetch_snapshot())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.config_hash, hash);
        assert_eq!(snapshot.to_vec().len(), usize::from(mixed));
    }
    core.shutdown().await.unwrap();
}

fn source_config(source_id: &str) -> crate::config::SourceSubscriptionConfig {
    crate::config::SourceSubscriptionConfig {
        source_id: source_id.into(),
        nodes: vec![],
        relations: vec![],
        pipeline: vec![],
    }
}

#[rstest::rstest]
#[tokio::test]
#[serial]
async fn test_default_mode_recognizes_certified_images(#[values(false, true)] bootstrap: bool) {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut expected = None;
    for certification in [true, false] {
        let core =
            build_e2e_lib_with_certification("certified-to-default", &tmp, None, certification)
                .await
                .unwrap();
        add_bootstrap_source(&core).await;
        let mut config = bootstrap_query(RecoveryPolicy::Strict);
        config.enable_bootstrap = bootstrap;
        let hash = crate::queries::compute_config_hash(&config);
        core.add_query(config).await.unwrap();
        core.start().await.unwrap();
        core.start_source("bootstrap-src").await.unwrap();
        core.start_query("bootstrap-q").await.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.config_hash, hash);
        assert_eq!(snapshot.output_generation, 0);
        if let Some(expected) = &expected {
            assert_eq!(&snapshot.to_vec(), expected);
        } else {
            expected = Some(snapshot.to_vec());
        }
        drop(query);
        core.shutdown().await.unwrap();
        drop(core);
        let state = logical_rocks_state(tmp.path(), "bootstrap-q");
        assert!(!state.is_empty());
    }
}

#[tokio::test]
#[serial]
async fn test_no_diff_after_bootstrap_advances_checkpoint_without_advancing_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut initial = None;
    for (policy, sequence, value, position) in [
        (RecoveryPolicy::Strict, 7, 1, b"no-diff".as_slice()),
        (RecoveryPolicy::ExistingOnly, 8, 2, b"next-cut".as_slice()),
    ] {
        let core = build_e2e_lib("bootstrap-no-diff", &tmp, None)
            .await
            .unwrap();
        let mut source = E2eTestSource::new("bootstrap-src", true).unwrap();
        source.bootstrap = Some(vec![bootstrap_node()]);
        let tx = source.event_sender();
        let handle = source.last_position_handle.clone();
        let resume_from = source.last_resume_from();
        let resume_sequence = source.last_resume_sequence();
        core.add_source(source).await.unwrap();
        core.add_query(bootstrap_query(policy)).await.unwrap();
        core.start().await.unwrap();
        core.start_source("bootstrap-src").await.unwrap();
        core.start_query("bootstrap-q").await.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        let identity = (
            snapshot.as_of_sequence,
            snapshot.config_hash,
            snapshot.output_generation,
            snapshot
                .stream_keyed()
                .collect::<std::collections::BTreeMap<_, _>>()
                .await,
        );
        if let Some(expected) = &initial {
            assert_eq!(&identity, expected);
            assert_eq!(
                *resume_from.read().await,
                Some(Bytes::from_static(b"no-diff"))
            );
            assert_eq!(*resume_sequence.read().await, Some(7));
        } else {
            assert_eq!(identity.0, 0);
            assert_eq!(identity.2, 0);
            assert_eq!(
                identity.3.values().cloned().collect::<Vec<_>>(),
                vec![serde_json::json!({"id": "seed", "value": 1})]
            );
            initial = Some(identity);
        }

        let drasi_core::models::SourceChange::Insert { mut element } = bootstrap_node() else {
            unreachable!()
        };
        let drasi_core::models::Element::Node {
            metadata,
            properties,
        } = &mut element
        else {
            unreachable!()
        };
        metadata.effective_from = sequence;
        properties.insert("value", drasi_core::models::ElementValue::Integer(value));
        let event = SourceEventWrapper::new(
            "bootstrap-src".into(),
            crate::channels::events::SourceEvent::Change(
                drasi_core::models::SourceChange::Update { element },
            ),
            chrono::Utc::now(),
            sequence,
        )
        .with_source_position(Bytes::copy_from_slice(position));
        tx.read()
            .await
            .as_ref()
            .unwrap()
            .send(Arc::new(event))
            .await
            .unwrap();
        let handle = handle.read().await.clone().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while handle.load(Ordering::Acquire) != sequence {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        let outbox = query.fetch_outbox(0).await.unwrap();
        let expected_sequence = u64::from(value == 2);
        assert_eq!(snapshot.as_of_sequence, expected_sequence);
        assert_eq!(outbox.latest_sequence, expected_sequence);
        assert_eq!(outbox.results.len(), expected_sequence as usize);
        assert_eq!(
            snapshot
                .stream_keyed()
                .collect::<std::collections::BTreeMap<_, _>>()
                .await,
            initial
                .as_ref()
                .unwrap()
                .3
                .keys()
                .map(|key| (*key, serde_json::json!({"id": "seed", "value": value})))
                .collect()
        );
        drop(query);
        core.shutdown().await.unwrap();
        drop(core);
    }
}

#[tokio::test]
#[serial]
async fn test_bootstrap_publishes_one_complete_multisource_aggregate() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut initial = None;
    for policy in [RecoveryPolicy::Strict, RecoveryPolicy::ExistingOnly] {
        let core = build_e2e_lib("bootstrap-multisource", &tmp, None)
            .await
            .unwrap();
        for id in ["bootstrap-src", "other-src"] {
            let mut change = bootstrap_node();
            let drasi_core::models::SourceChange::Insert {
                element: drasi_core::models::Element::Node { metadata, .. },
            } = &mut change
            else {
                unreachable!()
            };
            metadata.reference = drasi_core::models::ElementReference::new(id, "seed");
            let mut source = E2eTestSource::new(id, true).unwrap();
            source.bootstrap = Some(vec![change]);
            core.add_source(source).await.unwrap();
        }
        core.add_query(
            Query::cypher("bootstrap-q")
                .query("MATCH (n:Node) RETURN sum(n.value) AS total")
                .from_source("bootstrap-src")
                .from_source("other-src")
                .auto_start(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .with_recovery_policy(policy)
                .build(),
        )
        .await
        .unwrap();
        core.start().await.unwrap();
        core.start_source("bootstrap-src").await.unwrap();
        core.start_source("other-src").await.unwrap();
        core.start_query("bootstrap-q").await.unwrap();
        let query = core
            .query_manager()
            .get_query_instance("bootstrap-q")
            .await
            .unwrap();
        let snapshot = query.fetch_snapshot().await.unwrap();
        assert_eq!(snapshot.to_vec(), vec![serde_json::json!({"total": 2.0})]);
        assert_eq!(snapshot.as_of_sequence, 0);
        assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
        let identity = (
            snapshot.config_hash,
            snapshot.output_generation,
            snapshot
                .stream_keyed()
                .collect::<std::collections::BTreeMap<_, _>>()
                .await,
        );
        if let Some(expected) = &initial {
            assert_eq!(&identity, expected);
        } else {
            initial = Some(identity);
        }
        drop(query);
        core.shutdown().await.unwrap();
        drop(core);
    }
}
