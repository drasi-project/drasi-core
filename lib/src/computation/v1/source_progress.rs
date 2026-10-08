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

use super::{ComponentId, StreamId};
use async_trait::async_trait;
use drasi_core::interface::SourceCheckpoint;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceProgressKey {
    Source(String),
    Stream(StreamId),
}

/// A read-only view of an owning query or durable middleware's committed input
/// progress. Persistence comes from that component's actual checkpoint provider,
/// not from this watch channel. A non-immediate owner requires an asserted,
/// validated path through identity-preserving stateless components.
#[derive(Debug, Clone, Default)]
pub struct SourceProgressSnapshot {
    pub ready: bool,
    /// All source resume/reset preparation completed for this generation. Live
    /// inputs may now buffer, but evaluation still waits for bootstrap readiness.
    pub admitting: bool,
    pub recovered: bool,
    pub bootstrap_complete: bool,
    pub persistent: bool,
    pub reset_generation: u64,
    pub checkpoints: BTreeMap<SourceProgressKey, SourceCheckpoint>,
    /// Consumed transport positions are not source resume cursors. Replayed
    /// logical input can advance these without advancing `checkpoints`.
    pub transport_sequences: BTreeMap<StreamId, u64>,
    pub failure: Option<Arc<str>>,
}

/// A subscription to committed progress. Reads mark the returned state observed;
/// changes may coalesce, but cancelling `changed` must not consume an update.
#[async_trait]
pub trait SourceProgressUpdates: Send + Sync {
    fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>>;
    async fn changed(&mut self) -> anyhow::Result<()>;
}

#[async_trait]
impl SourceProgressUpdates for watch::Receiver<Arc<SourceProgressSnapshot>> {
    fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        Ok(self.borrow_and_update().clone())
    }

    async fn changed(&mut self) -> anyhow::Result<()> {
        Ok(watch::Receiver::changed(self).await?)
    }
}

/// Read-only progress supplied by another boundary, such as a plugin host.
/// Implementations must surface revocation and read errors, not return a default
/// snapshot. Synchronous reads must be bounded and perform no storage or network
/// I/O. Subscriptions own their observation state without forwarding tasks.
pub trait SourceProgressProvider: Send + Sync + 'static {
    fn graph_id(&self) -> &str;
    fn component_id(&self) -> &ComponentId;
    fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>>;
    fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>>;
}

/// Separates readable progress from proof of the actual local transaction owner.
/// An external provider never supplies that proof, even when its names match.
/// FFI adapters must retain the real owner on the host; Rust objects do not cross
/// the plugin boundary.
#[derive(Clone)]
pub enum SourceProgressReader {
    Local(Arc<QuerySourceProgress>),
    External(Arc<dyn SourceProgressProvider>),
}

impl From<Arc<QuerySourceProgress>> for SourceProgressReader {
    fn from(owner: Arc<QuerySourceProgress>) -> Self {
        Self::Local(owner)
    }
}

impl SourceProgressReader {
    pub fn graph_id(&self) -> &str {
        match self {
            Self::Local(owner) => owner.graph_id(),
            Self::External(provider) => provider.graph_id(),
        }
    }

    pub fn component_id(&self) -> &ComponentId {
        match self {
            Self::Local(owner) => owner.component_id(),
            Self::External(provider) => provider.component_id(),
        }
    }

    pub fn local_owner(&self) -> Option<Arc<QuerySourceProgress>> {
        match self {
            Self::Local(owner) => Some(owner.clone()),
            Self::External(_) => None,
        }
    }

    /// Pair a source and initialization provider using the same actual handle,
    /// not independently constructed providers with matching identifiers.
    pub fn same_owner(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(left), Self::Local(right)) => Arc::ptr_eq(left, right),
            (Self::External(left), Self::External(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    pub fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        match self {
            Self::Local(owner) => Ok(owner.snapshot()),
            Self::External(provider) => provider.snapshot(),
        }
    }

    pub fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
        match self {
            Self::Local(owner) => Ok(Box::new(owner.subscribe())),
            Self::External(provider) => provider.subscribe(),
        }
    }

    pub async fn wait_recovered(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.wait_for(|snapshot| snapshot.recovered).await
    }

    pub async fn wait_ready(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.wait_for(|snapshot| snapshot.ready).await
    }

    async fn wait_for(
        &self,
        predicate: impl Fn(&SourceProgressSnapshot) -> bool,
    ) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let mut updates = self.subscribe()?;
        loop {
            let snapshot = updates.snapshot()?;
            if let Some(failure) = &snapshot.failure {
                anyhow::bail!("{failure}");
            }
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            updates.changed().await?;
        }
    }
}

pub struct QuerySourceProgress {
    graph_id: Arc<str>,
    query_id: ComponentId,
    state: watch::Sender<Arc<SourceProgressSnapshot>>,
    replay_only: AtomicBool,
}

impl std::fmt::Debug for QuerySourceProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuerySourceProgress")
            .field("graph_id", &self.graph_id)
            .field("component_id", &self.query_id)
            .finish_non_exhaustive()
    }
}

impl QuerySourceProgress {
    pub fn new(graph_id: impl Into<Arc<str>>, query_id: ComponentId) -> super::Result<Self> {
        let graph_id = graph_id.into();
        super::data::validate_identifier("graph", &graph_id)?;
        Ok(Self {
            graph_id,
            query_id,
            state: watch::channel(Arc::new(SourceProgressSnapshot::default())).0,
            replay_only: AtomicBool::new(false),
        })
    }
    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }
    pub fn query_id(&self) -> &ComponentId {
        &self.query_id
    }
    /// Generalized owner name; `query_id` remains for source adapter compatibility.
    pub fn component_id(&self) -> &ComponentId {
        &self.query_id
    }
    /// Middleware owns streaming recovery, not a query's bootstrap/reset protocol.
    pub fn replay_only(&self) -> bool {
        self.replay_only.load(Ordering::Acquire)
    }
    pub(super) fn require_stream_replay(&self) {
        self.replay_only.store(true, Ordering::Release);
    }
    pub fn snapshot(&self) -> Arc<SourceProgressSnapshot> {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<SourceProgressSnapshot>> {
        self.state.subscribe()
    }
    pub async fn wait_recovered(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let mut receiver = self.state.subscribe();
        let snapshot = receiver
            .wait_for(|snapshot| snapshot.recovered || snapshot.failure.is_some())
            .await?;
        if let Some(failure) = &snapshot.failure {
            anyhow::bail!("{failure}");
        }
        Ok(snapshot.clone())
    }
    pub async fn wait_ready(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let mut receiver = self.state.subscribe();
        let snapshot = receiver
            .wait_for(|snapshot| snapshot.ready || snapshot.failure.is_some())
            .await?;
        if let Some(failure) = &snapshot.failure {
            anyhow::bail!("{failure}");
        }
        Ok(snapshot.clone())
    }
    pub(super) fn pending(&self) {
        self.state.send_modify(|state| {
            let state = Arc::make_mut(state);
            state.ready = false;
            state.admitting = false;
            state.recovered = false;
            state.failure = None;
        });
    }
    pub(super) fn fail(&self) {
        self.state.send_modify(|state| {
            let state = Arc::make_mut(state);
            state.ready = false;
            state.admitting = false;
            state.failure = Some(Arc::from(
                "owning component progress is fenced after interrupted or failed processing",
            ));
        });
    }
    pub(super) fn publish(&self, snapshot: SourceProgressSnapshot) {
        self.state.send_replace(Arc::new(snapshot));
    }
    pub(super) fn admit(&self) {
        self.state.send_modify(|state| {
            Arc::make_mut(state).admitting = true;
        });
    }
    pub(super) fn confirm(&self, key: SourceProgressKey, checkpoint: SourceCheckpoint) {
        self.state.send_modify(|state| {
            Arc::make_mut(state).checkpoints.insert(key, checkpoint);
        });
    }
    pub(super) fn confirm_input(&self, input: &super::producer_progress::GraphInputProgress) {
        self.state.send_modify(|state| {
            let state = Arc::make_mut(state);
            input.record_transport(&mut state.transport_sequences);
            state.checkpoints.insert(
                input.identity.clone(),
                SourceCheckpoint::new(input.sequence, input.position.clone()),
            );
        });
    }
}

/// Register this actual resource for its owning query/durable middleware and sources.
/// It never registers a legacy Source position handle or controls another pipeline.
pub struct QuerySourceProgressResource(pub Arc<QuerySourceProgress>);

#[cfg(test)]
mod tests {
    use super::*;

    struct External(Arc<QuerySourceProgress>);

    impl SourceProgressProvider for External {
        fn graph_id(&self) -> &str {
            self.0.graph_id()
        }
        fn component_id(&self) -> &ComponentId {
            self.0.component_id()
        }
        fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
            Ok(self.0.snapshot())
        }
        fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
            Ok(Box::new(self.0.subscribe()))
        }
    }

    fn owner() -> Arc<QuerySourceProgress> {
        Arc::new(QuerySourceProgress::new("graph", ComponentId::try_new("query").unwrap()).unwrap())
    }

    #[test]
    fn progress_reader_never_promotes_names_or_external_reads_to_local_ownership() {
        let owner = owner();
        let local = SourceProgressReader::from(owner.clone());
        let external = SourceProgressReader::External(Arc::new(External(owner.clone())));
        assert_eq!(local.graph_id(), external.graph_id());
        assert_eq!(local.component_id(), external.component_id());
        assert!(Arc::ptr_eq(&owner, &local.local_owner().unwrap()));
        assert!(local.same_owner(&local.clone()));
        assert!(external.same_owner(&external.clone()));
        assert!(external.local_owner().is_none());
        assert!(!local.same_owner(&external));
        assert!(!external.same_owner(&local));
        assert!(
            !external.same_owner(&SourceProgressReader::External(Arc::new(External(
                owner.clone()
            ))))
        );
        assert!(!local.same_owner(&SourceProgressReader::from(Arc::new(
            QuerySourceProgress::new(owner.graph_id(), owner.component_id().clone()).unwrap()
        ))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_reader_preserves_complete_state_and_independent_subscriptions() {
        let owner = owner();
        for reader in [
            SourceProgressReader::from(owner.clone()),
            SourceProgressReader::External(Arc::new(External(owner.clone()))),
        ] {
            owner.pending();
            let mut left = reader.subscribe().unwrap();
            let mut right = reader.subscribe().unwrap();
            left.snapshot().unwrap();
            right.snapshot().unwrap();
            owner.publish(SourceProgressSnapshot {
                ready: true,
                admitting: true,
                recovered: true,
                bootstrap_complete: true,
                persistent: true,
                reset_generation: 7,
                checkpoints: BTreeMap::from([
                    (
                        SourceProgressKey::Source("source".into()),
                        SourceCheckpoint::new(0, Some(bytes::Bytes::new())),
                    ),
                    (
                        SourceProgressKey::Stream(StreamId::try_new("source").unwrap()),
                        SourceCheckpoint::new(42, None),
                    ),
                ]),
                transport_sequences: BTreeMap::from([(
                    StreamId::try_new("source/out").unwrap(),
                    91,
                )]),
                failure: None,
            });
            left.changed().await.unwrap();
            let snapshot = left.snapshot().unwrap();
            assert!(Arc::ptr_eq(&snapshot, &reader.snapshot().unwrap()));
            assert!(Arc::ptr_eq(&snapshot, &reader.wait_ready().await.unwrap()));
            assert!(Arc::ptr_eq(
                &snapshot,
                &reader.wait_recovered().await.unwrap()
            ));
            assert!(snapshot.admitting && snapshot.bootstrap_complete && snapshot.persistent);
            assert_eq!(snapshot.reset_generation, 7);
            assert_eq!(snapshot.checkpoints.len(), 2);
            assert_eq!(
                snapshot.checkpoints[&SourceProgressKey::Source("source".into())],
                SourceCheckpoint::new(0, Some(bytes::Bytes::new()))
            );
            assert_eq!(
                snapshot.transport_sequences[&StreamId::try_new("source/out").unwrap()],
                91
            );
            right.changed().await.unwrap();
            assert!(Arc::ptr_eq(&snapshot, &right.snapshot().unwrap()));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_reader_cancelled_waits_do_not_consume_updates_or_hide_fencing() {
        let owner = owner();
        for reader in [
            SourceProgressReader::from(owner.clone()),
            SourceProgressReader::External(Arc::new(External(owner.clone()))),
        ] {
            owner.pending();
            let mut ready = Box::pin(reader.wait_ready());
            assert!(futures::poll!(&mut ready).is_pending());
            drop(ready);
            let mut updates = reader.subscribe().unwrap();
            updates.snapshot().unwrap();
            let mut changed = Box::pin(updates.changed());
            assert!(futures::poll!(&mut changed).is_pending());
            drop(changed);
            owner.publish(SourceProgressSnapshot {
                recovered: true,
                ..Default::default()
            });
            updates.changed().await.unwrap();
            assert!(updates.snapshot().unwrap().recovered);
            assert!(reader.wait_recovered().await.unwrap().recovered);
            let mut ready = Box::pin(reader.wait_ready());
            assert!(futures::poll!(&mut ready).is_pending());
            owner.fail();
            assert!(ready.await.unwrap_err().to_string().contains("fenced"));
            assert!(reader.wait_recovered().await.is_err());
        }
    }

    struct Unavailable {
        owner: Arc<QuerySourceProgress>,
        subscribe: bool,
        changed: bool,
    }

    impl SourceProgressProvider for Unavailable {
        fn graph_id(&self) -> &str {
            self.owner.graph_id()
        }
        fn component_id(&self) -> &ComponentId {
            self.owner.component_id()
        }
        fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
            Err(std::io::Error::other("revoked progress").into())
        }
        fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
            if !self.subscribe {
                return Err(std::io::Error::other("revoked subscription").into());
            }
            Ok(Box::new(UnavailableUpdates(self.changed)))
        }
    }

    struct UnavailableUpdates(bool);

    #[async_trait]
    impl SourceProgressUpdates for UnavailableUpdates {
        fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
            if self.0 {
                Ok(Arc::new(SourceProgressSnapshot::default()))
            } else {
                Err(std::io::Error::other("revoked snapshot").into())
            }
        }
        async fn changed(&mut self) -> anyhow::Result<()> {
            Err(std::io::Error::other("revoked wait").into())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_reader_propagates_read_subscription_and_wait_errors() {
        for (subscribe, changed, message) in [
            (false, false, "revoked subscription"),
            (true, false, "revoked snapshot"),
            (true, true, "revoked wait"),
        ] {
            let reader = SourceProgressReader::External(Arc::new(Unavailable {
                owner: owner(),
                subscribe,
                changed,
            }));
            let error = reader.snapshot().unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().to_string(),
                "revoked progress"
            );
            for error in [
                reader.wait_ready().await.unwrap_err(),
                reader.wait_recovered().await.unwrap_err(),
            ] {
                assert_eq!(
                    error.downcast_ref::<std::io::Error>().unwrap().to_string(),
                    message
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_subscription_reports_owner_closure_after_last_observation() {
        let (sender, receiver) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        let mut updates: Box<dyn SourceProgressUpdates> = Box::new(receiver);
        sender.send_replace(Arc::new(SourceProgressSnapshot {
            recovered: true,
            ..Default::default()
        }));
        drop(sender);
        updates.changed().await.unwrap();
        assert!(updates.snapshot().unwrap().recovered);
        let error = updates.changed().await.unwrap_err();
        assert!(error.downcast_ref::<watch::error::RecvError>().is_some());
    }

    #[test]
    fn ranked_admission_is_fenced_until_all_resume_preparation_completes() {
        let progress =
            QuerySourceProgress::new("graph", ComponentId::try_new("query").unwrap()).unwrap();
        progress.publish(SourceProgressSnapshot {
            recovered: true,
            reset_generation: 4,
            ..Default::default()
        });
        assert!(!progress.snapshot().admitting);
        progress.admit();
        assert!(progress.snapshot().admitting);
        assert!(
            !progress.snapshot().ready,
            "admission must not bypass bootstrap"
        );
        progress.pending();
        assert!(!progress.snapshot().admitting);
        progress.publish(SourceProgressSnapshot {
            recovered: true,
            reset_generation: 5,
            ..Default::default()
        });
        assert!(!progress.snapshot().admitting);
        progress.admit();
        progress.fail();
        assert!(!progress.snapshot().admitting);
        assert!(progress.snapshot().failure.is_some());
    }
}
