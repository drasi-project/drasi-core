// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::config::{Connection, Routine};
use anyhow::{Context, Result};
use drasi_lib::state_store::StateStoreProvider;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{broadcast, Mutex};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Records {
    pub connections: BTreeMap<String, Connection>,
    pub routines: BTreeMap<String, Routine>,
    pub runs: BTreeMap<String, Run>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub routine_id: String,
    pub agent: String,
    pub input: Value,
    pub status: String,
    pub response: Option<Value>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub attempts: u8,
}

pub struct Store {
    pub provider: Arc<dyn StateStoreProvider>,
    records: Mutex<Records>,
    pub events: broadcast::Sender<Value>,
}

impl Store {
    pub async fn open(provider: Arc<dyn StateStoreProvider>) -> Result<Arc<Self>> {
        let mut records: Records = match provider.get("foundry-demo", "records").await? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).context("Invalid saved demo configuration")?
            }
            None => Records::default(),
        };
        for routine in records.routines.values_mut() {
            if ["Active", "Starting", "Paused"].contains(&routine.status.as_str()) {
                routine.status = "Interrupted".into();
                routine.error =
                    Some("Query state was rebuilt. Resume watches future changes only.".into());
            }
        }
        for run in records.runs.values_mut() {
            match run.status.as_str() {
                "Submitting" => {
                    run.status = "Unknown".into();
                    run.error = Some(
                        "Process stopped during submission. Reconcile before resubmitting.".into(),
                    );
                }
                "Queued" => {
                    run.status = "Cancelled".into();
                    run.error = Some("Not submitted before interruption.".into());
                }
                _ => {}
            }
        }
        provider
            .set("foundry-demo", "records", serde_json::to_vec(&records)?)
            .await?;
        let (events, _) = broadcast::channel(256);
        Ok(Arc::new(Self {
            provider,
            records: Mutex::new(records),
            events,
        }))
    }

    pub async fn read(&self) -> Records {
        self.records.lock().await.clone()
    }

    pub async fn update<T>(&self, apply: impl FnOnce(&mut Records) -> Result<T>) -> Result<T> {
        let mut guard = self.records.lock().await;
        let mut next = guard.clone();
        let output = apply(&mut next)?;
        self.provider
            .set("foundry-demo", "records", serde_json::to_vec(&next)?)
            .await?;
        *guard = next;
        let _ = self.events.send(serde_json::json!({"type":"refresh"}));
        Ok(output)
    }
}
