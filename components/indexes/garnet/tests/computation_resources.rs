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

use std::sync::Arc;

use bytes::Bytes;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes},
    interface::{IndexBackendPlugin, OutboxWriter, RowMutation, SessionGuard},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
};
use drasi_index_garnet::{
    computation::GarnetComputationProvider, GarnetIndexProvider, GarnetOutboxWriter,
};
use shared_tests::redis_helpers::setup_redis;

const QUERY: &str = "query";

async fn stage_output(resources: &ComputationIndexes, sequence: u64) {
    resources
        .indexes()
        .element_index
        .set_element(
            &Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "node"),
                    labels: Arc::from([Arc::from("Node")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
            &vec![0],
        )
        .await
        .expect("stage element");
    let checkpoint = resources.checkpoint_store().expect("checkpoint");
    checkpoint
        .stage_checkpoint("source", sequence, Some(&Bytes::from_static(b"position")))
        .await
        .expect("stage source checkpoint");
    checkpoint
        .stage_result_sequence(QUERY, sequence)
        .await
        .expect("stage result sequence");
    resources
        .outbox_writer()
        .expect("outbox")
        .append_and_trim(QUERY, sequence, b"output", sequence)
        .await
        .expect("stage output and retain the new entry");
    resources
        .live_results_writer()
        .expect("live results")
        .apply_mutations(
            QUERY,
            &[RowMutation {
                row_signature: sequence,
                data: Some(b"live"),
            }],
        )
        .await
        .expect("stage live row");
}

async fn assert_committed_output(resources: &ComputationIndexes, sequence: Option<u64>) {
    assert_eq!(
        resources
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("committed result head"),
        sequence
    );
    let outbox = resources.outbox_writer().expect("outbox");
    assert_eq!(
        outbox.read_latest_sequence(QUERY).await.expect("head"),
        sequence
    );
    assert_eq!(
        outbox.read_from(QUERY, 0).await.expect("committed output"),
        sequence
            .map(|seq| vec![(seq, b"output".to_vec())])
            .unwrap_or_default()
    );
}

#[tokio::test]
#[ignore = "requires Docker through the existing Redis fixture"]
async fn graph_indexes_checkpoint_and_output_commit_and_roll_back_together() {
    let redis = setup_redis().await;
    let provider = GarnetComputationProvider::new(redis.url(), false);
    {
        let resources = provider
            .create_indexes("computation-test", QUERY)
            .await
            .expect("construct graph-scoped resources");
        resources
            .atomic_result_transaction()
            .expect("complete atomic output capability");
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        let outbox = resources.outbox_writer().expect("outbox");
        let live = resources.live_results_writer().expect("live results");
        assert!(checkpoint.stage_result_sequence(QUERY, 7).await.is_err());
        assert!(outbox.append(QUERY, 7, b"no-session").await.is_err());
        assert!(live.apply_mutations(QUERY, &[]).await.is_err());
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin rollback session");
            stage_output(&resources, 7).await;
            assert_committed_output(&resources, None).await;
            assert!(live
                .read_snapshot(QUERY)
                .await
                .expect("committed rows")
                .is_empty());
        }
        assert_committed_output(&resources, None).await;
        assert!(live
            .read_snapshot(QUERY)
            .await
            .expect("rolled back rows")
            .is_empty());
        assert!(checkpoint
            .read_checkpoint("source")
            .await
            .expect("rolled back checkpoint")
            .is_none());
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin verification read");
            assert!(resources
                .indexes()
                .element_index
                .get_element(&ElementReference::new("source", "node"))
                .await
                .expect("rolled back element")
                .is_none());
        }
        {
            let session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin committed session");
            stage_output(&resources, 8).await;
            assert_committed_output(&resources, None).await;
            session.commit().await.expect("commit sequence");
        }
        assert_committed_output(&resources, Some(8)).await;
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin failed replacement");
            stage_output(&resources, 9).await;
            assert_committed_output(&resources, Some(8)).await;
        }
        assert_committed_output(&resources, Some(8)).await;
        assert_eq!(
            live.read_snapshot(QUERY)
                .await
                .expect("rolled back replacement"),
            [(8, b"live".to_vec())]
        );
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin failed clear");
            outbox.clear(QUERY).await.expect("stage outbox clear");
            live.clear(QUERY).await.expect("stage live clear");
        }
        assert_committed_output(&resources, Some(8)).await;
        assert_eq!(live.row_count(QUERY).await.expect("rolled back clear"), 1);
    }
    let reopened = provider
        .create_indexes("computation-test", QUERY)
        .await
        .expect("reopen graph resources");
    assert_committed_output(&reopened, Some(8)).await;
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_checkpoint("source")
            .await
            .expect("reopened source checkpoint")
            .expect("checkpoint exists")
            .sequence,
        8
    );
    {
        let _session = SessionGuard::begin(reopened.indexes().session_control.clone())
            .await
            .expect("begin verification read");
        assert!(reopened
            .indexes()
            .element_index
            .get_element(&ElementReference::new("source", "node"))
            .await
            .expect("committed element")
            .is_some());
    }
    assert_eq!(
        reopened
            .live_results_writer()
            .expect("live results")
            .read_snapshot(QUERY)
            .await
            .expect("reopened rows"),
        [(8, b"live".to_vec())]
    );
    let legacy = GarnetIndexProvider::new(redis.url(), None, false)
        .create_indexes(QUERY)
        .await
        .expect("normal legacy resources");
    assert_eq!(
        legacy
            .checkpoint_store
            .expect("legacy checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("unmodified legacy partition"),
        None
    );
    let session = SessionGuard::begin(reopened.indexes().session_control.clone())
        .await
        .expect("begin committed clear");
    reopened
        .outbox_writer()
        .expect("outbox")
        .clear(QUERY)
        .await
        .expect("clear outbox");
    reopened
        .live_results_writer()
        .expect("live")
        .clear(QUERY)
        .await
        .expect("clear live rows");
    session.commit().await.expect("commit clear");
    assert!(reopened
        .outbox_writer()
        .expect("outbox")
        .read_from(QUERY, 0)
        .await
        .expect("cleared output")
        .is_empty());
    assert_eq!(
        reopened
            .live_results_writer()
            .expect("live")
            .row_count(QUERY)
            .await
            .expect("cleared live rows"),
        0
    );
    redis.cleanup().await;
}

#[tokio::test]
#[ignore = "requires Docker through the existing Redis fixture"]
async fn output_generation_and_committed_head_survive_checkpoint_clear_and_reopen() {
    let redis = setup_redis().await;
    let provider = GarnetComputationProvider::new(redis.url(), false);
    {
        let resources = provider
            .create_indexes("generation", QUERY)
            .await
            .expect("resources");
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        assert_eq!(
            checkpoint
                .read_output_generation(QUERY)
                .await
                .expect("new lifetime"),
            None
        );
        checkpoint
            .write_result_sequence(QUERY, 41)
            .await
            .expect("standalone head");
        checkpoint
            .write_output_generation(QUERY, u64::MAX)
            .await
            .expect("generation");
        checkpoint
            .write_config_hash(123)
            .await
            .expect("config hash");
        {
            let session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin checkpoint session");
            checkpoint
                .stage_checkpoint("source", 3, Some(&Bytes::from_static(b"pos")))
                .await
                .expect("source checkpoint");
            session.commit().await.expect("commit source");
        }
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin discarded head");
            checkpoint
                .stage_result_sequence(QUERY, 42)
                .await
                .expect("stage head");
            assert_eq!(
                checkpoint
                    .read_result_sequence(QUERY)
                    .await
                    .expect("committed head"),
                Some(41)
            );
        }
        checkpoint
            .clear_checkpoints()
            .await
            .expect("clear bootstrap checkpoint");
        assert!(checkpoint
            .read_all_checkpoints()
            .await
            .expect("cleared sources")
            .is_empty());
        assert_eq!(
            checkpoint.read_config_hash().await.expect("cleared hash"),
            None
        );
        assert_eq!(
            checkpoint
                .read_result_sequence(QUERY)
                .await
                .expect("retained head"),
            Some(41)
        );
        assert_eq!(
            checkpoint
                .read_output_generation(QUERY)
                .await
                .expect("retained lifetime"),
            Some(u64::MAX)
        );
    }
    let reopened = provider
        .create_indexes("generation", QUERY)
        .await
        .expect("reopen resources");
    let checkpoint = reopened.checkpoint_store().expect("checkpoint");
    assert!(checkpoint
        .read_all_checkpoints()
        .await
        .expect("sources after reopen")
        .is_empty());
    assert_eq!(
        checkpoint
            .read_config_hash()
            .await
            .expect("hash after reopen"),
        None
    );
    assert_eq!(
        checkpoint
            .read_result_sequence(QUERY)
            .await
            .expect("head after reopen"),
        Some(41)
    );
    assert_eq!(
        checkpoint
            .read_output_generation(QUERY)
            .await
            .expect("generation after reopen"),
        Some(u64::MAX)
    );
    let other = provider
        .create_indexes("other-generation", QUERY)
        .await
        .expect("other graph");
    assert_eq!(
        other
            .checkpoint_store()
            .expect("checkpoint")
            .read_output_generation(QUERY)
            .await
            .expect("isolated lifetime"),
        None
    );
    redis.cleanup().await;
}

#[tokio::test]
#[ignore = "requires Docker through the existing Redis fixture"]
async fn native_output_namespaces_keep_primary_keys_and_isolate_recovery_metadata() {
    let redis = setup_redis().await;
    let provider = GarnetComputationProvider::new(redis.url(), false);
    let metadata_id = "recovery:{query}";
    let marker = br#"{"sequence":7,"in_progress":false,"generation":3}"#;
    {
        let resources = provider
            .create_indexes("namespaces", QUERY)
            .await
            .expect("resources");
        let outbox = resources.outbox_writer().expect("outbox");
        let session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin");
        outbox
            .append(QUERY, 7, b"envelope")
            .await
            .expect("primary output");
        outbox
            .append(metadata_id, 1, marker)
            .await
            .expect("metadata output");
        outbox
            .append("other-metadata", 1, b"other")
            .await
            .expect("other metadata");
        session.commit().await.expect("commit namespaces");
        assert_eq!(
            outbox
                .read_from(metadata_id, 0)
                .await
                .expect("metadata only"),
            [(1, marker.to_vec())]
        );
        assert_eq!(
            outbox.read_from(QUERY, 0).await.expect("primary only"),
            [(7, b"envelope".to_vec())]
        );
        let session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin trim");
        assert_eq!(
            outbox
                .append_and_trim(QUERY, 8, b"new-envelope", 8)
                .await
                .expect("trim primary"),
            1
        );
        session.commit().await.expect("commit trim");
        assert_eq!(
            outbox
                .read_latest_sequence(metadata_id)
                .await
                .expect("metadata head"),
            Some(1)
        );
    }
    let reopened = provider
        .create_indexes("namespaces", QUERY)
        .await
        .expect("reopen resources");
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        outbox.read_from(QUERY, 0).await.expect("reopened primary"),
        [(8, b"new-envelope".to_vec())]
    );
    assert_eq!(
        outbox
            .read_from(metadata_id, 0)
            .await
            .expect("reopened marker"),
        [(1, marker.to_vec())]
    );

    let encode = |id: &str| -> String { id.bytes().map(|byte| format!("{byte:02x}")).collect() };
    let storage_scope = format!("computation-v1:{}:{}", encode("namespaces"), encode(QUERY));
    let connection = redis::Client::open(redis.url())
        .expect("client")
        .get_multiplexed_async_connection()
        .await
        .expect("connection");
    let legacy_reader = GarnetOutboxWriter::new(&storage_scope, connection);
    assert_eq!(
        legacy_reader
            .read_from(&storage_scope, 0)
            .await
            .expect("unchanged primary storage key"),
        [(8, b"new-envelope".to_vec())]
    );
    {
        let _session = SessionGuard::begin(reopened.indexes().session_control.clone())
            .await
            .expect("begin rolled back metadata clear");
        outbox
            .clear(metadata_id)
            .await
            .expect("stage metadata clear");
    }
    assert_eq!(
        outbox
            .read_from(metadata_id, 0)
            .await
            .expect("metadata after rollback"),
        [(1, marker.to_vec())]
    );
    let session = SessionGuard::begin(reopened.indexes().session_control.clone())
        .await
        .expect("begin metadata clear");
    outbox.clear(metadata_id).await.expect("clear metadata");
    session.commit().await.expect("commit metadata clear");
    assert!(outbox
        .read_from(metadata_id, 0)
        .await
        .expect("cleared metadata")
        .is_empty());
    assert_eq!(
        outbox.read_from(QUERY, 0).await.expect("primary survives"),
        [(8, b"new-envelope".to_vec())]
    );
    assert_eq!(
        outbox
            .read_from("other-metadata", 0)
            .await
            .expect("other metadata survives"),
        [(1, b"other".to_vec())]
    );
    let other = provider
        .create_indexes("other-namespaces", QUERY)
        .await
        .expect("other graph");
    assert!(other
        .outbox_writer()
        .expect("outbox")
        .read_from(metadata_id, 0)
        .await
        .expect("other graph metadata")
        .is_empty());
    redis.cleanup().await;
}

#[tokio::test]
#[ignore = "requires Docker through the existing Redis fixture"]
async fn native_retention_handles_staged_appends_and_full_width_sequences() {
    let redis = setup_redis().await;
    let provider = GarnetComputationProvider::new(redis.url(), false);
    let resources = provider
        .create_indexes("sequence-boundaries", QUERY)
        .await
        .expect("resources");
    let outbox = resources.outbox_writer().expect("outbox");
    let numbers = [
        0,
        (1 << 53) - 1,
        1 << 53,
        (1 << 53) + 1,
        9_999_999_999_999_999,
        10_000_000_000_000_000,
        u64::MAX - 1,
        u64::MAX,
    ];
    let session = SessionGuard::begin(resources.indexes().session_control.clone())
        .await
        .expect("begin boundary writes");
    for number in numbers {
        outbox
            .append(QUERY, number, b"boundary")
            .await
            .expect("stage boundary");
    }
    assert_eq!(outbox.trim_before(QUERY, 0).await.expect("retain zero"), 0);
    assert_eq!(
        outbox
            .read_latest_sequence(QUERY)
            .await
            .expect("committed head"),
        None
    );
    session.commit().await.expect("commit boundaries");
    assert_eq!(
        outbox
            .read_latest_sequence(QUERY)
            .await
            .expect("largest sequence"),
        Some(u64::MAX)
    );
    for after in numbers {
        assert_eq!(
            outbox
                .read_from(QUERY, after)
                .await
                .expect("strict sequence bound")
                .into_iter()
                .map(|(number, _)| number)
                .collect::<Vec<_>>(),
            numbers
                .into_iter()
                .filter(|number| *number > after)
                .collect::<Vec<_>>()
        );
    }
    {
        let _session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin discarded trim");
        assert_eq!(
            outbox
                .trim_before(QUERY, u64::MAX)
                .await
                .expect("trim before maximum"),
            numbers.len() - 1
        );
    }
    assert_eq!(
        outbox
            .read_from(QUERY, 0)
            .await
            .expect("rolled back trim")
            .len(),
        numbers.len() - 1
    );
    let session = SessionGuard::begin(resources.indexes().session_control.clone())
        .await
        .expect("begin retained maximum");
    assert_eq!(
        outbox
            .append_and_trim(QUERY, u64::MAX, b"boundary", u64::MAX)
            .await
            .expect("retain highest"),
        numbers.len() - 1
    );
    resources
        .checkpoint_store()
        .expect("checkpoint")
        .stage_result_sequence(QUERY, u64::MAX)
        .await
        .expect("stage maximum head");
    session.commit().await.expect("commit trim");
    assert_eq!(
        outbox
            .read_from(QUERY, u64::MAX - 1)
            .await
            .expect("read maximum"),
        [(u64::MAX, b"boundary".to_vec())]
    );
    assert!(outbox
        .read_from(QUERY, u64::MAX)
        .await
        .expect("past maximum")
        .is_empty());
    drop(resources);
    let reopened = provider
        .create_indexes("sequence-boundaries", QUERY)
        .await
        .expect("reopen maximum sequence");
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("reopened maximum head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_latest_sequence(QUERY)
            .await
            .expect("reopened outbox head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_from(QUERY, 0)
            .await
            .expect("persisted retention"),
        [(u64::MAX, b"boundary".to_vec())]
    );
    assert!(outbox
        .read_from(QUERY, u64::MAX)
        .await
        .expect("reopened past maximum")
        .is_empty());
    redis.cleanup().await;
}
