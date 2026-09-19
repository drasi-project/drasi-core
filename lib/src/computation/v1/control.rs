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

//! Volatile, low-volume peer notifications on dedicated bounded control queues.
//!
//! Sends never await queue capacity, data operations, or handlers. They briefly
//! serialize against other control/topology operations and reserve capacity for
//! every recipient before enqueueing anything. An error accepts no messages and
//! changes no readiness state. Success means enqueue acceptance, not handling,
//! durability, or survival of receiver cancellation.
//!
//! Each queue bounds message count; notification contents and JSON nesting have
//! separate limits. Caller-owned allocations and in-flight handlers are outside
//! those bounds. Routing scans current connections; fanout copies one bounded
//! payload per neighbor. Hosts and handlers must remain cooperative and use this
//! channel for low-volume control, not bulk data. The host owns polling,
//! cancellation, and error policy; this module starts no tasks. Closing a plane
//! permanently revokes all attachments; reopening requires a new plane.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::{ComponentGeneration, ComponentId};

/// Maximum UTF-8 reason length, or combined UTF-8 kind and encoded JSON length.
///
/// This excludes fixed notification metadata and component identifiers.
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 16 * 1024;

/// Maximum JSON child depth, counting the root value as depth zero.
pub const MAX_CONTROL_JSON_DEPTH: usize = 64;

/// Direction of travel relative to the sender and the graph's data edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDirection {
    Upstream,
    Downstream,
}

/// Readiness describes the sender, not the recipient or the data queue.
///
/// A successful `Ready` or `NotReady` send also updates the sender's persistent
/// readiness state, even when there are no recipients. Availability and custom
/// notifications do not implicitly change readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlNotification {
    Ready,
    NotReady,
    Available,
    Unavailable { reason: String },
    Custom { kind: String, payload: Value },
}

/// An accepted notification with unforgeable attachment and connection tokens.
///
/// The host must subscribe to control changes before validating delivery and
/// dispatching. While a callback is pending, revalidate its message on each watch
/// change and drop the callback on validation failure, including plane closure.
/// Prioritize invalidation over polling the callback when both are ready.
/// Cancellation is cooperative, not rollback of effects already performed.
#[derive(Debug, Clone)]
pub struct PeerMessage {
    pub from: ComponentId,
    pub generation: ComponentGeneration,
    pub direction: ControlDirection,
    pub notification: ControlNotification,
    recipient: ComponentId,
    recipient_generation: ComponentGeneration,
    sender_attachment: Arc<()>,
    recipient_attachment: Arc<()>,
    connection: Arc<()>,
}

/// Explicit rejection of a control operation. Enqueue errors accept no fanout.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("control queue capacity must be in 1..=Semaphore::MAX_PERMITS, got {capacity}")]
    InvalidCapacity { capacity: usize },
    #[error("control plane is closed")]
    PlaneClosed,
    #[error("control queue for {recipient} is closed or not attached")]
    Closed { recipient: ComponentId },
    #[error("control queue for {recipient} is full; no recipients accepted the notification")]
    QueueFull { recipient: ComponentId },
    #[error("{to} is not an immediate control neighbor of {from}")]
    NotNeighbor { from: ComponentId, to: ComponentId },
    #[error("stale control attachment for {id} at generation {generation:?}")]
    StaleComponent {
        id: ComponentId,
        generation: ComponentGeneration,
    },
    #[error("stale control connection from {from} to {to}")]
    StaleConnection { from: ComponentId, to: ComponentId },
    #[error("a control connection cannot connect {id} to itself")]
    SelfConnection { id: ComponentId },
    #[error("control notification exceeds the {limit}-byte payload limit")]
    PayloadTooLarge { limit: usize },
    #[error("control notification exceeds the maximum JSON child depth of {limit}")]
    PayloadTooDeep { limit: usize },
    #[error("control plane state is poisoned")]
    Poisoned,
}

/// A cloneable, restricted sender belonging to one component attachment.
///
/// Clones cannot impersonate another component, address arbitrary nodes, or keep
/// the graph alive. Reattaching the same ID invalidates all previous clones,
/// including when its generation is unchanged. A surviving attachment follows
/// the current connections; it cannot send through a removed connection.
#[derive(Clone)]
pub struct ComponentControl {
    id: ComponentId,
    generation: ComponentGeneration,
    attachment: Arc<()>,
    plane: Weak<ControlPlane>,
}

impl fmt::Debug for ComponentControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentControl")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl ComponentControl {
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    pub fn generation(&self) -> ComponentGeneration {
        self.generation
    }

    /// Atomically accept a notification into every immediate upstream queue.
    pub fn notify_upstream(&self, notification: ControlNotification) -> Result<(), ControlError> {
        self.notify(Target::Direction(ControlDirection::Upstream), notification)
    }

    /// Atomically accept a notification into every immediate downstream queue.
    pub fn notify_downstream(&self, notification: ControlNotification) -> Result<(), ControlError> {
        self.notify(
            Target::Direction(ControlDirection::Downstream),
            notification,
        )
    }

    /// Notify one currently connected peer, rejecting unrelated or indirect nodes.
    pub fn notify_neighbor(
        &self,
        neighbor: &ComponentId,
        notification: ControlNotification,
    ) -> Result<(), ControlError> {
        self.notify(Target::Neighbor(neighbor), notification)
    }

    /// Publish readiness and notify upstream peers. Failure leaves state unchanged.
    pub fn ready(&self) -> Result<(), ControlError> {
        self.notify_upstream(ControlNotification::Ready)
    }

    /// Withdraw readiness and notify upstream peers. Failure leaves state unchanged.
    pub fn not_ready(&self) -> Result<(), ControlError> {
        self.notify_upstream(ControlNotification::NotReady)
    }

    fn notify(
        &self,
        target: Target<'_>,
        notification: ControlNotification,
    ) -> Result<(), ControlError> {
        let plane = self.plane.upgrade().ok_or(ControlError::PlaneClosed)?;
        notification.validate_payload()?;
        let mut state = plane.lock_open()?;
        state.validate_attachment(&self.id, self.generation, &self.attachment)?;

        let recipients = state.recipients(&self.id, target)?;
        let has_recipients = !recipients.is_empty();
        let mut pending = Vec::with_capacity(recipients.len());
        for (recipient, direction, connection) in recipients {
            let endpoint =
                state
                    .components
                    .get(&recipient)
                    .ok_or_else(|| ControlError::Closed {
                        recipient: recipient.clone(),
                    })?;
            let permit = endpoint.sender.try_reserve().map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControlError::QueueFull {
                    recipient: recipient.clone(),
                },
                mpsc::error::TrySendError::Closed(_) => ControlError::Closed {
                    recipient: recipient.clone(),
                },
            })?;
            pending.push((
                permit,
                PeerMessage {
                    from: self.id.clone(),
                    generation: self.generation,
                    direction,
                    notification: notification.clone(),
                    recipient,
                    recipient_generation: endpoint.generation,
                    sender_attachment: self.attachment.clone(),
                    recipient_attachment: endpoint.attachment.clone(),
                    connection,
                },
            ));
        }

        // No fallible operation follows the reservations. Holding the state lock
        // makes enqueue/readiness commit linearizable with attachment and rebind.
        for (permit, message) in pending {
            permit.send(message);
        }
        let readiness = match notification {
            ControlNotification::Ready => Some(true),
            ControlNotification::NotReady => Some(false),
            _ => None,
        };
        let mut readiness_changed = false;
        if let Some(ready) = readiness {
            if let Some(endpoint) = state.components.get_mut(&self.id) {
                readiness_changed = endpoint.ready != ready;
                endpoint.ready = ready;
            }
        }
        drop(state);
        if has_recipients || readiness_changed {
            plane.changed();
        }
        Ok(())
    }
}

/// A separately polled control callback, independent of mutable data operations.
///
/// Implementations must be cooperative and bound their work. Returning success
/// does not acknowledge data, and failures require the host's explicit policy.
#[async_trait]
pub trait ControlHandler: Send + Sync {
    async fn on_message(
        &self,
        message: PeerMessage,
        control: ComponentControl,
    ) -> anyhow::Result<()>;
}

struct Endpoint {
    generation: ComponentGeneration,
    attachment: Arc<()>,
    sender: mpsc::Sender<PeerMessage>,
    ready: bool,
}

#[derive(Default)]
struct State {
    closed: bool,
    components: BTreeMap<ComponentId, Endpoint>,
    connections: BTreeMap<(ComponentId, ComponentId), Arc<()>>,
}

enum Target<'a> {
    Direction(ControlDirection),
    Neighbor(&'a ComponentId),
}

impl State {
    fn validate_attachment(
        &self,
        id: &ComponentId,
        generation: ComponentGeneration,
        attachment: &Arc<()>,
    ) -> Result<(), ControlError> {
        match self.components.get(id) {
            Some(endpoint)
                if endpoint.generation == generation
                    && Arc::ptr_eq(&endpoint.attachment, attachment) =>
            {
                Ok(())
            }
            _ => Err(ControlError::StaleComponent {
                id: id.clone(),
                generation,
            }),
        }
    }

    fn recipients(
        &self,
        from: &ComponentId,
        target: Target<'_>,
    ) -> Result<Vec<(ComponentId, ControlDirection, Arc<()>)>, ControlError> {
        let direction = match target {
            Target::Neighbor(to) => {
                for (pair, direction) in [
                    ((from.clone(), to.clone()), ControlDirection::Downstream),
                    ((to.clone(), from.clone()), ControlDirection::Upstream),
                ] {
                    if let Some(connection) = self.connections.get(&pair) {
                        return Ok(vec![(to.clone(), direction, connection.clone())]);
                    }
                }
                return Err(ControlError::NotNeighbor {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            Target::Direction(direction) => direction,
        };
        let mut recipients = Vec::new();
        for ((upstream, downstream), connection) in &self.connections {
            let peer = match direction {
                ControlDirection::Downstream if upstream == from => Some(downstream),
                ControlDirection::Upstream if downstream == from => Some(upstream),
                _ => None,
            };
            if let Some(peer) = peer {
                recipients.push((peer.clone(), direction, connection.clone()));
            }
        }
        Ok(recipients)
    }
}

/// Host-only ownership and topology for dedicated peer-control mailboxes.
pub(crate) struct ControlPlane {
    capacity: usize,
    state: Mutex<State>,
    revision: watch::Sender<u64>,
    weak_self: Weak<Self>,
}

impl ControlPlane {
    pub(crate) fn new(capacity: usize) -> Result<Arc<Self>, ControlError> {
        if capacity == 0 || capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ControlError::InvalidCapacity { capacity });
        }
        let (revision, _) = watch::channel(0);
        Ok(Arc::new_cyclic(|weak_self| Self {
            capacity,
            state: Mutex::new(State::default()),
            revision,
            weak_self: weak_self.clone(),
        }))
    }

    /// Install a fresh, initially not-ready attachment and invalidate its prior
    /// handles and mailbox, even for a same-generation rebind. An older generation
    /// cannot replace a currently newer attachment.
    pub(crate) fn attach(
        self: &Arc<Self>,
        id: ComponentId,
        generation: ComponentGeneration,
    ) -> Result<(ComponentControl, mpsc::Receiver<PeerMessage>), ControlError> {
        let mut state = self.lock_open()?;
        if state
            .components
            .get(&id)
            .is_some_and(|endpoint| endpoint.generation > generation)
        {
            return Err(ControlError::StaleComponent { id, generation });
        }
        let attachment = Arc::new(());
        let (sender, receiver) = mpsc::channel(self.capacity);
        state.components.insert(
            id.clone(),
            Endpoint {
                generation,
                attachment: attachment.clone(),
                sender,
                ready: false,
            },
        );
        let control = ComponentControl {
            id,
            generation,
            attachment,
            plane: Arc::downgrade(self),
        };
        drop(state);
        self.changed();
        Ok((control, receiver))
    }

    /// Obtain a sealed sender for an existing attachment without replacing its
    /// inbox or resetting readiness. Available immediately after `attach`, before
    /// realization or handler installation. Missing or mismatched generations
    /// are rejected; this does not create or revive a component.
    pub(crate) fn sender(
        &self,
        id: &ComponentId,
        generation: ComponentGeneration,
    ) -> Result<ComponentControl, ControlError> {
        let state = self.lock_open()?;
        let endpoint = state
            .components
            .get(id)
            .filter(|endpoint| endpoint.generation == generation)
            .ok_or_else(|| ControlError::StaleComponent {
                id: id.clone(),
                generation,
            })?;
        Ok(ComponentControl {
            id: id.clone(),
            generation,
            attachment: endpoint.attachment.clone(),
            plane: self.weak_self.clone(),
        })
    }

    /// Remove only the matching generation. Declared connections remain until the
    /// host updates them, so a missing downstream attachment cannot appear ready.
    pub(crate) fn remove(
        &self,
        id: &ComponentId,
        generation: ComponentGeneration,
    ) -> Result<(), ControlError> {
        let mut state = self.lock_open()?;
        match state.components.get(id) {
            Some(endpoint) if endpoint.generation == generation => {}
            _ => {
                return Err(ControlError::StaleComponent {
                    id: id.clone(),
                    generation,
                })
            }
        }
        state.components.remove(id);
        drop(state);
        self.changed();
        Ok(())
    }

    /// Permanently revoke every attachment and binding, clear readiness, and drop
    /// all mailbox senders. Idempotent; a closed plane cannot be reattached.
    ///
    /// Buffered messages may still be received but fail delivery validation.
    /// The watch is notified so host-supervised pending callbacks can be dropped.
    /// This method waits for no data operation or handler and starts no tasks.
    pub(crate) fn close(&self) -> Result<(), ControlError> {
        let mut state = self.state.lock().map_err(|_| ControlError::Poisoned)?;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        state.components.clear();
        state.connections.clear();
        drop(state);
        self.changed();
        Ok(())
    }

    /// Replace directed `(upstream, downstream)` bindings. Duplicate pairs collapse.
    ///
    /// Every call is a rebind boundary, including identical pairs: all previously
    /// queued messages become stale. Call at binding changes, not on each polling
    /// tick. Component attachments and their readiness survive; reattach a
    /// component to invalidate its handles too. Unattached peers are permitted for
    /// declared subscriptions but cannot receive or satisfy downstream readiness.
    /// Use `sync_connections` to preserve bindings during incremental topology updates.
    pub(crate) fn set_connections(
        &self,
        pairs: impl IntoIterator<Item = (ComponentId, ComponentId)>,
    ) -> Result<(), ControlError> {
        let pairs: BTreeSet<_> = pairs.into_iter().collect();
        Self::validate_connections(&pairs)?;
        let connections = pairs.into_iter().map(|pair| (pair, Arc::new(()))).collect();
        let mut state = self.lock_open()?;
        state.connections = connections;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Synchronize adjacency without invalidating retained bindings, except for
    /// pairs explicitly listed in `rebound`. New or rebound pairs receive fresh
    /// tokens; removed pairs cannot validate old messages, even if later re-added.
    ///
    /// Both inputs reject self-connections before mutation and collapse duplicate
    /// pairs. Rebound entries outside `pairs` do not create connections. Readiness
    /// and attachments are unchanged; an unchanged synchronization emits no wakeup.
    pub(crate) fn sync_connections(
        &self,
        pairs: impl IntoIterator<Item = (ComponentId, ComponentId)>,
        rebound: impl IntoIterator<Item = (ComponentId, ComponentId)>,
    ) -> Result<(), ControlError> {
        let pairs: BTreeSet<_> = pairs.into_iter().collect();
        let rebound: BTreeSet<_> = rebound.into_iter().collect();
        Self::validate_connections(&pairs)?;
        Self::validate_connections(&rebound)?;
        let mut state = self.lock_open()?;
        if state.connections.keys().eq(pairs.iter()) && pairs.is_disjoint(&rebound) {
            return Ok(());
        }
        let connections = pairs
            .into_iter()
            .map(|pair| {
                let connection = match state.connections.get(&pair) {
                    Some(connection) if !rebound.contains(&pair) => connection.clone(),
                    _ => Arc::new(()),
                };
                (pair, connection)
            })
            .collect();
        state.connections = connections;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Check both current endpoints and the exact binding before host dispatch
    /// and on control-watch changes while that handler is pending.
    pub(crate) fn validate_delivery(&self, message: &PeerMessage) -> Result<(), ControlError> {
        let state = self.lock_open()?;
        state.validate_attachment(
            &message.from,
            message.generation,
            &message.sender_attachment,
        )?;
        state.validate_attachment(
            &message.recipient,
            message.recipient_generation,
            &message.recipient_attachment,
        )?;
        let pair = match message.direction {
            ControlDirection::Upstream => (message.recipient.clone(), message.from.clone()),
            ControlDirection::Downstream => (message.from.clone(), message.recipient.clone()),
        };
        match state.connections.get(&pair) {
            Some(connection) if Arc::ptr_eq(connection, &message.connection) => Ok(()),
            _ => Err(ControlError::StaleConnection {
                from: message.from.clone(),
                to: message.recipient.clone(),
            }),
        }
    }

    /// Whether the current attachment at this generation has explicitly become
    /// ready. Missing, stale, initially not-ready, closed, or poisoned state
    /// returns false. This read does not publish a watch notification.
    pub(crate) fn is_ready(&self, id: &ComponentId, generation: ComponentGeneration) -> bool {
        let Ok(state) = self.lock_open() else {
            return false;
        };
        state
            .components
            .get(id)
            .is_some_and(|endpoint| endpoint.generation == generation && endpoint.ready)
    }

    /// Clear readiness at a host-owned activation boundary without enqueueing a
    /// notification or requiring mailbox capacity. Always wakes the watch on
    /// success, even if already not ready. Existing queued messages are unchanged.
    pub(crate) fn reset_ready(
        &self,
        id: &ComponentId,
        generation: ComponentGeneration,
    ) -> Result<(), ControlError> {
        let mut state = self.lock_open()?;
        let endpoint = state
            .components
            .get_mut(id)
            .filter(|endpoint| endpoint.generation == generation)
            .ok_or_else(|| ControlError::StaleComponent {
                id: id.clone(),
                generation,
            })?;
        endpoint.ready = false;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Whether every immediately downstream attachment has explicitly become
    /// ready. An attached leaf is ready vacuously; an unknown ID, missing peer,
    /// closed plane, or poisoned state fails closed. No data queue is inspected.
    pub(crate) fn downstream_ready(&self, id: &ComponentId) -> bool {
        let Ok(state) = self.lock_open() else {
            return false;
        };
        state.components.contains_key(id)
            && state
                .connections
                .keys()
                .filter(|(upstream, _)| upstream == id)
                .all(|(_, downstream)| {
                    state
                        .components
                        .get(downstream)
                        .is_some_and(|endpoint| endpoint.ready)
                })
    }

    /// Coalescing wakeups for accepted messages, readiness, topology, and closure.
    /// The revision is a wrapping wakeup token, not a delivery count or epoch.
    /// Drop borrowed revision guards before making mutating control calls.
    /// Subscribe before validating delivery so a subsequent rebind cannot be
    /// missed by an in-flight handler's cancellation check.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn validate_connections(
        pairs: &BTreeSet<(ComponentId, ComponentId)>,
    ) -> Result<(), ControlError> {
        if let Some((id, _)) = pairs
            .iter()
            .find(|(upstream, downstream)| upstream == downstream)
        {
            return Err(ControlError::SelfConnection { id: id.clone() });
        }
        Ok(())
    }

    fn lock_open(&self) -> Result<MutexGuard<'_, State>, ControlError> {
        let state = self.state.lock().map_err(|_| ControlError::Poisoned)?;
        if state.closed {
            return Err(ControlError::PlaneClosed);
        }
        Ok(state)
    }

    fn changed(&self) {
        // Do not hold the state lock while acquiring watch's write lock: a host
        // may inspect readiness while borrowing the watch revision.
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

impl ControlNotification {
    fn validate_payload(&self) -> Result<(), ControlError> {
        match self {
            Self::Unavailable { reason } => {
                PayloadBudget::new(MAX_CONTROL_PAYLOAD_BYTES).consume(reason.len())
            }
            Self::Custom { kind, payload } => {
                let mut budget = PayloadBudget::new(MAX_CONTROL_PAYLOAD_BYTES);
                budget.consume(kind.len())?;
                validate_json_shape(payload, budget.remaining)?;
                serde_json::to_writer(&mut budget, payload).map_err(|_| payload_too_large())
            }
            _ => Ok(()),
        }
    }
}

fn payload_too_large() -> ControlError {
    ControlError::PayloadTooLarge {
        limit: MAX_CONTROL_PAYLOAD_BYTES,
    }
}

/// Bound the traversal before recursive serialization/cloning, including wide
/// arrays. Charging children before pushing keeps the traversal stack bounded.
fn validate_json_shape(value: &Value, remaining: usize) -> Result<(), ControlError> {
    let mut budget = PayloadBudget::new(remaining);
    budget.consume(1)?;
    let mut pending = vec![(value, 0)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_CONTROL_JSON_DEPTH {
            return Err(ControlError::PayloadTooDeep {
                limit: MAX_CONTROL_JSON_DEPTH,
            });
        }
        match value {
            Value::String(value) => budget.consume(value.len())?,
            Value::Array(values) => {
                budget.consume(values.len())?;
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                budget.consume(values.len())?;
                for (key, value) in values {
                    budget.consume(key.len())?;
                    pending.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct PayloadBudget {
    remaining: usize,
}

impl PayloadBudget {
    fn new(remaining: usize) -> Self {
        Self { remaining }
    }

    fn consume(&mut self, bytes: usize) -> Result<(), ControlError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(payload_too_large)?;
        Ok(())
    }
}

impl Write for PayloadBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.consume(bytes.len())
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use super::*;

    fn id(value: &str) -> ComponentId {
        ComponentId::try_new(value).expect("valid component ID")
    }

    fn attach(
        plane: &Arc<ControlPlane>,
        name: &str,
    ) -> (ComponentControl, mpsc::Receiver<PeerMessage>) {
        plane
            .attach(id(name), ComponentGeneration(1))
            .expect("attach component")
    }

    fn connect(plane: &ControlPlane, pairs: &[(&str, &str)]) {
        plane
            .set_connections(
                pairs
                    .iter()
                    .map(|(upstream, downstream)| (id(upstream), id(downstream))),
            )
            .expect("connect components");
    }

    #[test]
    fn invalid_capacities_are_rejected_not_repaired() {
        for capacity in [0, usize::MAX] {
            assert!(matches!(
                ControlPlane::new(capacity),
                Err(ControlError::InvalidCapacity { capacity: actual }) if actual == capacity
            ));
        }
    }

    #[test]
    fn sender_reuses_current_attachment_without_resetting_readiness_or_inbox() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, _sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        sink.ready().expect("publish readiness");
        let changes = plane.subscribe();

        let sender = plane
            .as_ref()
            .sender(sink.id(), sink.generation())
            .expect("sender is available before realization");
        assert_eq!(sender.id(), sink.id());
        assert_eq!(sender.generation(), sink.generation());
        assert!(Arc::ptr_eq(&sender.attachment, &sink.attachment));
        assert!(!changes.has_changed().expect("lookup does not mutate state"));
        assert!(plane.downstream_ready(source.id()));
        plane
            .validate_delivery(&source_rx.try_recv().expect("original readiness message"))
            .expect("lookup preserves queued bindings");

        sender.not_ready().expect("retrieved sender can publish");
        assert!(!plane.downstream_ready(source.id()));
        assert!(changes
            .has_changed()
            .expect("readiness wakes the controller"));
        sink.ready().expect("original handle is still valid");
        assert!(plane.downstream_ready(source.id()));
        for expected in [ControlNotification::NotReady, ControlNotification::Ready] {
            let message = source_rx.try_recv().expect("same inbox");
            assert_eq!(message.notification, expected);
            plane.validate_delivery(&message).expect("same attachment");
        }
    }

    #[test]
    fn sender_rejects_missing_and_stale_generations_and_retains_attachment_fencing() {
        let plane = ControlPlane::new(1).expect("create plane");
        assert!(matches!(
            plane.sender(&id("source"), ComponentGeneration(1)),
            Err(ControlError::StaleComponent { .. })
        ));
        let (source, _source_rx) = attach(&plane, "source");
        let old = plane
            .sender(source.id(), source.generation())
            .expect("current sender");
        let (_, _replacement_rx) = plane
            .attach(id("source"), ComponentGeneration(2))
            .expect("replace component");
        for generation in [ComponentGeneration(1), ComponentGeneration(3)] {
            assert!(matches!(
                plane.sender(&id("source"), generation),
                Err(ControlError::StaleComponent { generation: rejected, .. })
                    if rejected == generation
            ));
        }
        assert!(matches!(
            old.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
        let replacement = plane
            .sender(&id("source"), ComponentGeneration(2))
            .expect("replacement sender");
        replacement
            .ready()
            .expect("current sender works without realization");

        let (_, _rebound_rx) = plane
            .attach(id("source"), ComponentGeneration(2))
            .expect("same-generation reattach");
        assert!(matches!(
            replacement.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
        let rebound = plane
            .sender(&id("source"), ComponentGeneration(2))
            .expect("new attachment sender");
        rebound.ready().expect("rebound sender is current");
        plane
            .remove(&id("source"), ComponentGeneration(2))
            .expect("remove component");
        assert!(matches!(
            plane.sender(&id("source"), ComponentGeneration(2)),
            Err(ControlError::StaleComponent { .. })
        ));
        assert!(matches!(
            rebound.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
    }

    #[test]
    fn routes_upstream_downstream_and_specific_neighbors() {
        let plane = ControlPlane::new(4).expect("create plane");
        let (upstream, mut upstream_rx) = attach(&plane, "upstream");
        let (middle, mut middle_rx) = attach(&plane, "middle");
        let (_, mut downstream_rx) = attach(&plane, "downstream");
        let (_, mut unrelated_rx) = attach(&plane, "unrelated");
        connect(&plane, &[("upstream", "middle"), ("middle", "downstream")]);

        assert_eq!(middle.id(), &id("middle"));
        assert_eq!(middle.generation(), ComponentGeneration(1));
        middle
            .notify_upstream(ControlNotification::Available)
            .expect("send upstream");
        middle
            .notify_downstream(ControlNotification::Custom {
                kind: "checkpoint".into(),
                payload: serde_json::json!({"sequence": 9}),
            })
            .expect("send downstream");
        let up = upstream_rx.try_recv().expect("upstream message");
        let down = downstream_rx.try_recv().expect("downstream message");
        for message in [&up, &down] {
            assert_eq!(message.from, id("middle"));
            assert_eq!(message.generation, ComponentGeneration(1));
            plane.validate_delivery(message).expect("current binding");
        }
        assert_eq!(up.direction, ControlDirection::Upstream);
        assert_eq!(up.notification, ControlNotification::Available);
        assert_eq!(down.direction, ControlDirection::Downstream);
        assert_eq!(
            down.notification,
            ControlNotification::Custom {
                kind: "checkpoint".into(),
                payload: serde_json::json!({"sequence": 9}),
            }
        );

        middle
            .notify_neighbor(upstream.id(), ControlNotification::NotReady)
            .expect("notify one upstream");
        middle
            .notify_neighbor(&id("downstream"), ControlNotification::Available)
            .expect("notify one downstream");
        assert_eq!(
            upstream_rx.try_recv().expect("targeted upstream").direction,
            ControlDirection::Upstream
        );
        assert_eq!(
            downstream_rx
                .try_recv()
                .expect("targeted downstream")
                .direction,
            ControlDirection::Downstream
        );
        assert!(matches!(
            middle_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            unrelated_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn rejects_unrelated_indirect_unknown_and_self_destinations() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, _middle_rx) = attach(&plane, "middle");
        let (_, mut sink_rx) = attach(&plane, "sink");
        let (_, mut unrelated_rx) = attach(&plane, "unrelated");
        connect(&plane, &[("source", "middle"), ("middle", "sink")]);

        for destination in ["sink", "unrelated", "missing", "source"] {
            assert_eq!(
                source.notify_neighbor(&id(destination), ControlNotification::Available),
                Err(ControlError::NotNeighbor {
                    from: id("source"),
                    to: id(destination),
                })
            );
        }
        assert!(sink_rx.try_recv().is_err());
        assert!(unrelated_rx.try_recv().is_err());
    }

    #[test]
    fn successful_fanout_reaches_every_neighbor_in_only_the_requested_direction() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (middle, mut middle_rx) = attach(&plane, "middle");
        let (_, first_upstream) = attach(&plane, "up-a");
        let (_, last_upstream) = attach(&plane, "up-z");
        let (_, first_downstream) = attach(&plane, "down-a");
        let (_, last_downstream) = attach(&plane, "down-z");
        connect(
            &plane,
            &[
                ("up-a", "middle"),
                ("up-z", "middle"),
                ("middle", "down-a"),
                ("middle", "down-z"),
            ],
        );
        middle
            .notify_upstream(ControlNotification::Available)
            .expect("upstream fanout");
        middle
            .notify_downstream(ControlNotification::NotReady)
            .expect("downstream fanout");
        for (mut receiver, direction, notification) in [
            (
                first_upstream,
                ControlDirection::Upstream,
                ControlNotification::Available,
            ),
            (
                last_upstream,
                ControlDirection::Upstream,
                ControlNotification::Available,
            ),
            (
                first_downstream,
                ControlDirection::Downstream,
                ControlNotification::NotReady,
            ),
            (
                last_downstream,
                ControlDirection::Downstream,
                ControlNotification::NotReady,
            ),
        ] {
            let message = receiver.try_recv().expect("fanout notification");
            assert_eq!(message.from, id("middle"));
            assert_eq!(message.direction, direction);
            assert_eq!(message.notification, notification);
            plane.validate_delivery(&message).expect("current binding");
            assert!(receiver.try_recv().is_err());
        }
        assert!(middle_rx.try_recv().is_err());
    }

    #[test]
    fn full_fanout_reserves_atomically_and_releases_failed_reservations() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut first_rx) = attach(&plane, "a");
        let (_, mut last_rx) = attach(&plane, "z");
        connect(&plane, &[("source", "a"), ("source", "z")]);
        source
            .notify_neighbor(&id("z"), ControlNotification::Available)
            .expect("fill final recipient");
        let changes = plane.subscribe();

        assert_eq!(
            source.notify_downstream(ControlNotification::NotReady),
            Err(ControlError::QueueFull { recipient: id("z") })
        );
        assert!(!changes.has_changed().expect("live plane"));
        assert!(matches!(
            first_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        source
            .notify_neighbor(&id("a"), ControlNotification::Ready)
            .expect("failed reservation was released");
        assert_eq!(
            first_rx.try_recv().expect("first recipient").notification,
            ControlNotification::Ready
        );
        assert_eq!(
            last_rx
                .try_recv()
                .expect("original notification")
                .notification,
            ControlNotification::Available
        );
        assert!(last_rx.try_recv().is_err());
    }

    #[test]
    fn closed_or_unattached_neighbor_rejects_without_partial_fanout() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut first_rx) = attach(&plane, "a");
        let (_, closed_rx) = attach(&plane, "z");
        connect(&plane, &[("source", "a"), ("source", "z")]);
        drop(closed_rx);
        assert_eq!(
            source.notify_downstream(ControlNotification::Available),
            Err(ControlError::Closed { recipient: id("z") })
        );
        assert!(first_rx.try_recv().is_err());
        connect(&plane, &[("source", "a"), ("source", "missing")]);
        assert_eq!(
            source.notify_downstream(ControlNotification::Available),
            Err(ControlError::Closed {
                recipient: id("missing")
            })
        );
        assert!(first_rx.try_recv().is_err());
    }

    #[test]
    fn removal_replacement_and_same_generation_reattach_invalidate_handles() {
        let plane = ControlPlane::new(4).expect("create plane");
        let (old, _) = attach(&plane, "source");
        let old_clone = old.clone();
        let (_, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        old.notify_downstream(ControlNotification::Available)
            .expect("old send");
        plane
            .remove(old.id(), old.generation())
            .expect("remove old attachment");
        assert!(matches!(
            old.notify_downstream(ControlNotification::Available),
            Err(ControlError::StaleComponent { .. })
        ));
        let old_message = sink_rx.try_recv().expect("old queued message");
        assert!(matches!(
            plane.validate_delivery(&old_message),
            Err(ControlError::StaleComponent { .. })
        ));

        let (reattached, _) = attach(&plane, "source");
        assert!(matches!(
            old_clone.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
        assert!(matches!(
            plane.validate_delivery(&old_message),
            Err(ControlError::StaleComponent { .. })
        ));
        let (replacement, _) = plane
            .attach(id("source"), ComponentGeneration(2))
            .expect("newer generation");
        assert!(matches!(
            reattached.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
        assert!(matches!(
            plane.remove(&id("source"), ComponentGeneration(1)),
            Err(ControlError::StaleComponent { .. })
        ));
        assert!(matches!(
            plane.attach(id("source"), ComponentGeneration(1)),
            Err(ControlError::StaleComponent { .. })
        ));
        replacement
            .notify_downstream(ControlNotification::Available)
            .expect("late old operations did not remove replacement");
        plane
            .validate_delivery(&sink_rx.try_recv().expect("replacement message"))
            .expect("replacement binding is current");
    }

    #[test]
    fn replacing_recipient_invalidates_old_queue_even_at_same_generation() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (old_sink, mut old_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send to old recipient");
        let (_, mut new_rx) = attach(&plane, "sink");
        let message = old_rx
            .try_recv()
            .expect("old mailbox retains queued message");
        assert!(matches!(
            plane.validate_delivery(&message),
            Err(ControlError::StaleComponent { id: stale, .. }) if stale == id("sink")
        ));
        assert!(matches!(
            old_sink.ready(),
            Err(ControlError::StaleComponent { .. })
        ));
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send to new attachment");
        plane
            .validate_delivery(&new_rx.try_recv().expect("new recipient message"))
            .expect("new attachment validates");
        assert!(matches!(
            old_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn every_rebind_invalidates_queued_messages_including_identical_pairs() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send before rebind");
        connect(&plane, &[("source", "sink")]);
        let old = sink_rx.try_recv().expect("queued old binding");
        assert!(matches!(
            plane.validate_delivery(&old),
            Err(ControlError::StaleConnection { .. })
        ));
        source
            .notify_downstream(ControlNotification::Available)
            .expect("surviving attachment uses current binding");
        let current = sink_rx.try_recv().expect("current message");
        plane.validate_delivery(&current).expect("current binding");

        connect(&plane, &[]);
        assert!(matches!(
            source.notify_neighbor(&id("sink"), ControlNotification::Available),
            Err(ControlError::NotNeighbor { .. })
        ));
        connect(&plane, &[("source", "sink")]);
        assert!(matches!(
            plane.validate_delivery(&current),
            Err(ControlError::StaleConnection { .. })
        ));
    }

    #[test]
    fn connection_sync_preserves_queued_messages_across_unrelated_node_and_link_changes() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, mut sink_rx) = attach(&plane, "sink");
        let retained = (id("source"), id("sink"));
        connect(&plane, &[("source", "sink")]);
        source.ready().expect("source ready without upstream");
        sink.ready().expect("queue upstream readiness");
        source
            .notify_downstream(ControlNotification::Available)
            .expect("queue downstream notification");

        let (other_source, _other_source_rx) = attach(&plane, "other-source");
        let (other_sink, mut other_sink_rx) = attach(&plane, "other-sink");
        let added = (id("other-source"), id("other-sink"));
        let changes = plane.subscribe();
        plane
            .sync_connections([retained.clone(), added], [])
            .expect("add unrelated link");
        assert!(changes
            .has_changed()
            .expect("new link wakes the controller"));
        other_source
            .notify_downstream(ControlNotification::Available)
            .expect("new link can route");
        plane
            .validate_delivery(&other_sink_rx.try_recv().expect("new link message"))
            .expect("new link has a valid token");
        plane
            .remove(other_sink.id(), other_sink.generation())
            .expect("remove unrelated node");
        plane
            .sync_connections([retained.clone()], [])
            .expect("remove unrelated link");
        let changes = plane.subscribe();
        plane
            .sync_connections([retained.clone(), retained], [])
            .expect("unchanged duplicate-normalized refresh");
        assert!(!changes
            .has_changed()
            .expect("unchanged refresh is read-only"));

        for (receiver, direction, notification) in [
            (
                &mut source_rx,
                ControlDirection::Upstream,
                ControlNotification::Ready,
            ),
            (
                &mut sink_rx,
                ControlDirection::Downstream,
                ControlNotification::Available,
            ),
        ] {
            let message = receiver.try_recv().expect("retained queued message");
            assert_eq!(message.direction, direction);
            assert_eq!(message.notification, notification);
            plane
                .validate_delivery(&message)
                .expect("unrelated changes preserve the queued binding");
        }
        assert!(plane.is_ready(source.id(), source.generation()));
        assert!(plane.is_ready(sink.id(), sink.generation()));
        assert!(plane.downstream_ready(source.id()));
    }

    #[test]
    fn connection_sync_rebind_invalidates_only_targeted_messages_in_both_directions() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (target, mut target_rx) = attach(&plane, "target");
        let (retained, mut retained_rx) = attach(&plane, "retained");
        let target_pair = (id("source"), id("target"));
        let retained_pair = (id("source"), id("retained"));
        connect(&plane, &[("source", "target"), ("source", "retained")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("queue downstream fanout");
        target
            .notify_upstream(ControlNotification::Available)
            .expect("queue targeted upstream message");
        retained
            .notify_upstream(ControlNotification::Available)
            .expect("queue retained upstream message");
        let changes = plane.subscribe();
        plane
            .sync_connections(
                [retained_pair, target_pair.clone()],
                [target_pair.clone(), target_pair],
            )
            .expect("rebind one pair");
        assert!(changes
            .has_changed()
            .expect("rebind wakes pending handlers"));

        for message in [
            target_rx.try_recv().expect("old target downstream"),
            source_rx.try_recv().expect("old target upstream"),
        ] {
            assert!(matches!(
                plane.validate_delivery(&message),
                Err(ControlError::StaleConnection { .. })
            ));
        }
        for message in [
            retained_rx.try_recv().expect("retained downstream"),
            source_rx.try_recv().expect("retained upstream"),
        ] {
            plane
                .validate_delivery(&message)
                .expect("untargeted queued binding is still valid");
        }
        source
            .notify_neighbor(target.id(), ControlNotification::Available)
            .expect("same attachment uses the fresh targeted binding");
        plane
            .validate_delivery(&target_rx.try_recv().expect("new target message"))
            .expect("rebound pair has a fresh valid token");
    }

    #[test]
    fn connection_sync_removal_rejects_old_bindings_without_reviving_removed_peers() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _source_rx) = attach(&plane, "source");
        let (sink, mut sink_rx) = attach(&plane, "sink");
        let (_, mut retained_rx) = attach(&plane, "retained");
        let removed = (id("source"), id("sink"));
        let retained = (id("source"), id("retained"));
        connect(&plane, &[("source", "sink"), ("source", "retained")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("queue on both bindings");
        plane
            .sync_connections([retained.clone()], [removed.clone()])
            .expect("removed rebound entries do not recreate links");
        let old = sink_rx.try_recv().expect("old removed binding");
        assert!(matches!(
            plane.validate_delivery(&old),
            Err(ControlError::StaleConnection { .. })
        ));
        assert!(matches!(
            source.notify_neighbor(sink.id(), ControlNotification::Available),
            Err(ControlError::NotNeighbor { .. })
        ));
        plane
            .validate_delivery(&retained_rx.try_recv().expect("unaffected binding"))
            .expect("removing one link preserves another");

        plane
            .sync_connections([retained.clone(), removed.clone()], [])
            .expect("re-add link with a new token");
        assert!(matches!(
            plane.validate_delivery(&old),
            Err(ControlError::StaleConnection { .. })
        ));
        source
            .notify_neighbor(sink.id(), ControlNotification::Available)
            .expect("new binding routes");
        plane
            .remove(sink.id(), sink.generation())
            .expect("remove the recipient attachment");
        plane
            .sync_connections([retained, removed], [])
            .expect("synchronizing declared links cannot revive a peer");
        assert!(matches!(
            plane.validate_delivery(&sink_rx.try_recv().expect("removed peer message")),
            Err(ControlError::StaleComponent { id: stale, .. }) if stale == id("sink")
        ));
        assert!(matches!(
            source.notify_neighbor(sink.id(), ControlNotification::Available),
            Err(ControlError::Closed { .. })
        ));
    }

    #[test]
    fn connection_sync_validates_both_inputs_before_mutation() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _source_rx) = attach(&plane, "source");
        let (_, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("queue a valid binding");
        let changes = plane.subscribe();
        for (pairs, rebound, invalid) in [
            (vec![(id("source"), id("source"))], vec![], id("source")),
            (vec![], vec![(id("sink"), id("sink"))], id("sink")),
        ] {
            assert_eq!(
                plane.sync_connections(pairs, rebound),
                Err(ControlError::SelfConnection { id: invalid })
            );
            assert!(!changes.has_changed().expect("invalid sync is atomic"));
        }
        plane
            .validate_delivery(&sink_rx.try_recv().expect("original queued message"))
            .expect("invalid input preserved existing membership and binding");
    }

    #[test]
    fn public_message_fields_cannot_forge_membership_or_direction() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut sink_rx) = attach(&plane, "sink");
        let (_, _other_rx) = attach(&plane, "other");
        connect(&plane, &[("source", "sink"), ("other", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send");
        let message = sink_rx.try_recv().expect("receive");
        let mut forged = message.clone();
        forged.from = id("other");
        assert!(matches!(
            plane.validate_delivery(&forged),
            Err(ControlError::StaleComponent { .. })
        ));
        let mut forged = message.clone();
        forged.generation = ComponentGeneration(9);
        assert!(matches!(
            plane.validate_delivery(&forged),
            Err(ControlError::StaleComponent { .. })
        ));
        let mut forged = message;
        forged.direction = ControlDirection::Upstream;
        assert!(matches!(
            plane.validate_delivery(&forged),
            Err(ControlError::StaleConnection { .. })
        ));
    }

    #[test]
    fn readiness_is_explicit_persistent_and_tracks_all_current_downstream_peers() {
        let plane = ControlPlane::new(8).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (first, _first_rx) = attach(&plane, "first");
        let (second, _second_rx) = attach(&plane, "second");
        connect(&plane, &[("source", "first"), ("source", "second")]);
        assert!(!plane.downstream_ready(source.id()));
        assert!(plane.downstream_ready(first.id()));
        assert!(!plane.downstream_ready(&id("unknown")));
        first.ready().expect("first ready");
        assert!(!plane.downstream_ready(source.id()));
        second.ready().expect("second ready");
        assert!(plane.downstream_ready(source.id()));
        source_rx.try_recv().expect("first readiness message");
        source_rx.try_recv().expect("second readiness message");
        assert!(plane.downstream_ready(source.id()));
        second
            .notify_upstream(ControlNotification::Unavailable {
                reason: "idle data source".into(),
            })
            .expect("availability is separate");
        assert!(plane.downstream_ready(source.id()));
        second.not_ready().expect("withdraw readiness");
        assert!(!plane.downstream_ready(source.id()));
        second.ready().expect("ready again");
        plane
            .remove(first.id(), first.generation())
            .expect("remove ready peer");
        assert!(!plane.downstream_ready(source.id()));
        let (new_first, _new_rx) = attach(&plane, "first");
        assert!(!plane.downstream_ready(source.id()));
        new_first.ready().expect("new attachment must opt in");
        assert!(plane.downstream_ready(source.id()));
        connect(&plane, &[("source", "first"), ("source", "missing")]);
        assert!(!plane.downstream_ready(source.id()));
    }

    #[test]
    fn is_ready_reads_only_current_generation_state_and_never_infers_startup() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (component, _receiver) = attach(&plane, "component");
        let changes = plane.subscribe();
        assert!(plane.downstream_ready(component.id()));
        assert!(!plane.is_ready(component.id(), component.generation()));
        assert!(!plane.is_ready(&id("missing"), ComponentGeneration(1)));
        assert!(!changes.has_changed().expect("read-only inspection"));

        component.ready().expect("explicit readiness");
        assert!(plane.is_ready(component.id(), component.generation()));
        assert!(!plane.is_ready(component.id(), ComponentGeneration(0)));
        assert!(!plane.is_ready(component.id(), ComponentGeneration(2)));
        component
            .not_ready()
            .expect("explicit readiness withdrawal");
        assert!(!plane.is_ready(component.id(), component.generation()));
        component.ready().expect("ready again");

        let (replacement, _replacement_rx) = plane
            .attach(id("component"), ComponentGeneration(2))
            .expect("replacement starts not ready");
        assert!(!plane.is_ready(component.id(), component.generation()));
        assert!(!plane.is_ready(replacement.id(), replacement.generation()));
        replacement.ready().expect("replacement reports readiness");
        assert!(plane.is_ready(replacement.id(), replacement.generation()));
        assert!(!plane.is_ready(component.id(), component.generation()));

        let (rebound, _rebound_rx) = plane
            .attach(id("component"), ComponentGeneration(2))
            .expect("same-generation reattach resets readiness");
        assert!(!plane.is_ready(rebound.id(), rebound.generation()));
        rebound.ready().expect("new attachment reports readiness");
        plane
            .remove(rebound.id(), rebound.generation())
            .expect("remove ready attachment");
        assert!(!plane.is_ready(rebound.id(), rebound.generation()));
    }

    #[test]
    fn failed_readiness_fanout_is_not_a_hidden_state_change() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, _sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        sink.notify_upstream(ControlNotification::Available)
            .expect("fill upstream queue");
        let changes = plane.subscribe();
        assert!(matches!(sink.ready(), Err(ControlError::QueueFull { .. })));
        assert!(!plane.downstream_ready(source.id()));
        assert!(!plane.is_ready(sink.id(), sink.generation()));
        assert!(!changes.has_changed().expect("live plane"));
        source_rx.try_recv().expect("drain initial notification");
        sink.ready().expect("readiness accepted");
        assert!(plane.downstream_ready(source.id()));
        assert!(plane.is_ready(sink.id(), sink.generation()));
        assert!(changes.has_changed().expect("state change is observable"));
        assert!(matches!(
            sink.not_ready(),
            Err(ControlError::QueueFull { .. })
        ));
        assert!(plane.downstream_ready(source.id()));
        assert!(plane.is_ready(sink.id(), sink.generation()));
    }

    #[test]
    fn host_readiness_reset_ignores_full_inbox_but_rejects_stale_generations() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, _sink_rx) = plane
            .attach(id("sink"), ComponentGeneration(2))
            .expect("attach current generation");
        connect(&plane, &[("source", "sink")]);
        sink.ready().expect("set readiness and fill upstream queue");
        assert!(matches!(
            sink.not_ready(),
            Err(ControlError::QueueFull { .. })
        ));
        let mut changes = plane.subscribe();
        for generation in [ComponentGeneration(1), ComponentGeneration(3)] {
            assert_eq!(
                plane.reset_ready(sink.id(), generation),
                Err(ControlError::StaleComponent {
                    id: sink.id().clone(),
                    generation,
                })
            );
            assert!(plane.is_ready(sink.id(), sink.generation()));
            assert!(!changes
                .has_changed()
                .expect("stale reset is not a mutation"));
        }

        plane
            .reset_ready(sink.id(), sink.generation())
            .expect("host reset does not require upstream capacity");
        assert!(!plane.is_ready(sink.id(), sink.generation()));
        assert!(!plane.downstream_ready(source.id()));
        assert!(changes.has_changed().expect("reset wakes the controller"));
        assert!(matches!(
            sink.notify_upstream(ControlNotification::Available),
            Err(ControlError::QueueFull { .. })
        ));
        let revision = *changes.borrow_and_update();
        plane
            .reset_ready(sink.id(), sink.generation())
            .expect("reset when already not ready");
        assert!(changes.has_changed().expect("every successful reset wakes"));
        assert_ne!(*changes.borrow(), revision);
        assert_eq!(
            source_rx
                .try_recv()
                .expect("original queued readiness")
                .notification,
            ControlNotification::Ready
        );
        assert!(matches!(
            source_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn readiness_watch_and_peer_delivery_are_independent_of_blocked_data() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, _sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        let mut changes = plane.subscribe();
        let (data_sender, mut data_receiver) = mpsc::channel(1);
        data_sender.send(1).await.expect("fill data queue");
        let mut blocked_data = Box::pin(data_sender.send(2));
        assert!(
            tokio::time::timeout(Duration::from_millis(1), blocked_data.as_mut())
                .await
                .is_err()
        );

        sink.ready().expect("control does not wait for data");
        tokio::time::timeout(Duration::from_secs(1), changes.changed())
            .await
            .expect("readiness wakes independently")
            .expect("live watch");
        assert!(plane.downstream_ready(source.id()));
        let message = tokio::time::timeout(Duration::from_secs(1), source_rx.recv())
            .await
            .expect("control queue is responsive")
            .expect("readiness notification");
        assert_eq!(message.notification, ControlNotification::Ready);
        plane.validate_delivery(&message).expect("valid readiness");
        assert_eq!(data_receiver.try_recv().expect("data remains queued"), 1);
        blocked_data
            .await
            .expect("data completes only after capacity is freed");
        assert_eq!(data_receiver.recv().await, Some(2));
    }

    #[tokio::test]
    async fn handler_is_object_safe_and_only_runs_when_the_host_polls_it() {
        struct Reply;

        #[async_trait]
        impl ControlHandler for Reply {
            async fn on_message(
                &self,
                message: PeerMessage,
                control: ComponentControl,
            ) -> anyhow::Result<()> {
                control.notify_neighbor(&message.from, ControlNotification::Available)?;
                Ok(())
            }
        }

        let plane = ControlPlane::new(1).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send request");
        let message = sink_rx.try_recv().expect("request");
        plane.validate_delivery(&message).expect("host validation");
        let handler: Arc<dyn ControlHandler> = Arc::new(Reply);
        let callback = handler.on_message(message, sink);
        assert!(matches!(
            source_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        callback.await.expect("host polls callback");
        let reply = source_rx.try_recv().expect("handler replied");
        assert_eq!(reply.from, id("sink"));
        assert_eq!(reply.direction, ControlDirection::Upstream);
        plane.validate_delivery(&reply).expect("valid response");
    }

    async fn assert_invalidation_aborts_waiting_handler(
        invalidate: impl FnOnce(&ControlPlane),
        expected: ControlError,
    ) -> Arc<ControlPlane> {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        struct WaitingHandler {
            entered: Arc<tokio::sync::Notify>,
            dropped: Arc<AtomicBool>,
        }

        #[async_trait]
        impl ControlHandler for WaitingHandler {
            async fn on_message(
                &self,
                _message: PeerMessage,
                _control: ComponentControl,
            ) -> anyhow::Result<()> {
                let _drop_signal = DropSignal(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending().await
            }
        }

        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _source_rx) = attach(&plane, "source");
        let (sink, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("send request");
        let message = sink_rx.try_recv().expect("request");
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let handler = WaitingHandler {
            entered: entered.clone(),
            dropped: dropped.clone(),
        };
        let mut supervised = Box::pin(async {
            let mut changes = plane.subscribe();
            plane.validate_delivery(&message)?;
            let binding = message.clone();
            let callback = handler.on_message(message, sink.clone());
            tokio::pin!(callback);
            loop {
                tokio::select! {
                    biased;
                    changed = changes.changed() => {
                        changed.map_err(|_| ControlError::PlaneClosed)?;
                        plane.validate_delivery(&binding)?;
                    }
                    result = callback.as_mut() => {
                        result.expect("handler result");
                        return Ok::<(), ControlError>(());
                    }
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = entered.notified() => {}
                result = supervised.as_mut() => {
                    panic!("handler unexpectedly completed: {result:?}");
                }
            }
        })
        .await
        .expect("host polls handler until it waits");
        assert!(!dropped.load(Ordering::SeqCst));

        invalidate(&plane);
        let result = tokio::time::timeout(Duration::from_secs(1), supervised)
            .await
            .expect("invalidation wakes the host without any data operation");
        assert_eq!(result, Err(expected));
        assert!(dropped.load(Ordering::SeqCst));
        plane
    }

    #[tokio::test]
    async fn rebind_watch_allows_host_to_abort_an_already_waiting_handler() {
        let plane = assert_invalidation_aborts_waiting_handler(
            |plane| connect(plane, &[("source", "sink")]),
            ControlError::StaleConnection {
                from: id("source"),
                to: id("sink"),
            },
        )
        .await;
        plane
            .sender(&id("source"), ComponentGeneration(1))
            .expect("sender generation did not change");
        plane
            .sender(&id("sink"), ComponentGeneration(1))
            .expect("recipient generation did not change");
    }

    #[tokio::test]
    async fn close_watch_allows_host_to_abort_an_already_waiting_handler() {
        let plane = assert_invalidation_aborts_waiting_handler(
            |plane| plane.close().expect("close control plane"),
            ControlError::PlaneClosed,
        )
        .await;
        assert!(!plane.is_ready(&id("source"), ComponentGeneration(1)));
        assert!(!plane.is_ready(&id("sink"), ComponentGeneration(1)));
    }

    #[test]
    fn readiness_survives_without_a_watch_subscriber_or_notification_recipient() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (sink, _sink_rx) = attach(&plane, "sink");
        sink.ready().expect("readiness with no upstream");
        let (source, _source_rx) = attach(&plane, "source");
        connect(&plane, &[("source", "sink")]);
        let mut changes = plane.subscribe();
        assert!(*changes.borrow_and_update() > 0);
        assert!(plane.downstream_ready(source.id()));
        sink.not_ready().expect("withdraw readiness");
        assert!(changes.has_changed().expect("watch retained state"));
        assert!(!plane.downstream_ready(source.id()));
    }

    #[test]
    fn payload_bytes_escaped_json_and_depth_are_bounded_before_enqueue() {
        let plane = ControlPlane::new(2).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink")]);
        for notification in [
            ControlNotification::Unavailable {
                reason: "x".repeat(MAX_CONTROL_PAYLOAD_BYTES + 1),
            },
            ControlNotification::Custom {
                kind: "x".repeat(MAX_CONTROL_PAYLOAD_BYTES),
                payload: Value::Null,
            },
            ControlNotification::Custom {
                kind: String::new(),
                payload: Value::String("\0".repeat(MAX_CONTROL_PAYLOAD_BYTES / 6 + 1)),
            },
            ControlNotification::Custom {
                kind: String::new(),
                payload: Value::Array(vec![Value::Null; MAX_CONTROL_PAYLOAD_BYTES]),
            },
        ] {
            assert!(matches!(
                source.notify_downstream(notification),
                Err(ControlError::PayloadTooLarge { .. })
            ));
        }
        let mut payload = Value::Null;
        for _ in 0..=MAX_CONTROL_JSON_DEPTH {
            payload = Value::Array(vec![payload]);
        }
        assert!(matches!(
            source.notify_downstream(ControlNotification::Custom {
                kind: "deep".into(),
                payload,
            }),
            Err(ControlError::PayloadTooDeep { .. })
        ));
        assert!(sink_rx.try_recv().is_err());

        source
            .notify_downstream(ControlNotification::Unavailable {
                reason: "x".repeat(MAX_CONTROL_PAYLOAD_BYTES),
            })
            .expect("reason exactly at limit");
        source
            .notify_downstream(ControlNotification::Custom {
                kind: String::new(),
                payload: Value::String("x".repeat(MAX_CONTROL_PAYLOAD_BYTES - 2)),
            })
            .expect("encoded JSON exactly at limit");
        assert!(sink_rx.try_recv().is_ok());
        assert!(sink_rx.try_recv().is_ok());
    }

    #[test]
    fn invalid_topology_is_atomic_and_duplicate_pairs_do_not_duplicate_delivery() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, _) = attach(&plane, "source");
        let (_, mut sink_rx) = attach(&plane, "sink");
        connect(&plane, &[("source", "sink"), ("source", "sink")]);
        source
            .notify_downstream(ControlNotification::Available)
            .expect("one slot suffices for duplicate pair");
        let queued = sink_rx.try_recv().expect("one message");
        assert!(matches!(
            plane.set_connections([(id("source"), id("source"))]),
            Err(ControlError::SelfConnection { .. })
        ));
        plane
            .validate_delivery(&queued)
            .expect("failed topology change preserved binding");
        assert!(sink_rx.try_recv().is_err());
    }

    #[test]
    fn close_revokes_handles_readiness_and_queued_messages_and_drops_senders() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (source, mut source_rx) = attach(&plane, "source");
        let (sink, mut sink_rx) = attach(&plane, "sink");
        let retrieved = plane
            .sender(source.id(), source.generation())
            .expect("retrieve sealed sender");
        connect(&plane, &[("source", "sink")]);
        source.ready().expect("ready without upstream recipients");
        sink.ready().expect("sink readiness enqueued upstream");
        source
            .notify_downstream(ControlNotification::Available)
            .expect("fill downstream queue");
        let mut changes = plane.subscribe();

        plane.close().expect("close with both queues full");
        assert!(changes.has_changed().expect("closure wakes the controller"));
        let revision = *changes.borrow_and_update();
        for control in [&source, &sink, &retrieved, &source.clone()] {
            assert!(!plane.is_ready(control.id(), control.generation()));
            assert!(!plane.downstream_ready(control.id()));
            assert_eq!(control.ready(), Err(ControlError::PlaneClosed));
            assert_eq!(control.not_ready(), Err(ControlError::PlaneClosed));
            assert_eq!(
                control.notify_upstream(ControlNotification::Available),
                Err(ControlError::PlaneClosed)
            );
            assert_eq!(
                control.notify_downstream(ControlNotification::Available),
                Err(ControlError::PlaneClosed)
            );
            assert_eq!(
                control.notify_neighbor(&id("sink"), ControlNotification::Available),
                Err(ControlError::PlaneClosed)
            );
            assert!(matches!(
                plane.sender(control.id(), control.generation()),
                Err(ControlError::PlaneClosed)
            ));
        }
        for receiver in [&mut source_rx, &mut sink_rx] {
            let buffered = receiver.try_recv().expect("previously accepted message");
            assert_eq!(
                plane.validate_delivery(&buffered),
                Err(ControlError::PlaneClosed)
            );
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
        assert_eq!(
            plane.set_connections([(id("source"), id("sink"))]),
            Err(ControlError::PlaneClosed)
        );
        assert_eq!(
            plane.sync_connections([], []),
            Err(ControlError::PlaneClosed)
        );
        assert_eq!(
            plane.remove(source.id(), source.generation()),
            Err(ControlError::PlaneClosed)
        );
        assert!(matches!(
            plane.attach(id("source"), ComponentGeneration(2)),
            Err(ControlError::PlaneClosed)
        ));
        plane.close().expect("repeated close is idempotent");
        assert!(!changes
            .has_changed()
            .expect("no change after terminal closure"));
        assert_eq!(*changes.borrow(), revision);
    }

    #[test]
    fn closure_is_permanent_even_for_empty_planes_and_reopen_requires_a_new_plane() {
        let empty = ControlPlane::new(1).expect("empty plane");
        empty.close().expect("close without endpoints");
        assert!(matches!(
            empty.attach(id("component"), ComponentGeneration(1)),
            Err(ControlError::PlaneClosed)
        ));
        assert!(matches!(
            empty.sender(&id("component"), ComponentGeneration(1)),
            Err(ControlError::PlaneClosed)
        ));

        let old_plane = ControlPlane::new(1).expect("old plane");
        let (old, mut old_rx) = attach(&old_plane, "component");
        old_plane.close().expect("close disconnected component");
        assert_eq!(old.ready(), Err(ControlError::PlaneClosed));
        let new_plane = ControlPlane::new(1).expect("new plane");
        let (new, _new_rx) = attach(&new_plane, "component");
        new.ready().expect("same ID and generation on fresh plane");
        assert!(new_plane.is_ready(new.id(), new.generation()));
        assert!(!old_plane.is_ready(old.id(), old.generation()));
        assert_eq!(old.ready(), Err(ControlError::PlaneClosed));
        assert!(matches!(
            old_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn control_handles_do_not_keep_the_graph_or_mailboxes_alive() {
        let plane = ControlPlane::new(1).expect("create plane");
        let (control, mut receiver) = attach(&plane, "component");
        let clone = control.clone();
        let sender = plane
            .sender(control.id(), control.generation())
            .expect("retrieved sender");
        drop(plane);
        assert_eq!(control.ready(), Err(ControlError::PlaneClosed));
        assert_eq!(clone.not_ready(), Err(ControlError::PlaneClosed));
        assert_eq!(sender.ready(), Err(ControlError::PlaneClosed));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
