// Copyright 2026 The Drasi Authors.
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

#![cfg(test)]

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult},
    channels::{BootstrapEvent, BootstrapEventSender, SubscriptionResponse},
    computation::v1::{
        LegacyPluginServices, LegacySourceSubscription, SourcePluginHost,
        SourceSubscriptionOptions, StreamId,
    },
    queries::SnapshotResponse,
    sources::SourceError,
    wal::{WalError, WalProvider},
    CapacityPolicy, ComponentStatus, DispatchMode, DrasiLib, DurabilityConfig, Query,
    RecoveryPolicy, Source, SourceRuntimeContext, SourceSubscriptionSettings, StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle,
};
use drasi_wal_redb::RedbWalProvider;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::time::{sleep, timeout};

const QUERY: &str = "names";
const DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct Subscription {
    settings: SourceSubscriptionSettings,
    position: Option<Weak<AtomicU64>>,
    unavailable: bool,
}

#[derive(Default)]
struct Observations {
    subscriptions: Mutex<Vec<Subscription>>,
    bootstraps: AtomicUsize,
}

impl Observations {
    fn subscriptions(&self) -> Vec<Subscription> {
        self.subscriptions.lock().unwrap().clone()
    }

    fn latest(&self) -> Subscription {
        self.subscriptions().last().unwrap().clone()
    }

    fn confirmed(&self) -> Option<u64> {
        self.latest()
            .position
            .and_then(|position| position.upgrade())
            .map(|position| position.load(Ordering::Acquire))
    }
}

struct ObservedApplication {
    inner: ApplicationSource,
    observations: Arc<Observations>,
    dispatch_mode: DispatchMode,
}

#[async_trait]
impl Source for ObservedApplication {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn type_name(&self) -> &str {
        self.inner.type_name()
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.inner.properties()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        // Exercise the runtime's declared lossy admission policy independently
        // of the production source's dedicated WAL replay receiver.
        self.dispatch_mode
    }

    fn supports_replay(&self) -> bool {
        self.inner.supports_replay()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.inner.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.inner.start().await
    }

    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }

    async fn status(&self) -> ComponentStatus {
        self.inner.status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let result = self.inner.subscribe(settings.clone()).await;
        let unavailable =
            result
                .as_ref()
                .err()
                .and_then(|error| match error.downcast_ref::<WalError>() {
                    Some(WalError::PositionUnavailable {
                        source_id,
                        requested,
                        oldest_available,
                    }) => Some(SourceError::PositionUnavailable {
                        source_id: source_id.clone(),
                        requested: Bytes::copy_from_slice(&requested.to_be_bytes()),
                        earliest_available: oldest_available
                            .map(|sequence| Bytes::copy_from_slice(&sequence.to_be_bytes())),
                    }),
                    _ => None,
                });
        self.observations
            .subscriptions
            .lock()
            .unwrap()
            .push(Subscription {
                settings,
                position: result
                    .as_ref()
                    .ok()
                    .and_then(|response| response.position_handle.as_ref().map(Arc::downgrade)),
                unavailable: unavailable.is_some(),
            });
        // Real retention gaps exercise the Source contract's typed error, not
        // a synthetic failure counter or a claim that replay is unsupported.
        if let Some(error) = unavailable {
            return Err(error.into());
        }
        result
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.inner.remove_position_handle(query_id).await;
    }

    async fn on_subscriptions_complete(&self) {
        self.inner.on_subscriptions_complete().await;
    }
}

#[derive(Default)]
struct Snapshot {
    changes: Vec<SourceChange>,
    position: u64,
}

struct SnapshotProvider {
    snapshot: Arc<Mutex<Snapshot>>,
    observations: Arc<Observations>,
}

#[async_trait]
impl BootstrapProvider for SnapshotProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &BootstrapContext,
        events: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        self.observations.bootstraps.fetch_add(1, Ordering::AcqRel);
        let (changes, position) = {
            let snapshot = self.snapshot.lock().unwrap();
            (snapshot.changes.clone(), snapshot.position)
        };
        let event_count = changes.len();
        for change in changes {
            events
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change,
                    timestamp: chrono::Utc::now(),
                    sequence: context.next_sequence(),
                })
                .await?;
        }
        Ok(BootstrapResult {
            event_count,
            source_position: Some(Bytes::copy_from_slice(&position.to_be_bytes())),
        })
    }
}

struct Input {
    handle: ApplicationSourceHandle,
    observations: Arc<Observations>,
    snapshot: Arc<Mutex<Snapshot>>,
}

struct Fixture {
    core: DrasiLib,
    wal: Arc<RedbWalProvider>,
    inputs: BTreeMap<String, Input>,
    directory: tempfile::TempDir,
}

fn application(id: &str) -> Result<(ApplicationSource, ApplicationSourceHandle)> {
    ApplicationSource::new(
        id,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: Some(DurabilityConfig {
                enabled: true,
                max_events: 128,
                capacity_policy: CapacityPolicy::RejectIncoming,
            }),
        },
    )
}

impl Fixture {
    async fn new(
        policy: RecoveryPolicy,
        dispatch_mode: DispatchMode,
        sources: &[&str],
        bootstrap: bool,
    ) -> Result<Self> {
        let root = "target/computation-source-recovery";
        std::fs::create_dir_all(root)?;
        let directory = tempfile::Builder::new()
            .prefix("runtime-")
            .tempdir_in(root)?;
        let wal = Arc::new(RedbWalProvider::new(directory.path().join("wal")));
        let mut builder = DrasiLib::builder()
            .with_id("source-recovery")
            .with_wal_provider(wal.clone())
            .with_index_provider(
                "rocks",
                Arc::new(RocksDbIndexProvider::new(
                    directory.path().join("indexes"),
                    false,
                    false,
                )),
            );
        let mut query = Query::cypher(QUERY)
            .query("MATCH (p:Person) RETURN p.name AS name")
            .enable_bootstrap(bootstrap)
            .with_storage_backend(StorageBackendRef::Named("rocks".into()))
            .with_recovery_policy(policy);
        let mut inputs = BTreeMap::new();
        for id in sources {
            let (source, handle) = application(id)?;
            let observations = Arc::new(Observations::default());
            let snapshot = Arc::new(Mutex::new(Snapshot::default()));
            if bootstrap {
                source
                    .set_bootstrap_provider(Box::new(SnapshotProvider {
                        snapshot: snapshot.clone(),
                        observations: observations.clone(),
                    }))
                    .await;
            }
            builder = builder.with_source(ObservedApplication {
                inner: source,
                observations: observations.clone(),
                dispatch_mode,
            });
            query = query.from_source(*id);
            inputs.insert(
                (*id).to_owned(),
                Input {
                    handle,
                    observations,
                    snapshot,
                },
            );
        }
        let core = builder.with_query(query.build()).build().await?;
        core.start().await?;
        wait_running(&core).await?;
        Ok(Self {
            core,
            wal,
            inputs,
            directory,
        })
    }

    async fn send(&self, source: &str, id: &str) -> Result<u64> {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source, id),
                    labels: vec![Arc::from("Person")].into(),
                    effective_from: chrono::Utc::now().timestamp_millis() as u64,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"name": id})),
            },
        };
        let input = &self.inputs[source];
        input.handle.send(change.clone()).await?;
        let position = self.wal.head_sequence(source).await?;
        let mut snapshot = input.snapshot.lock().unwrap();
        snapshot.changes.push(change);
        snapshot.position = position;
        Ok(position)
    }

    fn observations(&self, source: &str) -> &Observations {
        &self.inputs[source].observations
    }

    async fn wait_confirmed(&self, source: &str, sequence: u64) -> Result<()> {
        timeout(DEADLINE, async {
            while self.observations(source).confirmed() != Some(sequence) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| format!("source {source} was not acknowledged at {sequence}"))?;
        Ok(())
    }

    async fn restart(&self) -> Result<()> {
        timeout(DEADLINE, self.core.start_query(QUERY))
            .await
            .context("query restart timed out")??;
        wait_running(&self.core).await
    }

    async fn stop_query(&self) -> Result<()> {
        self.core.stop_query(QUERY).await?;
        wait_status(&self.core, ComponentStatus::Stopped).await
    }

    async fn shutdown(self) -> Result<()> {
        timeout(DEADLINE, self.core.shutdown())
            .await
            .context("shutdown timed out")??;
        drop(self.inputs);
        drop(self.core);
        drop(self.wal);
        self.directory.close()?;
        Ok(())
    }
}

async fn wait_running(core: &DrasiLib) -> Result<()> {
    wait_status(core, ComponentStatus::Running).await
}

async fn wait_status(core: &DrasiLib, expected: ComponentStatus) -> Result<()> {
    timeout(DEADLINE, async {
        loop {
            match core.get_query_status(QUERY).await? {
                status if status == expected => return Ok(()),
                ComponentStatus::Error => anyhow::bail!("query failed while starting"),
                _ => sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .with_context(|| format!("query did not reach {expected:?}"))?
}

async fn rows(core: &DrasiLib, expected: &[&str]) -> Result<SnapshotResponse> {
    let mut expected: Vec<_> = expected.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    timeout(DEADLINE, async {
        let query = core
            .query_manager()
            .get_query_instance(QUERY)
            .await
            .map_err(anyhow::Error::msg)?;
        loop {
            let snapshot = query.fetch_snapshot().await?;
            let mut actual: Vec<_> = snapshot
                .to_vec()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_owned())
                .collect();
            actual.sort();
            if actual == expected {
                return Ok(snapshot);
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("query did not recover rows {expected:?}"))?
}

#[tokio::test]
async fn ordinary_runtime_replays_uncheckpointed_wal_without_reset_in_both_dispatch_modes(
) -> Result<()> {
    for dispatch in [DispatchMode::Channel, DispatchMode::Broadcast] {
        for policy in [RecoveryPolicy::Strict, RecoveryPolicy::AutoReset] {
            let fixture = Fixture::new(policy, dispatch, &["people"], false).await?;
            let before = rows(&fixture.core, &[]).await?;
            assert_eq!(
                fixture.observations("people").confirmed(),
                Some(u64::MAX),
                "{policy:?}/{dispatch:?}"
            );
            fixture.stop_query().await?;
            fixture.send("people", "missed").await?;
            assert_ne!(fixture.observations("people").confirmed(), Some(1));
            fixture.restart().await?;

            let recovered = rows(&fixture.core, &["missed"]).await?;
            assert_eq!(recovered.output_generation, before.output_generation);
            assert_eq!(recovered.as_of_sequence, before.as_of_sequence + 1);
            let resumed = fixture.observations("people").latest();
            assert_eq!(resumed.settings.resume_from, None);
            assert_eq!(resumed.settings.resume_sequence, Some(0));
            assert!(resumed.settings.request_position_handle);
            assert!(!resumed.settings.enable_bootstrap);
            fixture.wait_confirmed("people", 1).await?;

            fixture.send("people", "live").await?;
            let live = rows(&fixture.core, &["missed", "live"]).await?;
            assert_eq!(live.as_of_sequence, recovered.as_of_sequence + 1);
            fixture.wait_confirmed("people", 2).await?;

            fixture.stop_query().await?;
            fixture.restart().await?;
            let again = rows(&fixture.core, &["missed", "live"]).await?;
            assert_eq!(again.output_generation, before.output_generation);
            assert_eq!(again.as_of_sequence, live.as_of_sequence);
            let resumed = fixture.observations("people").latest();
            assert_eq!(resumed.settings.resume_sequence, Some(2));
            assert_eq!(
                resumed.settings.resume_from.as_deref(),
                Some(2u64.to_be_bytes().as_slice())
            );
            fixture.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_multisource_recovery_keeps_committed_rows_when_only_one_checkpoint_is_missing(
) -> Result<()> {
    for policy in [RecoveryPolicy::Strict, RecoveryPolicy::AutoReset] {
        let fixture =
            Fixture::new(policy, DispatchMode::Channel, &["active", "quiet"], false).await?;
        fixture.send("active", "committed").await?;
        let before = rows(&fixture.core, &["committed"]).await?;
        fixture.wait_confirmed("active", 1).await?;
        assert_eq!(fixture.observations("quiet").confirmed(), Some(u64::MAX));
        fixture.stop_query().await?;
        fixture.send("active", "active-missed").await?;
        fixture.send("quiet", "quiet-missed").await?;
        fixture.restart().await?;

        let recovered = rows(
            &fixture.core,
            &["committed", "active-missed", "quiet-missed"],
        )
        .await?;
        assert_eq!(recovered.output_generation, before.output_generation);
        assert_eq!(recovered.as_of_sequence, before.as_of_sequence + 2);
        let active = fixture.observations("active").latest().settings;
        assert_eq!(active.resume_sequence, Some(1));
        assert_eq!(
            active.resume_from.as_deref(),
            Some(1u64.to_be_bytes().as_slice())
        );
        let quiet = fixture.observations("quiet").latest().settings;
        assert_eq!(quiet.resume_sequence, Some(0));
        assert!(quiet.resume_from.is_none());
        fixture.wait_confirmed("active", 2).await?;
        fixture.wait_confirmed("quiet", 1).await?;
        fixture.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn real_retention_gaps_remain_strict_or_rebootstrap_under_the_requested_policy() -> Result<()>
{
    for dispatch in [DispatchMode::Channel, DispatchMode::Broadcast] {
        for policy in [RecoveryPolicy::Strict, RecoveryPolicy::AutoReset] {
            let fixture = Fixture::new(policy, dispatch, &["people"], true).await?;
            fixture.send("people", "committed").await?;
            let before = rows(&fixture.core, &["committed"]).await?;
            fixture.wait_confirmed("people", 1).await?;
            fixture.stop_query().await?;
            fixture.send("people", "pruned").await?;
            fixture.send("people", "retained").await?;
            fixture.wal.prune_up_to("people", 2).await?;

            let restarted = timeout(DEADLINE, fixture.core.start_query(QUERY))
                .await
                .context("gap recovery timed out")?;
            if policy == RecoveryPolicy::Strict {
                assert!(restarted.is_err(), "{policy:?}/{dispatch:?}");
                wait_status(&fixture.core, ComponentStatus::Error).await?;
                assert_eq!(fixture.observations("people").subscriptions().len(), 2);
                assert_eq!(
                    fixture
                        .observations("people")
                        .bootstraps
                        .load(Ordering::Acquire),
                    1
                );
            } else {
                restarted?;
                wait_running(&fixture.core).await?;
                let recovered = rows(&fixture.core, &["committed", "pruned", "retained"]).await?;
                assert!(recovered.output_generation > before.output_generation);
                let resumed = fixture.observations("people").latest().settings;
                assert!(resumed.enable_bootstrap);
                assert!(resumed.resume_from.is_none());
                assert!(resumed.resume_sequence.is_none());
                assert_eq!(
                    fixture
                        .observations("people")
                        .bootstraps
                        .load(Ordering::Acquire),
                    2
                );
                fixture.send("people", "after-reset").await?;
                rows(
                    &fixture.core,
                    &["committed", "pruned", "retained", "after-reset"],
                )
                .await?;
                fixture.wait_confirmed("people", 4).await?;
            }
            let subscriptions = fixture.observations("people").subscriptions();
            let failed: Vec<_> = subscriptions
                .iter()
                .filter(|subscription| subscription.unavailable)
                .collect();
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].settings.resume_sequence, Some(1));
            assert_eq!(
                failed[0].settings.resume_from.as_deref(),
                Some(1u64.to_be_bytes().as_slice())
            );
            assert!(failed[0].settings.request_position_handle);
            fixture.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn broadcast_replay_still_requires_explicit_loss_permission() -> Result<()> {
    let (source, _input) = application("broadcast")?;
    let host = SourcePluginHost::owned(
        Box::new(ObservedApplication {
            inner: source,
            observations: Arc::new(Observations::default()),
            dispatch_mode: DispatchMode::Broadcast,
        }),
        LegacyPluginServices::empty("test"),
    );
    let stream = StreamId::try_new("broadcast/out")?;
    assert!(LegacySourceSubscription::new(
        host.clone(),
        SourceSubscriptionOptions::default(),
        stream.clone(),
        None,
    )
    .is_err());
    LegacySourceSubscription::new(
        host,
        SourceSubscriptionOptions {
            allow_broadcast_loss: true,
            ..Default::default()
        },
        stream,
        None,
    )?;
    Ok(())
}
