// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;

use super::{
    AcceptanceReceipt, AcceptedComponentCreation, AcceptedComponentUpdate, CommittedConfiguration,
    ComponentUpdateCompletion, ConfigurationSession, ConfigurationSnapshotSummary,
    ConfigurationStore, ConfigurationStoreGuarantees, DesiredInstance, ManagementError,
};

/// One private storage owner per DrasiLib. This holds records, not runtime objects.
#[derive(Default)]
pub(crate) struct MemoryConfigStore {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
    owner: Option<u64>,
    next_owner: u64,
}

struct Session {
    state: Arc<Mutex<State>>,
    owner: u64,
}

impl Session {
    fn state(&self) -> anyhow::Result<MutexGuard<'_, State>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("memory configuration storage ownership is poisoned"))?;
        anyhow::ensure!(state.owner == Some(self.owner), ManagementError::Closed);
        Ok(state)
    }
}

#[async_trait]
impl ConfigurationStore for MemoryConfigStore {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        ConfigurationStoreGuarantees::MEMORY
    }

    async fn open(&self, instance_id: &str) -> anyhow::Result<Arc<dyn ConfigurationSession>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("memory configuration storage ownership is poisoned"))?;
        anyhow::ensure!(
            state.owner.is_none(),
            ManagementError::AlreadyOwned(instance_id.into())
        );
        let owner = state
            .next_owner
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("memory configuration owner generation exhausted"))?;
        state.next_owner = owner;
        state.owner = Some(owner);
        Ok(Arc::new(Session {
            state: self.state.clone(),
            owner,
        }))
    }
}

#[async_trait]
impl ConfigurationSession for Session {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration> {
        Ok(self.state()?.current.clone())
    }

    async fn commit(
        &self,
        expected_revision: u64,
        request_id: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> anyhow::Result<AcceptanceReceipt> {
        let mut state = self.state()?;
        if let Some((previous, receipt)) = state.receipts.get(request_id) {
            anyhow::ensure!(
                previous == desired
                    && receipt.creation.as_ref() == creation
                    && receipt.updates == updates,
                ManagementError::RequestConflict
            );
            return Ok(receipt.clone());
        }
        anyhow::ensure!(
            state.current.revision == expected_revision,
            ManagementError::RevisionConflict {
                expected: expected_revision,
                actual: state.current.revision,
            }
        );
        let revision = if state.current.desired == *desired {
            expected_revision
        } else {
            expected_revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("configuration revision exhausted"))?
        };
        let receipt = AcceptanceReceipt {
            request_id: request_id.into(),
            revision,
            durable: false,
            creation: creation.cloned(),
            updates: updates.to_vec(),
        };
        let mut pending = state.current.pending_updates.clone();
        for update in updates {
            anyhow::ensure!(
                !pending
                    .iter()
                    .any(|old| old.component_id == update.component_id),
                "component already has an incomplete accepted update"
            );
            pending.push(update.clone());
        }
        pending.sort_by(|left, right| left.component_id.cmp(&right.component_id));
        state.current = CommittedConfiguration {
            revision,
            desired: desired.clone(),
            pending_updates: pending,
        };
        state
            .receipts
            .insert(request_id.into(), (desired.clone(), receipt.clone()));
        Ok(receipt)
    }

    async fn complete_updates(
        &self,
        expected_revision: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.state()?;
        anyhow::ensure!(
            state.current.revision == expected_revision,
            ManagementError::RevisionConflict {
                expected: expected_revision,
                actual: state.current.revision
            }
        );
        for update in updates {
            if let Some(pending) = state
                .current
                .pending_updates
                .iter()
                .find(|pending| pending.component_id == update.component_id)
            {
                anyhow::ensure!(
                    pending.operation_id == update.operation_id,
                    ManagementError::RequestConflict
                );
            }
        }
        state.current.pending_updates.retain(|pending| {
            !updates.iter().any(|update| {
                update.component_id == pending.component_id
                    && update.operation_id == pending.operation_id
            })
        });
        Ok(state.current.clone())
    }

    async fn receipt(&self, request_id: &str) -> anyhow::Result<Option<AcceptanceReceipt>> {
        Ok(self
            .state()?
            .receipts
            .get(request_id)
            .map(|(_, receipt)| receipt.clone()))
    }

    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.state()?;
        anyhow::ensure!(
            !state.snapshots.contains_key(name),
            ManagementError::SnapshotExists(name.into())
        );
        let current = state.current.clone();
        state.snapshots.insert(name.into(), current.clone());
        Ok(current)
    }

    async fn load_snapshot(&self, name: &str) -> anyhow::Result<Option<CommittedConfiguration>> {
        Ok(self.state()?.snapshots.get(name).cloned())
    }

    async fn list_snapshots(
        &self,
        after: Option<&str>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<ConfigurationSnapshotSummary>> {
        Ok(self
            .state()?
            .snapshots
            .iter()
            .filter(|(name, _)| after.is_none_or(|after| name.as_str() > after))
            .take(limit.get())
            .map(|(name, snapshot)| ConfigurationSnapshotSummary {
                name: name.clone(),
                revision: snapshot.revision,
            })
            .collect())
    }

    async fn delete_snapshot(&self, name: &str, expected_revision: u64) -> anyhow::Result<bool> {
        let mut state = self.state()?;
        let Some(snapshot) = state.snapshots.get(name) else {
            return Ok(false);
        };
        anyhow::ensure!(
            snapshot.revision == expected_revision,
            ManagementError::RevisionConflict {
                expected: expected_revision,
                actual: snapshot.revision
            }
        );
        state.snapshots.remove(name);
        Ok(true)
    }

    async fn close(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("memory configuration storage ownership is poisoned"))?;
        if state.owner == Some(self.owner) {
            state.owner = None;
        }
        Ok(())
    }
}
