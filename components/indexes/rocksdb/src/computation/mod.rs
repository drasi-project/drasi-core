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

//! Explicit computation resources, separate from the ordinary index plugin.
//! Graph and component ownership use separate typed storage scopes. Output
//! staging requires this bundle's active session.
//! Bootstrap checkpoint clearing preserves committed output heads and generations.

mod checkpoint;
mod output;
mod session;

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{IndexError, IndexSet, SessionControl},
};
use drasi_core_types::{ComponentId, StorageScope};

use crate::{
    element_index::RocksDbElementIndex, future_queue::RocksDbFutureQueue, open_unified_db,
    result_index::RocksDbResultIndex, RocksDbSessionState, RocksIndexOptions,
};

use checkpoint::ComputationCheckpointStore;
use drasi_core::computation::{ComputationIoScope as BlockingScope, ScopedIndex};
use output::{ComputationLiveResultsWriter, ComputationOutboxWriter};
use session::ComputationSession;

pub struct RocksDbComputationProvider {
    path: PathBuf,
    options: RocksIndexOptions,
}

impl RocksDbComputationProvider {
    pub fn new(path: impl Into<PathBuf>, options: RocksIndexOptions) -> Self {
        Self {
            path: path.into(),
            options,
        }
    }
}

fn validate_graph_scope(id: &str) -> Result<(), IndexError> {
    if id.is_empty()
        || id
            .chars()
            .any(|value| value.is_control() || value.is_whitespace())
    {
        return Err(IndexError::other(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "computation scope identifiers must be nonempty without whitespace or controls",
        )));
    }
    Ok(())
}

#[async_trait]
impl ComputationIndexProvider for RocksDbComputationProvider {
    async fn create_indexes(
        &self,
        graph_id: &str,
        query_id: &ComponentId,
    ) -> Result<ComputationIndexes, IndexError> {
        validate_graph_scope(graph_id)?;
        let scope = StorageScope::new("computation").nested(&StorageScope::new(graph_id));
        let path = self.path.to_str().ok_or_else(|| {
            IndexError::other(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "computation index path must be valid UTF-8",
            ))
        })?;
        let db = open_unified_db(path, &scope, query_id, &self.options)?;
        let session = Arc::new(RocksDbSessionState::new(db.clone()));
        let work = Arc::new(BlockingScope::default());
        let owner = Arc::new(ComputationSession {
            state: session.clone(),
            work: work.clone(),
        });
        let control: Arc<dyn SessionControl> = owner.clone();
        let domain = TransactionDomain::new(control.clone());
        let elements = Arc::new(ScopedIndex::new(
            RocksDbElementIndex::new(db.clone(), self.options.clone(), session.clone()),
            work.clone(),
        ));
        let results = Arc::new(ScopedIndex::new(
            RocksDbResultIndex::new(db.clone(), session.clone(), self.options.clone()),
            work.clone(),
        ));
        let futures = Arc::new(ScopedIndex::new(
            RocksDbFutureQueue::new(db.clone(), session.clone(), self.options.clone()),
            work.clone(),
        ));
        let resources = ComputationIndexes::try_new(
            IndexSet {
                element_index: elements.clone(),
                archive_index: elements,
                result_index: results,
                future_queue: futures,
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                Arc::new(ComputationCheckpointStore::new(
                    db.clone(),
                    session.clone(),
                    work.clone(),
                )),
                &domain,
            )),
            Some(ComputationResource::participating(
                Arc::new(ComputationOutboxWriter::new(
                    db.clone(),
                    session.clone(),
                    work.clone(),
                )),
                &domain,
            )),
            Some(ComputationResource::participating(
                Arc::new(ComputationLiveResultsWriter::new(db, session, work)),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(resources
            .with_cleanup(owner)
            .with_durability(self.durability()))
    }

    fn is_volatile(&self) -> bool {
        false
    }

    fn durability(&self) -> drasi_core::computation::StorageDurability {
        drasi_core::computation::StorageDurability::LOCAL_PROCESS_RESTART
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_graph_scope, StorageScope};

    #[test]
    fn scope_segments_are_unambiguous_and_cannot_escape_the_graph_directory() {
        assert_ne!(
            StorageScope::new("a/b").storage_key(),
            StorageScope::new("a-b").storage_key()
        );
        let traversal = StorageScope::new("../query").storage_key();
        assert!(!traversal.contains('/'));
        assert!(!traversal.contains(".."));
        assert!(traversal.ends_with("2e2e2f7175657279"));
        for invalid in ["", "a b", "a\0b", "a\nb"] {
            assert!(validate_graph_scope(invalid).is_err());
        }
    }
}
