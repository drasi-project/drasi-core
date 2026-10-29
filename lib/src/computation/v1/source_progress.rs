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
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::watch;

const MAX_INPUT_POSITION_BINDINGS: usize = 4096;

pub use drasi_computation_contracts::{
    SourceProgressKey, SourceProgressProvider, SourceProgressSnapshot, SourceProgressUpdates,
    SourceProgressView,
};

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
    /// A data-only view of this exact provider, without local ownership proof.
    pub fn read_only(&self) -> SourceProgressView {
        match self {
            Self::Local(owner) => SourceProgressView::new(owner.clone()),
            Self::External(provider) => SourceProgressView::new(provider.clone()),
        }
    }

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

struct InputPositionBinding {
    role: super::ComponentRole,
    policy: Option<Arc<super::InputPositionPolicy>>,
    paths: BTreeSet<Vec<StreamId>>,
}

pub(super) struct InputBoundary {
    pub role: super::ComponentRole,
    pub policy: Option<Arc<super::InputPositionPolicy>>,
    pub system: Arc<super::SystemMetadata>,
    pub distinct_ancestor: bool,
}

struct VolatileWriter {
    identity: std::sync::Weak<()>,
    active: bool,
}

fn position_origin(
    input: &super::ChangeEnvelope,
    path: &[StreamId],
) -> Option<(Arc<super::SystemMetadata>, bool)> {
    let mut expected = path.iter().rev();
    let mut next = expected.next()?;
    let mut current = input.system();
    let mut ancestor = input.lineage();
    let mut previous = None;
    loop {
        if previous != Some(current.stream()) {
            if current.stream() != next {
                return None;
            }
            let Some(parent) = expected.next() else {
                let mut earlier = ancestor;
                let mut distinct = false;
                while let Some(parent) = earlier {
                    if parent.system().stream() != current.stream() {
                        distinct = true;
                        break;
                    }
                    earlier = parent.parent();
                }
                return Some((current.clone(), distinct));
            };
            next = parent;
            previous = Some(current.stream());
        }
        let parent = ancestor?;
        current = parent.system();
        ancestor = parent.parent();
    }
}

pub struct QuerySourceProgress {
    graph_id: Arc<str>,
    query_id: ComponentId,
    state: watch::Sender<Arc<SourceProgressSnapshot>>,
    replay_only: AtomicBool,
    input_positions: std::sync::RwLock<BTreeMap<ComponentId, InputPositionBinding>>,
    has_input_positions: AtomicBool,
    volatile_writer: std::sync::Mutex<Option<VolatileWriter>>,
}

impl SourceProgressProvider for QuerySourceProgress {
    fn graph_id(&self) -> &str {
        QuerySourceProgress::graph_id(self)
    }

    fn component_id(&self) -> &ComponentId {
        QuerySourceProgress::component_id(self)
    }

    fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        Ok(QuerySourceProgress::snapshot(self))
    }

    fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
        Ok(Box::new(QuerySourceProgress::subscribe(self)))
    }
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
            input_positions: std::sync::RwLock::new(BTreeMap::new()),
            has_input_positions: AtomicBool::new(false),
            volatile_writer: std::sync::Mutex::new(None),
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

    pub(super) fn claim_volatile_writer(&self, identity: &Arc<()>) -> anyhow::Result<()> {
        let mut writer = self
            .volatile_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("volatile input progress ownership is poisoned"))?;
        anyhow::ensure!(
            !writer.as_ref().is_some_and(|writer| writer.active),
            "volatile input progress requires successful cleanup of its previous owner"
        );
        let snapshot = self.snapshot();
        anyhow::ensure!(
            !snapshot.ready && !snapshot.persistent,
            "volatile input progress cannot replace an active or persistent publisher"
        );
        let same = writer
            .as_ref()
            .is_some_and(|writer| writer.identity.ptr_eq(&Arc::downgrade(identity)));
        if !same {
            self.publish(SourceProgressSnapshot {
                reset_generation: snapshot
                    .reset_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("volatile input generation exhausted"))?,
                ..Default::default()
            });
        } else {
            self.pending();
        }
        *writer = Some(VolatileWriter {
            identity: Arc::downgrade(identity),
            active: true,
        });
        Ok(())
    }

    pub(super) fn release_volatile_writer(&self, identity: &Arc<()>) -> anyhow::Result<()> {
        let mut writer = self
            .volatile_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("volatile input progress ownership is poisoned"))?;
        let writer = writer
            .as_mut()
            .filter(|writer| writer.active && writer.identity.ptr_eq(&Arc::downgrade(identity)))
            .ok_or_else(|| anyhow::anyhow!("volatile input progress owner changed"))?;
        self.pending();
        writer.active = false;
        Ok(())
    }

    pub(super) fn bind_input_boundary(
        &self,
        producer: &ComponentId,
        role: super::ComponentRole,
        policy: Option<Arc<super::InputPositionPolicy>>,
        path: Vec<StreamId>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !path.is_empty(),
            "input replay position has no validated data path"
        );
        if let Some(SourceProgressKey::Stream(stream)) = policy.as_ref().map(|policy| policy.key())
        {
            anyhow::ensure!(
                path.first() == Some(stream),
                "input replay position stream does not belong to its producer"
            );
        }
        let mut positions = self
            .input_positions
            .write()
            .map_err(|_| anyhow::anyhow!("input position bindings are poisoned"))?;
        for (bound_producer, bound) in positions.iter() {
            let previous = bound.policy.as_ref().map(|policy| policy.key());
            let next = policy.as_ref().map(|policy| policy.key());
            if bound_producer == producer {
                anyhow::ensure!(previous == next && bound.role == role,
                    "producer changed its input replay identity without reconstructing its consumer");
            } else {
                anyhow::ensure!(
                    next.is_none() || previous != next,
                    "different producers claim the same input replay position"
                );
            }
        }
        if !positions
            .get(producer)
            .is_some_and(|bound| bound.paths.contains(&path))
        {
            anyhow::ensure!(
                positions
                    .values()
                    .map(|binding| binding.paths.len())
                    .sum::<usize>()
                    < MAX_INPUT_POSITION_BINDINGS,
                "input replay position binding limit reached"
            );
        }
        // Queued inputs can outlive a producer/path replacement. Retain the
        // validated old paths, but never promote arbitrary inherited lineage.
        let binding = positions
            .entry(producer.clone())
            .or_insert_with(|| InputPositionBinding {
                role,
                policy: policy.clone(),
                paths: BTreeSet::new(),
            });
        binding.policy = policy;
        binding.paths.insert(path);
        self.has_input_positions.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) fn input_binding(
        &self,
        input: &super::ChangeEnvelope,
        fallback: &SourceProgressKey,
    ) -> anyhow::Result<Option<InputBoundary>> {
        if !self.has_input_positions.load(Ordering::Acquire) {
            return Ok(None);
        }
        let bindings = self
            .input_positions
            .read()
            .map_err(|_| anyhow::anyhow!("input position bindings are poisoned"))?;
        let mut selected = None;
        for binding in bindings.values() {
            for path in &binding.paths {
                if let Some((system, distinct_ancestor)) = position_origin(input, path) {
                    anyhow::ensure!(
                        selected.is_none(),
                        "input matches more than one producer replay position"
                    );
                    selected = Some(InputBoundary {
                        role: binding.role,
                        policy: binding.policy.clone(),
                        system,
                        distinct_ancestor,
                    });
                }
            }
        }
        anyhow::ensure!(
            selected.is_some()
                || input.lineage().is_some()
                || !bindings
                    .values()
                    .any(|binding| binding.paths.iter().any(|path| {
                        path.len() > 1 && path.last() == Some(input.system().stream())
                    })),
            "identity-preserving replay path omitted producer provenance"
        );
        anyhow::ensure!(
            selected.is_some()
                || !bindings.values().any(|binding| {
                    binding
                        .policy
                        .as_ref()
                        .is_some_and(|policy| policy.key() == fallback)
                }),
            "input replay provenance is outside its validated producer path"
        );
        Ok(selected)
    }

    pub(super) fn position_handled(
        &self,
        input: &super::producer_progress::GraphInputProgress,
        checkpoint: Option<&SourceCheckpoint>,
    ) -> anyhow::Result<Option<bool>> {
        let Some(policy) = input
            .positioned
            .as_ref()
            .and_then(|position| position.policy.as_ref())
        else {
            return Ok(None);
        };
        let position = input.position.as_deref().ok_or_else(|| {
            anyhow::anyhow!("producer omitted its required input replay position")
        })?;
        anyhow::ensure!(
            position.len() <= drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES,
            "input replay position exceeds the source cursor limit"
        );
        let published;
        let checkpoint = match checkpoint {
            Some(checkpoint) => Some(checkpoint),
            None => {
                published = self.snapshot();
                published.checkpoints.get(&input.identity)
            }
        };
        let Some(checkpoint) = checkpoint else {
            anyhow::ensure!(
                !policy.is_after(position, position)?,
                "input position ordering must be strictly after, not equal"
            );
            return Ok(Some(false));
        };
        let handled = checkpoint.source_position.as_deref().ok_or_else(|| {
            anyhow::anyhow!("handled input has no position for its declared replay policy")
        })?;
        let after = policy.is_after(position, handled)?;
        anyhow::ensure!(
            !after || input.sequence > checkpoint.sequence,
            "new source position regressed its logical sequence"
        );
        Ok(Some(!after))
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
        assert!(local.read_only().same_owner(&local.clone().read_only()));
        assert!(external
            .read_only()
            .same_owner(&external.clone().read_only()));
        assert!(!local.read_only().same_owner(&external.read_only()));
        assert!(!local.read_only().same_owner(
            &SourceProgressReader::from(Arc::new(
                QuerySourceProgress::new(owner.graph_id(), owner.component_id().clone()).unwrap()
            ))
            .read_only()
        ));
        let projected = SourceProgressReader::External(Arc::new(local.read_only()));
        assert!(projected.local_owner().is_none());
        assert!(!projected.same_owner(&local));
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
            let view = reader.read_only();
            let mut right = view.subscribe().unwrap();
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
            assert!(Arc::ptr_eq(&snapshot, &view.snapshot().unwrap()));
            assert!(Arc::ptr_eq(&snapshot, &view.wait_ready().await.unwrap()));
            assert!(Arc::ptr_eq(
                &snapshot,
                &view.wait_recovered().await.unwrap()
            ));
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
            let view = reader.read_only();
            assert_eq!(
                view.snapshot()
                    .unwrap_err()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .to_string(),
                "revoked progress"
            );
            for error in [
                reader.wait_ready().await.unwrap_err(),
                reader.wait_recovered().await.unwrap_err(),
                view.wait_ready().await.unwrap_err(),
                view.wait_recovered().await.unwrap_err(),
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
