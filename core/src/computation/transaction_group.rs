// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use async_trait::async_trait;
use tokio::sync::OwnedMutexGuard;

use crate::interface::{IndexError, SessionControl};

use super::{ComputationIndexes, ComputationResourceCleanup, ComputationTransaction, Result};

const MAX_JOURNALS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum TransactionGroupError {
    #[error("the shared transaction group requires cleanup and reconstruction")]
    Closed,
    #[error("the shared transaction group still has an active operation")]
    OperationInProgress,
    #[error("the shared transaction group already has a processor")]
    ProcessorInUse,
    #[error("shared journal '{0}' already has an owner")]
    JournalInUse(String),
    #[error("a shared journal name must fit in 256 bytes without whitespace or controls")]
    InvalidJournal,
    #[error("a shared transaction group supports at most 256 active journals")]
    JournalLimit,
    #[error("the shared member has no active transaction")]
    NoActiveTransaction,
    #[error("a shared transaction supports at most 256 staged mutations")]
    MutationLimit,
    #[error("shared transaction ownership is poisoned")]
    Poisoned,
}

fn failure(error: TransactionGroupError) -> IndexError {
    IndexError::other(error)
}

#[derive(Default)]
struct Members {
    processor: Weak<GroupSession>,
    journals: BTreeMap<String, Weak<GroupSession>>,
}

type RetirementGate = Mutex<Option<OwnedMutexGuard<()>>>;

struct Group {
    resources: ComputationIndexes,
    gate: Arc<tokio::sync::Mutex<()>>,
    closed: AtomicBool,
    changed: tokio::sync::Notify,
    members: Mutex<Members>,
    retirement: Mutex<Weak<RetirementGate>>,
}

impl Group {
    fn contains(self: &Arc<Self>, transaction: &ComputationTransaction) -> bool {
        !self.closed.load(Ordering::Acquire)
            && !transaction.recovery_required()
            && transaction
                .resources()
                .group_session
                .as_ref()
                .is_some_and(|session| Arc::ptr_eq(self, &session.group))
    }

    fn check(&self) -> std::result::Result<(), IndexError> {
        if self.closed.load(Ordering::Acquire) {
            Err(failure(TransactionGroupError::Closed))
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
        if let Some(cleanup) = self.resources.cleanup() {
            cleanup.cancel();
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// One explicitly shared provider session, owned independently of its members.
///
/// Use a dedicated storage bundle; this does not migrate standalone output.
/// One processor owns the evaluator indexes. Journals share its proven output
/// writers and the same serialized session, not another database or transaction
/// coordinator. Each member's outbox keys have a separate persisted namespace.
/// Journals must keep metadata in their outbox, not the processor's checkpoints.
/// Closing a healthy member does not close its siblings or the provider.
/// An interrupted active transaction or a dropped owner fences the entire group.
/// Members retain storage cleanup ownership even after the owner is dropped.
pub struct ComputationTransactionGroup {
    group: Arc<Group>,
}

/// Holds the actual shared storage gate while a caller verifies retirement and
/// durably accepts its replacement. This is not itself evidence that work drained.
/// Dropping an unresolved guard fences all members; only a confirmed rejection
/// may resume the old owner. Sealing is process-local, so restart requires a new
/// guard unless the replacement configuration was durably accepted.
/// Terminal owner shutdown revokes the gate without reopening any member.
#[must_use]
pub struct TransactionGroupRetirement {
    group: Arc<Group>,
    _gate: Arc<RetirementGate>,
    resolved: bool,
}

impl TransactionGroupRetirement {
    pub fn journal_count(&self) -> Result<usize> {
        self.group.check()?;
        let members = self
            .group
            .members
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))?;
        Ok(members
            .journals
            .values()
            .filter(|journal| journal.strong_count() != 0)
            .count())
    }

    pub fn contains(&self, transaction: &ComputationTransaction) -> bool {
        self.group.contains(transaction)
    }

    pub fn has_processor(&self) -> Result<bool> {
        self.group.check()?;
        let members = self
            .group
            .members
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))?;
        Ok(members
            .processor
            .upgrade()
            .is_some_and(|processor| !processor.recovery_required()))
    }

    pub async fn has_scheduled_work(&self) -> Result<bool> {
        self.group.check()?;
        let pending = self
            .group
            .resources
            .indexes()
            .future_queue
            .peek_due_time()
            .await?;
        self.group.check()?;
        Ok(pending.is_some())
    }

    /// Call only after checking all obligations and confirming acceptance.
    pub fn retire(mut self) {
        self.group.cancel();
        self.resolved = true;
    }

    /// Release the gate only when the proposed transition definitely did not commit.
    pub fn resume(mut self) {
        self.resolved = true;
    }
}

impl Drop for TransactionGroupRetirement {
    fn drop(&mut self) {
        if !self.resolved {
            self.group.cancel();
        }
    }
}

/// A mutation staged by the current group member, not an independent transaction.
/// `committed` updates local committed views while the group is still locked.
/// Failure or cancellation after storage commit fences every group member.
/// Retain weak owner handles to avoid cycles through the pending mutation list.
#[async_trait]
pub trait TransactionGroupMutation: Send {
    async fn stage(
        &mut self,
        context: &TransactionGroupContext<'_>,
    ) -> std::result::Result<(), IndexError>;

    async fn committed(&mut self) -> std::result::Result<(), IndexError> {
        Ok(())
    }
}

/// Borrowed only while the group owner drives a staged mutation.
pub struct TransactionGroupContext<'a> {
    session: &'a GroupSession,
}

impl TransactionGroupContext<'_> {
    pub fn require_member(&self, transaction: &ComputationTransaction) -> Result<()> {
        if transaction.recovery_required() || self.session.recovery_required() {
            return Err(super::ComputationQueryError::RecoveryRequired);
        }
        if !transaction
            .resources()
            .group_session
            .as_ref()
            .is_some_and(|other| self.session.same_group(other))
        {
            return Err(super::ComputationQueryError::TransactionMismatch);
        }
        Ok(())
    }
}

impl ComputationTransactionGroup {
    pub fn try_new(resources: ComputationIndexes) -> Result<Self> {
        resources.atomic_result_transaction()?;
        if resources.group_session.is_some() {
            return Err(super::ComputationQueryError::TransactionMismatch);
        }
        Ok(Self {
            group: Arc::new(Group {
                resources,
                gate: Arc::new(tokio::sync::Mutex::new(())),
                closed: AtomicBool::new(false),
                changed: tokio::sync::Notify::new(),
                members: Mutex::new(Members::default()),
                retirement: Mutex::new(Weak::new()),
            }),
        })
    }

    pub fn durability(&self) -> super::StorageDurability {
        self.group.resources.durability()
    }

    pub fn recovery_required(&self) -> bool {
        self.group.closed.load(Ordering::Acquire)
    }

    pub fn contains(&self, transaction: &ComputationTransaction) -> bool {
        self.group.contains(transaction)
    }

    pub fn processor_indexes(&self) -> Result<ComputationIndexes> {
        let session = self.claim(None)?;
        self.group.resources.for_group_member(session, None)
    }

    /// Independent journal operations acquire the shared session. Do not nest
    /// this transaction inside a processor transaction that already holds it.
    pub fn journal_transaction(&self, name: &str) -> Result<ComputationTransaction> {
        if name.is_empty()
            || name.len() > 256
            || name
                .chars()
                .any(|value| value.is_whitespace() || value.is_control())
        {
            return Err(failure(TransactionGroupError::InvalidJournal).into());
        }
        let session = self.claim(Some(name))?;
        ComputationTransaction::try_new(self.group.resources.for_group_member(session, Some(name))?)
    }

    fn claim(&self, journal: Option<&str>) -> std::result::Result<Arc<GroupSession>, IndexError> {
        self.group.check()?;
        let mut members = self
            .group
            .members
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))?;
        let session = Arc::new(GroupSession {
            group: self.group.clone(),
            closed: AtomicBool::new(false),
            active: Mutex::new(None),
            mutations: tokio::sync::Mutex::new(Vec::new()),
        });
        if let Some(journal) = journal {
            members
                .journals
                .retain(|_, member| member.strong_count() != 0);
            if members.journals.contains_key(journal) {
                return Err(failure(TransactionGroupError::JournalInUse(journal.into())));
            }
            if members.journals.len() == MAX_JOURNALS {
                return Err(failure(TransactionGroupError::JournalLimit));
            }
            members
                .journals
                .insert(journal.into(), Arc::downgrade(&session));
        } else {
            if members.processor.strong_count() != 0 {
                return Err(failure(TransactionGroupError::ProcessorInUse));
            }
            members.processor = Arc::downgrade(&session);
        }
        Ok(session)
    }

    pub fn cancel(&self) {
        self.group.cancel();
    }

    /// Refuse rather than sampling storage while another operation is in flight.
    /// Writers stay excluded until the retirement guard is resolved or dropped.
    pub fn freeze_for_retirement(&self) -> Result<TransactionGroupRetirement> {
        self.group.check()?;
        let gate = self
            .group
            .gate
            .clone()
            .try_lock_owned()
            .map_err(|_| failure(TransactionGroupError::OperationInProgress))?;
        self.group.check()?;
        let gate = Arc::new(Mutex::new(Some(gate)));
        *self
            .group
            .retirement
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))? = Arc::downgrade(&gate);
        self.group.check()?;
        Ok(TransactionGroupRetirement {
            group: self.group.clone(),
            _gate: gate,
            resolved: false,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.cancel();
        // Closing is terminal: release our own retirement gate without reopening
        // members, so uncertain configuration cannot prevent awaited shutdown.
        let retirement = self
            .group
            .retirement
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))?
            .upgrade();
        if let Some(retirement) = retirement {
            retirement
                .lock()
                .map_err(|_| failure(TransactionGroupError::Poisoned))?
                .take();
        }
        let _gate = self
            .group
            .gate
            .try_lock()
            .map_err(|_| failure(TransactionGroupError::OperationInProgress))?;
        if let Some(cleanup) = self.group.resources.cleanup() {
            cleanup.shutdown().await?;
        }
        self.group.resources.indexes().session_control.rollback()?;
        Ok(())
    }
}

impl Drop for ComputationTransactionGroup {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) struct GroupSession {
    group: Arc<Group>,
    closed: AtomicBool,
    active: Mutex<Option<ActiveOperation>>,
    mutations: tokio::sync::Mutex<Vec<Box<dyn TransactionGroupMutation>>>,
}

struct ActiveOperation {
    _gate: OwnedMutexGuard<()>,
    committed: bool,
}

impl GroupSession {
    fn active(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, Option<ActiveOperation>>, IndexError> {
        self.active
            .lock()
            .map_err(|_| failure(TransactionGroupError::Poisoned))
    }

    pub(super) fn recovery_required(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.group.closed.load(Ordering::Acquire)
    }

    pub(super) fn same_group(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.group, &other.group)
    }

    pub(super) async fn wait_for_failure(&self) {
        loop {
            let changed = self.group.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.recovery_required() {
                return;
            }
            changed.await;
        }
    }

    pub(super) async fn stage_mutations(
        &self,
        mutations: Vec<Box<dyn TransactionGroupMutation>>,
    ) -> std::result::Result<(), IndexError> {
        if mutations.len() > MAX_JOURNALS {
            return Err(failure(TransactionGroupError::MutationLimit));
        }
        if self.recovery_required() {
            return Err(failure(TransactionGroupError::Closed));
        }
        if !matches!(
            self.active()?.as_ref(),
            Some(ActiveOperation {
                committed: false,
                ..
            })
        ) {
            return Err(failure(TransactionGroupError::NoActiveTransaction));
        }
        let mut pending = self.mutations.lock().await;
        if !pending.is_empty() {
            return Err(failure(TransactionGroupError::OperationInProgress));
        }
        *pending = mutations;
        let context = TransactionGroupContext { session: self };
        for mutation in pending.iter_mut() {
            mutation.stage(&context).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl SessionControl for GroupSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        let group = &self.group;
        if self.recovery_required() {
            return Err(failure(TransactionGroupError::Closed));
        }
        let gate = tokio::select! {
            biased;
            _ = self.wait_for_failure() => return Err(failure(TransactionGroupError::Closed)),
            gate = group.gate.clone().lock_owned() => gate,
        };
        if self.recovery_required() {
            return Err(failure(TransactionGroupError::Closed));
        }
        {
            let mut active = self.active()?;
            if self.recovery_required() {
                return Err(failure(TransactionGroupError::Closed));
            }
            if active.is_some() {
                return Err(failure(TransactionGroupError::OperationInProgress));
            }
            *active = Some(ActiveOperation {
                _gate: gate,
                committed: false,
            });
        }
        if let Err(error) = group.resources.indexes().session_control.begin().await {
            group.cancel();
            return Err(error);
        }
        Ok(())
    }

    async fn commit(&self) -> std::result::Result<(), IndexError> {
        let group = &self.group;
        group.check()?;
        if self.active()?.is_none() {
            return Err(failure(TransactionGroupError::NoActiveTransaction));
        }
        if let Err(error) = group.resources.indexes().session_control.commit().await {
            group.cancel();
            return Err(error);
        }
        self.active()?
            .as_mut()
            .ok_or_else(|| failure(TransactionGroupError::NoActiveTransaction))?
            .committed = true;
        {
            let mut pending = self.mutations.lock().await;
            for mutation in pending.iter_mut() {
                if let Err(error) = mutation.committed().await {
                    group.cancel();
                    return Err(error);
                }
            }
            pending.clear();
        }
        self.active()?.take();
        Ok(())
    }

    fn rollback(&self) -> std::result::Result<(), IndexError> {
        let mut active = self.active()?;
        // A cancelled waiter must never roll back another member's transaction.
        if active.is_none() {
            return Ok(());
        }
        let group = &self.group;
        let mut pending = self.mutations.try_lock().map_err(|_| {
            group.cancel();
            failure(TransactionGroupError::OperationInProgress)
        })?;
        if !active.as_ref().expect("active operation").committed {
            if let Err(error) = group.resources.indexes().session_control.rollback() {
                group.cancel();
                return Err(error);
            }
        }
        pending.clear();
        active.take();
        Ok(())
    }
}

#[async_trait]
impl ComputationResourceCleanup for GroupSession {
    fn cancel(&self) {
        self.closed.store(true, Ordering::Release);
        self.group.changed.notify_waiters();
        match self.active() {
            Ok(active) if active.is_none() => {}
            result => {
                if let Err(error) = result {
                    log::error!("Shared member cancellation cannot inspect ownership: {error}");
                }
                self.group.cancel();
            }
        }
    }

    async fn quiesce(&self) -> std::result::Result<(), IndexError> {
        if self.recovery_required() {
            return Err(failure(TransactionGroupError::Closed));
        }
        if self.active()?.is_some() {
            return Err(failure(TransactionGroupError::OperationInProgress));
        }
        Ok(())
    }

    async fn shutdown(&self) -> std::result::Result<(), IndexError> {
        self.cancel();
        if self.active()?.is_some() {
            let group = &self.group;
            if let Some(cleanup) = group.resources.cleanup() {
                cleanup.quiesce().await?;
            }
            self.rollback()?;
        }
        Ok(())
    }
}

impl Drop for GroupSession {
    fn drop(&mut self) {
        self.cancel();
    }
}
