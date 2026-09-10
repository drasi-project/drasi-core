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
//! Data lives below `computation-v1/<encoded graph>/<encoded query>`, never in a
//! legacy query's database. Output staging requires this bundle's active session.

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

fn scope_segment(id: &str) -> Result<String, IndexError> {
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
    Ok(id.bytes().map(|byte| format!("{byte:02x}")).collect())
}

#[async_trait]
impl ComputationIndexProvider for RocksDbComputationProvider {
    async fn create_indexes(
        &self,
        graph_id: &str,
        query_id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let graph = scope_segment(graph_id)?;
        let query = scope_segment(query_id)?;
        let path = self.path.join("computation-v1").join(graph);
        let path = path.to_str().ok_or_else(|| {
            IndexError::other(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "computation index path must be valid UTF-8",
            ))
        })?;
        let db = open_unified_db(path, &query, &self.options)?;
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
        Ok(resources.with_cleanup(owner))
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::scope_segment;

    #[test]
    fn scope_segments_are_unambiguous_and_cannot_escape_the_graph_directory() {
        assert_ne!(scope_segment("a/b").unwrap(), scope_segment("a-b").unwrap());
        assert_eq!(scope_segment("../query").unwrap(), "2e2e2f7175657279");
        for invalid in ["", "a b", "a\0b", "a\nb"] {
            assert!(scope_segment(invalid).is_err());
        }
    }
}
