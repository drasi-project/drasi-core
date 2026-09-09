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

#![cfg(feature = "computation")]

use drasi_core::{
    computation::{ComputationIndexProvider, ComputationQueryError},
    interface::{IndexBackendPlugin, SessionGuard},
};
use drasi_index_garnet::{computation::GarnetComputationProvider, GarnetIndexProvider};

#[tokio::test]
#[ignore = "requires Docker through the existing Redis fixture"]
async fn graph_checkpoint_joins_buffer_but_complete_output_remains_unsupported() {
    let redis = shared_tests::redis_helpers::setup_redis().await;
    {
        let provider = GarnetComputationProvider::new(redis.url(), false);
        let resources = provider
            .create_indexes("computation-test", "query")
            .await
            .expect("construct graph-scoped resources");
        assert!(matches!(
            resources.atomic_result_transaction(),
            Err(ComputationQueryError::AtomicOutputUnsupported)
        ));
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin rollback session");
            checkpoint
                .stage_checkpoint("source", 7, None)
                .await
                .expect("stage source checkpoint");
            checkpoint
                .write_result_sequence("query", 7)
                .await
                .expect("stage result sequence");
            assert_eq!(
                checkpoint
                    .read_result_sequence("query")
                    .await
                    .expect("staged sequence"),
                Some(7)
            );
        }
        assert_eq!(
            checkpoint
                .read_result_sequence("query")
                .await
                .expect("rolled back sequence"),
            None
        );
        assert!(checkpoint
            .read_checkpoint("source")
            .await
            .expect("rolled back checkpoint")
            .is_none());
        {
            let session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin committed session");
            checkpoint
                .write_result_sequence("query", 8)
                .await
                .expect("stage result sequence");
            session.commit().await.expect("commit sequence");
        }
        assert_eq!(
            checkpoint
                .read_result_sequence("query")
                .await
                .expect("committed sequence"),
            Some(8)
        );
        let legacy = GarnetIndexProvider::new(redis.url(), None, false)
            .create_indexes("query")
            .await
            .expect("normal legacy resources");
        assert_eq!(
            legacy
                .checkpoint_store
                .expect("legacy checkpoint")
                .read_result_sequence("query")
                .await
                .expect("unmodified legacy partition"),
            None
        );
    }
    redis.cleanup().await;
}
