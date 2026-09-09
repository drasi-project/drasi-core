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

//! Graph-scoped Garnet resources. Index/checkpoint mutations use the session
//! buffer, but outbox and live results are independent: complete atomic output
//! is explicitly unsupported. No legacy plugin contract is extended.

mod checkpoint;

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{IndexError, IndexSet, SessionControl},
};

use crate::{
    element_index::GarnetElementIndex, future_queue::GarnetFutureQueue,
    result_index::GarnetResultIndex, GarnetLiveResultsWriter, GarnetOutboxWriter,
    GarnetSessionControl, GarnetSessionState,
};

use checkpoint::ComputationCheckpointStore;

pub struct GarnetComputationProvider {
    connection_string: String,
    enable_archive: bool,
}

impl GarnetComputationProvider {
    pub fn new(connection_string: impl Into<String>, enable_archive: bool) -> Self {
        Self {
            connection_string: connection_string.into(),
            enable_archive,
        }
    }
}

fn partition(graph_id: &str, query_id: &str) -> Result<String, IndexError> {
    for id in [graph_id, query_id] {
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
    }
    let encode = |id: &str| -> String { id.bytes().map(|byte| format!("{byte:02x}")).collect() };
    Ok(format!(
        "computation-v1:{}:{}",
        encode(graph_id),
        encode(query_id)
    ))
}

#[async_trait]
impl ComputationIndexProvider for GarnetComputationProvider {
    async fn create_indexes(
        &self,
        graph_id: &str,
        query_id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let partition = partition(graph_id, query_id)?;
        let connection = redis::Client::open(self.connection_string.as_str())
            .map_err(IndexError::connection_failed)?
            .get_multiplexed_async_connection()
            .await
            .map_err(IndexError::connection_failed)?;
        let session = Arc::new(GarnetSessionState::new(connection.clone()));
        let control: Arc<dyn SessionControl> = Arc::new(GarnetSessionControl::new(session.clone()));
        let domain = TransactionDomain::new(control.clone());
        let elements = Arc::new(GarnetElementIndex::new(
            &partition,
            connection.clone(),
            self.enable_archive,
            session.clone(),
        ));
        let results = Arc::new(GarnetResultIndex::new(
            &partition,
            connection.clone(),
            session.clone(),
        ));
        let futures = Arc::new(GarnetFutureQueue::new(
            &partition,
            connection.clone(),
            session.clone(),
        ));
        ComputationIndexes::try_new(
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
                    &partition,
                    connection.clone(),
                    session,
                )),
                &domain,
            )),
            Some(ComputationResource::independent(Arc::new(
                GarnetOutboxWriter::new(&partition, connection.clone()),
            ))),
            Some(ComputationResource::independent(Arc::new(
                GarnetLiveResultsWriter::new(&partition, connection),
            ))),
        )
        .map_err(IndexError::other)
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_partitions_preserve_field_boundaries_and_do_not_inject_hash_tags() {
        assert_ne!(partition("ab", "c").unwrap(), partition("a", "bc").unwrap());
        let encoded = partition("{legacy}", "query:*").unwrap();
        assert!(!encoded.contains(['{', '}', '*']));
        assert!(encoded.starts_with("computation-v1:"));
    }

    #[tokio::test]
    async fn invalid_graph_scope_is_rejected_before_connecting() {
        let provider = GarnetComputationProvider::new("not-a-redis-url", false);
        let error = provider.create_indexes("", "query").await.err().unwrap();
        assert!(error.to_string().contains("scope identifiers"));
    }
}
