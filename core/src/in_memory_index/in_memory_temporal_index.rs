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

use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::{
    evaluation::temporal::{
        codec::{
            decode_catalog, decode_record, encode_catalog, encode_key, epoch_prefix, prepare_batch,
            CATALOG_KEY,
        },
        QueryEpoch, TemporalBatch, TemporalCatalog, TemporalKey, TemporalRecord,
    },
    interface::{IndexError, TemporalIndex},
};

/// Immediate, nontransactional storage. The root must fence a dirty aborted query.
#[derive(Default)]
pub struct InMemoryTemporalIndex {
    records: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl InMemoryTemporalIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TemporalIndex for InMemoryTemporalIndex {
    async fn load_catalog(&self) -> Result<Option<TemporalCatalog>, IndexError> {
        self.records
            .read()
            .await
            .get(CATALOG_KEY)
            .map(|bytes| decode_catalog(bytes).map_err(IndexError::other))
            .transpose()
    }

    async fn store_catalog(&self, catalog: TemporalCatalog) -> Result<(), IndexError> {
        let bytes = encode_catalog(&catalog).map_err(IndexError::other)?;
        self.records
            .write()
            .await
            .insert(CATALOG_KEY.to_vec(), bytes);
        Ok(())
    }

    async fn get(&self, key: &TemporalKey) -> Result<Option<TemporalRecord>, IndexError> {
        let storage_key = encode_key(key).map_err(IndexError::other)?;
        self.records
            .read()
            .await
            .get(&storage_key)
            .map(|bytes| decode_record(key, bytes).map_err(IndexError::other))
            .transpose()
    }

    async fn apply(&self, batch: TemporalBatch) -> Result<(), IndexError> {
        let writes = prepare_batch(&batch).map_err(IndexError::other)?;
        let mut records = self.records.write().await;
        for (key, value) in writes {
            match value {
                Some(value) => {
                    records.insert(key, value);
                }
                None => {
                    records.remove(&key);
                }
            }
        }
        Ok(())
    }

    async fn clear_epoch(&self, epoch: QueryEpoch) -> Result<(), IndexError> {
        let prefix = epoch_prefix(epoch);
        self.records
            .write()
            .await
            .retain(|key, _| !key.starts_with(&prefix));
        Ok(())
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.records.write().await.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::temporal::{
        EpochState, Incarnation, InputOrigin, MatchIdentity, OriginKey, PartId, PlanVersion,
        SourceRevision, TemporalInputId, TemporalNamespace,
    };

    #[tokio::test]
    async fn batches_read_the_last_write_and_clear_only_the_requested_epoch() {
        let index = InMemoryTemporalIndex::new();
        let ns = TemporalNamespace {
            epoch: QueryEpoch([1; 16]),
            plan: PlanVersion(1),
        };
        let next_plan = TemporalNamespace {
            plan: PlanVersion(2),
            ..ns
        };
        let other = TemporalNamespace {
            epoch: QueryEpoch([2; 16]),
            ..ns
        };
        for namespace in [ns, other] {
            let mut batch = TemporalBatch::new(namespace);
            batch
                .put(TemporalRecord::Epoch(EpochState::new(namespace)))
                .unwrap();
            index.apply(batch).await.unwrap();
        }
        let origin = OriginKey {
            namespace: next_plan,
            part: PartId(0),
            origin: InputOrigin::Match(MatchIdentity::Fixed { slots: vec![] }),
        };
        let obsolete_key = TemporalKey::Origin(origin.clone());
        let mut obsolete = TemporalBatch::new(next_plan);
        obsolete
            .put(TemporalRecord::Origin {
                key: origin,
                input: TemporalInputId {
                    namespace: next_plan,
                    part: PartId(0),
                    incarnation: Incarnation(9),
                },
            })
            .unwrap();
        index.apply(obsolete).await.unwrap();
        let mut batch = TemporalBatch::new(ns);
        batch.delete(TemporalKey::Epoch(ns)).unwrap();
        batch
            .put(TemporalRecord::Epoch(EpochState {
                namespace: ns,
                next_incarnation: Incarnation(8),
            }))
            .unwrap();
        index.apply(batch).await.unwrap();
        let Some(TemporalRecord::Epoch(state)) = index.get(&TemporalKey::Epoch(ns)).await.unwrap()
        else {
            panic!("missing epoch record");
        };
        assert_eq!(state.next_incarnation, Incarnation(8));
        index.clear_epoch(ns.epoch).await.unwrap();
        index.clear_epoch(ns.epoch).await.unwrap();
        assert!(index.get(&TemporalKey::Epoch(ns)).await.unwrap().is_none());
        assert!(index.get(&obsolete_key).await.unwrap().is_none());
        assert!(index
            .get(&TemporalKey::Epoch(other))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn an_old_plan_manifest_is_not_treated_as_empty_state() {
        let index = InMemoryTemporalIndex::new();
        let old = TemporalNamespace {
            epoch: QueryEpoch([4; 16]),
            plan: PlanVersion(1),
        };
        let new = TemporalNamespace {
            plan: PlanVersion(2),
            ..old
        };
        let mut batch = TemporalBatch::new(old);
        batch
            .put(TemporalRecord::Epoch(EpochState::new(old)))
            .unwrap();
        index.apply(batch).await.unwrap();
        assert!(index.get(&TemporalKey::Epoch(new)).await.is_err());
    }

    #[tokio::test]
    async fn corrupt_records_are_errors_not_missing_rows() {
        let index = InMemoryTemporalIndex::new();
        let key = TemporalKey::Epoch(TemporalNamespace {
            epoch: QueryEpoch([3; 16]),
            plan: PlanVersion(1),
        });
        index
            .records
            .write()
            .await
            .insert(encode_key(&key).unwrap(), b"legacy".to_vec());
        assert!(index.get(&key).await.is_err());
    }

    #[tokio::test]
    async fn catalog_discovers_namespace_without_a_known_epoch() {
        let index = InMemoryTemporalIndex::new();
        assert!(index.load_catalog().await.unwrap().is_none());
        let catalog = TemporalCatalog {
            namespace: TemporalNamespace {
                epoch: QueryEpoch([8; 16]),
                plan: PlanVersion(9),
            },
            query: "MATCH (n)\nRETURN n  ".into(),
            next_revision: SourceRevision(47),
        };
        index.store_catalog(catalog.clone()).await.unwrap();
        assert_eq!(index.load_catalog().await.unwrap(), Some(catalog.clone()));
        index.clear_epoch(catalog.namespace.epoch).await.unwrap();
        assert_eq!(index.load_catalog().await.unwrap(), Some(catalog.clone()));
        index
            .store_catalog(TemporalCatalog {
                next_revision: SourceRevision(48),
                ..catalog.clone()
            })
            .await
            .unwrap();
        assert_eq!(
            index.load_catalog().await.unwrap().unwrap().next_revision,
            SourceRevision(48)
        );
    }

    #[tokio::test]
    async fn clear_removes_every_epoch_and_corrupt_catalog_without_decoding() {
        let index = InMemoryTemporalIndex::new();
        for epoch in [QueryEpoch([6; 16]), QueryEpoch([7; 16])] {
            let namespace = TemporalNamespace {
                epoch,
                plan: PlanVersion(1),
            };
            let mut batch = TemporalBatch::new(namespace);
            batch
                .put(TemporalRecord::Epoch(EpochState::new(namespace)))
                .unwrap();
            index.apply(batch).await.unwrap();
        }
        {
            let mut records = index.records.write().await;
            records.insert(CATALOG_KEY.to_vec(), b"corrupt catalog".to_vec());
            records.insert(b"unrecognized temporal key".to_vec(), vec![255]);
        }
        assert!(index.load_catalog().await.is_err());
        index.clear().await.unwrap();
        index.clear().await.unwrap();
        assert!(index.load_catalog().await.unwrap().is_none());
        assert!(index.records.read().await.is_empty());
    }

    #[tokio::test]
    async fn unsupported_catalog_version_is_discovered_and_rejected() {
        let index = InMemoryTemporalIndex::new();
        let catalog = TemporalCatalog {
            namespace: TemporalNamespace {
                epoch: QueryEpoch([8; 16]),
                plan: PlanVersion(1),
            },
            query: "RETURN 1".into(),
            next_revision: SourceRevision(0),
        };
        let mut bytes = encode_catalog(&catalog).unwrap();
        bytes[4..6].copy_from_slice(&99_u16.to_be_bytes());
        index
            .records
            .write()
            .await
            .insert(CATALOG_KEY.to_vec(), bytes);
        let error = index.load_catalog().await.unwrap_err();
        assert!(matches!(
            error,
            IndexError::Other(error)
                if matches!(
                    error.downcast_ref::<crate::evaluation::temporal::codec::TemporalCodecError>(),
                    Some(crate::evaluation::temporal::codec::TemporalCodecError::UnsupportedVersion { found: 99 })
                )
        ));
        index.clear().await.unwrap();
        assert!(index.load_catalog().await.unwrap().is_none());
    }
}
