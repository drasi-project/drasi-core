// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::durable::destinations::{codec, held_indexes, CommitGate};
use drasi_core::{
    computation::{ComputationIndexes, ComputationTransaction},
    interface::{FailureMode, IndexError, OutboxPageLimits, StorageDurability},
};
use std::{collections::BTreeMap, sync::atomic::Ordering};

const ROOT: &str = "DRASI_MIDDLE_REBUILD_CRASH_ROOT";
const PHASE: &str = "DRASI_MIDDLE_REBUILD_CRASH_PHASE";
const MIDDLE: &str = "DRASI_MIDDLE_REBUILD_CRASH_MIDDLE";

impl Middle {
    fn from_env() -> Self {
        match std::env::var(MIDDLE).as_deref() {
            Ok("middleware") => Self::Middleware,
            Ok("view") => Self::View,
            Ok("geometry") => Self::Geometry,
            _ => Self::Query,
        }
    }

    /// The middle's output after its rebuild or a later input: one keyed
    /// update of the single total, at `position` in its output history.
    fn check_update(
        self,
        output: &ChangeEnvelope,
        before: f64,
        after: f64,
        position: u64,
    ) -> Result<()> {
        if self == Self::Query {
            if position == 1 {
                return check_correction(output);
            }
            return aggregate(
                &[OutputEnvelope::new(PortId::try_new("out")?, output.clone())],
                Some(before),
                after,
                position,
            );
        }
        anyhow::ensure!(
            matches!(
                output.changes().operations(),
                [ChangeOperation::Updated { .. }]
            ),
            "snapshot was forwarded instead of a keyed correction"
        );
        assert_eq!(middleware_change(output)?, (after, position));
        Ok(())
    }
}

struct HeldProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    gate: Arc<CommitGate>,
}

#[async_trait]
impl ComputationIndexProvider for HeldProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &ComponentId,
    ) -> Result<ComputationIndexes, IndexError> {
        held_indexes(
            self.inner.create_indexes(graph, component).await?,
            Some(self.gate.clone()),
        )
        .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))
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

struct Branch {
    channel: Arc<QosChannel>,
    seen: Arc<Mutex<Vec<ChangeEnvelope>>>,
    changed: Arc<Notify>,
    sink: ComponentHandle,
}

impl Branch {
    async fn receipt(&self) -> Result<ChangeEnvelope> {
        loop {
            let notified = self.changed.notified();
            if let Some(first) = self.seen.lock().expect("receipt capture").first().cloned() {
                return Ok(first);
            }
            notified.await;
        }
    }
}

async fn branches(
    graph: &Graph,
    middle: &Child,
    root: &Path,
    gate: Option<Arc<CommitGate>>,
) -> Result<Vec<Branch>> {
    let mut branches = Vec::new();
    let mut edges = Vec::new();
    for (index, id) in [
        "ae020000-0000-4000-8000-000000000001",
        "ae020000-0000-4000-8000-000000000002",
    ]
    .iter()
    .enumerate()
    {
        let id = ComponentId::try_new(*id)?;
        let name = format!("rebuild-branch-{index}");
        let resource = ResourceId::try_new(name.clone())?;
        let indexes = held_indexes(
            provider(&root.join("journals"))?
                .create_indexes(GRAPH, &id)
                .await?,
            (index == 1).then(|| gate.clone()).flatten(),
        )?;
        let channel = QosChannel::persistent_with_recovery(
            QosChannelDefinition {
                stream: StreamId::try_new("downstream/out")?,
                capacity: 4.try_into()?,
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([("target".into(), SubscriptionStart::Earliest)]),
            },
            indexes,
            codec()?,
            "output",
            QosRecoveryOptions::Replay(ReplayOptions {
                failure_scope: FailureMode::ProcessRestart,
                receipt_capacity: 4.try_into()?,
            }),
        )
        .await?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let sink = graph
            .core
            .add_computation_component(
                ComponentAddition::new(
                    ComponentName::try_new(name.clone())?,
                    ConstructedComponent::sink(Box::new(Sink {
                        descriptor: ComponentDescriptor::try_new(
                            id.clone(),
                            vec![port_descriptor("in", PortDirection::Input)?],
                        )?,
                        received: seen.clone(),
                        notification: changed.clone(),
                    })),
                )
                .with_resource(
                    ResourceSpecification {
                        id: resource.clone(),
                        role: ResourceRole::StateStore,
                        ownership: ResourceOwnership::Graph,
                        binding: name.into(),
                    },
                    channel.resource(),
                )
                .auto_start(false),
            )
            .await?;
        edges.push(DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(middle.handle.id().clone(), PortId::try_new("out")?),
                Endpoint::new(id, PortId::try_new("in")?),
            ),
            policy: Default::default(),
            pipe: DesiredPipe::Qos(channel.definition().pipe(resource, "target")),
        });
        branches.push(Branch {
            channel,
            seen,
            changed,
            sink,
        });
    }
    crate::bootstrap::connect_relationships(&graph.core, edges).await?;
    for branch in &branches {
        branch.sink.start().await?;
    }
    Ok(branches)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "process-exit fixture launched by the atomic rebuild cases"]
async fn rebuild_exit_child() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        return Ok(());
    };
    let fanout = std::env::var(PHASE)? == "fanout";
    let database = root.join("source.sqlite");
    sqlite::create_database(&database)?;
    let middle_kind = Middle::from_env();
    let graph = middle_kind
        .graph(&database, &root.join("upstream"), 1)
        .await?;
    graph.start().await?;
    let gate = Arc::new(CommitGate {
        // The middle's first emission exists only in its installation transaction.
        checkpoint: (!fanout).then(|| (metadata_checkpoint("\0transformer:emission:v1"), 1)),
        ..Default::default()
    });
    let mut indexes: Arc<dyn ComputationIndexProvider> = provider(&root.join("middle"))?;
    if !fanout {
        indexes = Arc::new(HeldProvider {
            inner: indexes,
            gate: gate.clone(),
        });
    }
    let middle = middle_kind.child(&graph, indexes).await?;
    let leaf = add_leaf(&graph, &middle).await?;
    let branches = branches(&graph, &middle, &root, fanout.then(|| gate.clone())).await?;
    middle.handle.start().await?;
    leaf.start().await?;
    let snapshot = middle.snapshot(&graph).await?;
    assert_eq!(middle_kind.total(&snapshot)?, 30.0);
    std::fs::write(
        root.join("installed-cut.json"),
        serde_json::to_vec(snapshot.cursor())?,
    )?;
    miss_two_updates(&graph, &middle).await?;
    gate.armed.store(true, Ordering::Release);
    let restart = middle.handle.start();
    tokio::pin!(restart);
    tokio::select! {
        result = &mut restart => {
            result?;
            anyhow::ensure!(fanout, "startup escaped the installation gate");
            gate.entered.notified().await;
        },
        _ = gate.entered.notified() => {}
    }
    if fanout {
        let receipt = branches[0].receipt().await?;
        middle_kind.check_update(&receipt, 30.0, 32.0, 1)?;
        assert_eq!(branches[0].channel.progress().await?.accepted, 1);
    } else {
        assert_eq!(branches[0].channel.progress().await?.accepted, 0);
        assert_eq!(branches[1].channel.progress().await?.accepted, 0);
    }
    std::process::exit(if fanout { 94 } else { 93 })
}

fn check_correction(receipt: &ChangeEnvelope) -> Result<()> {
    let rows = QueryChangeCodec::decode_evaluation(receipt)?;
    let [QueryPartEvaluationContext::Updating { before, after, .. }] = rows.as_slice() else {
        anyhow::bail!("snapshot was forwarded instead of a keyed correction")
    };
    assert_eq!(
        QueryChangeCodec::row_values_to_json(before)["total"].as_f64(),
        Some(30.0)
    );
    assert_eq!(
        QueryChangeCodec::row_values_to_json(after)["total"].as_f64(),
        Some(32.0)
    );
    assert_eq!(QueryChangeCodec::query_sequence(receipt)?, 1);
    Ok(())
}

async fn recovery(phase: &str, middle_kind: Middle) -> Result<()> {
    let root = tempfile::tempdir()?;
    let fanout = phase == "fanout";
    let child = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::bootstrap::coordinated::chain::rebuild_crash::rebuild_exit_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, root.path())
            .env(PHASE, phase)
            .env(MIDDLE, middle_kind.name())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        child.status.code() == Some(if fanout { 94 } else { 93 }),
        "rebuild child failed: {}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let cut: OutputCursor =
        serde_json::from_slice(&std::fs::read(root.path().join("installed-cut.json"))?)?;
    let partition =
        drasi_core_types::OutputPartition::Component(cut.producer().component_id().clone());
    let owner = ComputationTransaction::try_new(
        provider(&root.path().join("middle"))?
            .create_indexes(cut.producer().graph_id(), cut.producer().component_id())
            .await?,
    )?;
    let bundle = owner.resources();
    let store = bundle.checkpoint_store().context("missing actual store")?;
    assert_eq!(
        store.read_result_sequence(&partition).await?,
        Some(u64::from(fanout))
    );
    let limits = OutboxPageLimits {
        max_records: 2.try_into()?,
        max_bytes: (1 << 20).try_into()?,
    };
    let input_key =
        drasi_core_types::CheckpointKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?)
            .associated("\0computation:processing-input:v1:\"in\"");
    assert_eq!(
        store
            .read_checkpoint(&input_key)
            .await?
            .context("missing installed cut")?
            .sequence,
        if fanout { 2 } else { 0 }
    );
    let rows = bundle
        .live_results_writer()
        .context("missing live rows")?
        .read_snapshot(cut.producer().component_id())
        .await?;
    let [(_, value)] = rows.as_slice() else {
        anyhow::bail!("installed view must contain one total")
    };
    let image = drasi_computation_contracts::ElementImage::decode(value, value.len())?;
    let value = drasi_core_types::data_value::decode_variables(image.properties().clone())?;
    assert_eq!(
        QueryChangeCodec::row_values_to_json(&value)["total"].as_f64(),
        Some(if fanout { 32.0 } else { 30.0 })
    );
    let pending = bundle
        .outbox_writer()
        .context("missing actual outbox")?
        .read_page(&partition, 0, limits)
        .await?;
    assert_eq!(pending.len(), usize::from(fanout));
    if fanout {
        middle_kind.check_update(
            &drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?
                .decode(&pending[0].1)?,
            30.0,
            32.0,
            1,
        )?;
    }
    owner.shutdown().await?;
    drop(owner);

    let graph = middle_kind
        .graph(
            &root.path().join("source.sqlite"),
            &root.path().join("upstream"),
            2,
        )
        .await?;
    let run = async {
        graph.start().await?;
        let middle = middle_kind
            .child(&graph, provider(&root.path().join("middle"))?)
            .await?;
        let leaf = add_leaf(&graph, &middle).await?;
        let branches = branches(&graph, &middle, root.path(), None).await?;
        assert_eq!(
            branches[0].channel.progress().await?.accepted,
            u64::from(fanout)
        );
        assert_eq!(branches[1].channel.progress().await?.accepted, 0);
        middle.handle.start().await?;
        let owed = branches[1].receipt().await?;
        middle_kind.check_update(&owed, 30.0, 32.0, 1)?;
        if !fanout {
            middle_kind.check_update(&branches[0].receipt().await?, 30.0, 32.0, 1)?;
        }
        assert_eq!(branches[0].channel.progress().await?.accepted, 1);
        assert_eq!(branches[1].channel.progress().await?.accepted, 1);
        let recovered = middle.snapshot(&graph).await?;
        assert_eq!(recovered.cursor(), &cut.at_sequence(1));
        assert_eq!(middle_kind.total(&recovered)?, 32.0);
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        assert_eq!(middle.progress.snapshot().checkpoints[&key].sequence, 2);
        leaf.start().await?;
        // A later input exercises the installed evaluator indexes, not just
        // the independently readable output rows and the delivery journal.
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":14}}]))
            .await?;
        let next = middle.output(1).await?;
        middle_kind.check_update(&next, 32.0, 34.0, 2)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    run?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_exit_before_install_keeps_old_view_and_retries() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), recovery("install", Middle::Query)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_exit_after_one_journal_recovers_the_owed_difference() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), recovery("fanout", Middle::Query)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_rebuild_exit_before_install_keeps_old_view_and_retries() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(60),
        recovery("install", Middle::Middleware),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middleware_rebuild_exit_after_one_journal_recovers_the_owed_difference() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(60),
        recovery("fanout", Middle::Middleware),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_rebuild_exit_before_install_keeps_old_view_and_retries() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), recovery("install", Middle::View)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_rebuild_exit_after_one_journal_recovers_the_owed_difference() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), recovery("fanout", Middle::View)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn geometry_rebuild_exit_before_install_keeps_old_view_and_retries() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(60),
        recovery("install", Middle::Geometry),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn geometry_rebuild_exit_after_one_journal_recovers_the_owed_difference() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(60),
        recovery("fanout", Middle::Geometry),
    )
    .await?
}
