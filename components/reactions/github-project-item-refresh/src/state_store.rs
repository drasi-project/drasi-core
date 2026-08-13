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

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

use drasi_lib::state_store::StateStoreProvider;

use crate::models::{
    DeliveryKey, DeliveryReservation, ItemVersionRecord, PublicationRecord, PublicationState,
};

/// Durable state wrapper for reservations, publication attempts, and per-item versions.
#[derive(Clone)]
pub struct RefreshStateStore {
    store: Arc<dyn StateStoreProvider>,
    store_id: String,
}

impl RefreshStateStore {
    pub fn new(store: Arc<dyn StateStoreProvider>, reaction_id: impl Into<String>) -> Self {
        Self {
            store,
            store_id: format!("gh-project-item-refresh:{}", reaction_id.into()),
        }
    }

    pub async fn get_reservation(
        &self,
        key: &DeliveryKey,
    ) -> anyhow::Result<Option<DeliveryReservation>> {
        self.get_typed(&reservation_key(key))
            .await
            .context("reading reservation")
    }

    pub async fn set_reservation(
        &self,
        key: &DeliveryKey,
        value: &DeliveryReservation,
    ) -> anyhow::Result<()> {
        self.set_typed(&reservation_key(key), value)
            .await
            .context("writing reservation")
    }

    pub async fn get_publication(
        &self,
        key: &DeliveryKey,
    ) -> anyhow::Result<Option<PublicationRecord>> {
        self.get_typed(&publication_key(key))
            .await
            .context("reading publication record")
    }

    pub async fn set_publication(
        &self,
        key: &DeliveryKey,
        value: &PublicationRecord,
    ) -> anyhow::Result<()> {
        self.set_typed(&publication_key(key), value)
            .await
            .context("writing publication record")
    }

    pub async fn mark_failed(
        &self,
        key: &DeliveryKey,
        mut record: PublicationRecord,
        state: PublicationState,
        error_message: impl Into<String>,
    ) -> anyhow::Result<()> {
        record.state = state;
        record.last_error = Some(error_message.into());
        self.set_publication(key, &record).await
    }

    pub async fn prune_terminal_records_older_than(
        &self,
        ttl_secs: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let keys = self
            .store
            .list_keys(&self.store_id)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("listing state store keys for pruning")?;
        let cutoff = now
            .checked_sub_signed(Duration::seconds(ttl_secs as i64))
            .unwrap_or(now);

        let mut pruned = 0usize;
        for key in keys {
            if !key.starts_with("publication:") {
                continue;
            }

            let Some(record) = self.get_typed::<PublicationRecord>(&key).await? else {
                continue;
            };

            let is_terminal = matches!(
                record.state,
                PublicationState::Published | PublicationState::Stale | PublicationState::Rejected
            );
            let Some(completed_at) = record.completed_at else {
                continue;
            };
            if !is_terminal || completed_at >= cutoff {
                continue;
            }

            self.store
                .delete(&self.store_id, &key)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
                .with_context(|| format!("deleting publication key '{key}'"))?;

            if let Some(suffix) = key.strip_prefix("publication:") {
                let reservation_key = format!("reservation:{suffix}");
                self.store
                    .delete(&self.store_id, &reservation_key)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .with_context(|| format!("deleting reservation key '{reservation_key}'"))?;
            }

            pruned = pruned.saturating_add(1);
        }

        Ok(pruned)
    }

    pub async fn get_item_version(
        &self,
        project_item_node_id: &str,
    ) -> anyhow::Result<Option<ItemVersionRecord>> {
        self.get_typed(&version_key(project_item_node_id))
            .await
            .context("reading item version")
    }

    pub async fn set_item_version(
        &self,
        project_item_node_id: &str,
        value: &ItemVersionRecord,
    ) -> anyhow::Result<()> {
        self.set_typed(&version_key(project_item_node_id), value)
            .await
            .context("writing item version")
    }

    async fn get_typed<T: for<'de> serde::Deserialize<'de>>(
        &self,
        key: &str,
    ) -> anyhow::Result<Option<T>> {
        let bytes = self
            .store
            .get(&self.store_id, key)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .with_context(|| format!("state store get failed for key '{key}'"))?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let parsed = serde_json::from_slice(&bytes)
            .with_context(|| format!("deserializing state for key '{key}'"))?;
        Ok(Some(parsed))
    }

    async fn set_typed<T: serde::Serialize>(&self, key: &str, value: &T) -> anyhow::Result<()> {
        let encoded = serde_json::to_vec(value)
            .with_context(|| format!("serializing state for key '{key}'"))?;
        self.store
            .set(&self.store_id, key, encoded)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .with_context(|| format!("state store set failed for key '{key}'"))
    }
}

fn reservation_key(key: &DeliveryKey) -> String {
    format!("reservation:{}", key.as_storage_key())
}

fn publication_key(key: &DeliveryKey) -> String {
    format!("publication:{}", key.as_storage_key())
}

fn version_key(project_item_node_id: &str) -> String {
    format!("version:{project_item_node_id}")
}
