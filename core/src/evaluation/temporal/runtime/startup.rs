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

use std::sync::Arc;

use crate::{
    evaluation::temporal::{
        codec::TemporalCodecError, EpochState, PlanVersion, QueryEpoch, SourceRevision,
        TemporalBatch, TemporalCatalog, TemporalKey, TemporalNamespace, TemporalRecord,
    },
    interface::{SessionControl, SessionGuard, TemporalIndex},
};

use super::{TemporalRuntimeError, TemporalStartup};

pub const TEMPORAL_PLAN_VERSION: PlanVersion = PlanVersion(1);

pub async fn open_namespace(
    index: &dyn TemporalIndex,
    control: Arc<dyn SessionControl>,
    startup: TemporalStartup,
    query: &str,
) -> Result<TemporalNamespace, TemporalRuntimeError> {
    let root = SessionGuard::begin(control.clone()).await?;
    let catalog = index.load_catalog().await?;
    let namespace = match (startup, catalog) {
        (TemporalStartup::Create, Some(catalog)) => {
            return Err(TemporalRuntimeError::EpochAlreadyExists(catalog.namespace));
        }
        (TemporalStartup::Create, None) => {
            let namespace = TemporalNamespace {
                epoch: QueryEpoch(*uuid::Uuid::new_v4().as_bytes()),
                plan: TEMPORAL_PLAN_VERSION,
            };
            let mut batch = TemporalBatch::new(namespace);
            batch.put(TemporalRecord::Epoch(EpochState::new(namespace)))?;
            crate::interface::session_tracker(&control)?.mark_dirty()?;
            index.apply(batch).await?;
            index
                .store_catalog(TemporalCatalog {
                    namespace,
                    query: query.to_owned(),
                    next_revision: SourceRevision(0),
                })
                .await?;
            namespace
        }
        (TemporalStartup::Reopen, None) => {
            return Err(TemporalCodecError::MigrationRequired.into());
        }
        (TemporalStartup::Reopen, Some(catalog)) => {
            if catalog.namespace.plan != TEMPORAL_PLAN_VERSION {
                return Err(TemporalCodecError::PlanVersionMismatch {
                    expected: TEMPORAL_PLAN_VERSION.0,
                    found: catalog.namespace.plan.0,
                }
                .into());
            }
            if catalog.query != query {
                return Err(TemporalRuntimeError::QueryPlanChanged);
            }
            match index.get(&TemporalKey::Epoch(catalog.namespace)).await? {
                Some(TemporalRecord::Epoch(epoch)) if epoch.namespace == catalog.namespace => {}
                Some(_) => return Err(TemporalRuntimeError::UnexpectedRecord("epoch")),
                None => return Err(TemporalCodecError::MigrationRequired.into()),
            }
            catalog.namespace
        }
    };
    root.commit().await?;
    Ok(namespace)
}
