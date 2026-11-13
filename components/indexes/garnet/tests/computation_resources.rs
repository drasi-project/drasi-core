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
    interface::{
        CheckpointKey, IndexBackendPlugin, OutboxWriter, OutputPartition, RowMutation,
        SessionGuard, StorageScope,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
    ComponentId,
};
use drasi_index_garnet::{
    computation::GarnetComputationProvider, GarnetIndexProvider, GarnetOutboxWriter,
};
use shared_tests::redis_helpers::setup_redis;

fn query_id() -> ComponentId {
    ComponentId::try_new("70a753ff-f396-5582-91b3-1c2f5948550c").expect("query fixture UUID")
}

fn source_checkpoint() -> CheckpointKey {
    CheckpointKey::Source(
        ComponentId::try_new("00000000-0000-0000-0000-00000000004d").expect("source fixture UUID"),
    )
}

async fn stage_output(resources: &ComputationIndexes, query_id: &ComponentId, sequence: u64) {
    let output = OutputPartition::Component(query_id.clone());
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
        .stage_checkpoint(
            &source_checkpoint(),
            sequence,
            Some(&Bytes::from_static(b"position")),
        )
        .await
        .expect("stage source checkpoint");
    checkpoint
        .stage_result_sequence(&output, sequence)
        .await
        .expect("stage result sequence");
    resources
        .outbox_writer()
        .expect("outbox")
        .append_and_trim(&output, sequence, b"output", sequence)
        .await
        .expect("stage output and retain the new entry");
    resources
        .live_results_writer()
        .expect("live results")
        .apply_mutations(
            query_id,
            &[RowMutation {
                row_signature: sequence,
                data: Some(b"live"),
            }],
        )
        .await
        .expect("stage live row");
}

async fn assert_committed_output(
    resources: &ComputationIndexes,
    output: &OutputPartition,
    sequence: Option<u64>,
) {
    assert_eq!(
        resources
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(output)
            .await
            .expect("committed result head"),
        sequence
    );
    let outbox = resources.outbox_writer().expect("outbox");
    assert_eq!(
        outbox.read_latest_sequence(output).await.expect("head"),
        sequence
    );
    assert_eq!(
        outbox.read_from(output, 0).await.expect("committed output"),
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
    let query_id = query_id();
    let output = OutputPartition::Component(query_id.clone());
    {
        let resources = provider
            .create_indexes("computation-test", &query_id)
            .await
            .expect("construct graph-scoped resources");
        resources
            .atomic_result_transaction()
            .expect("complete atomic output capability");
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        let outbox = resources.outbox_writer().expect("outbox");
        let live = resources.live_results_writer().expect("live results");
        assert!(checkpoint.stage_result_sequence(&output, 7).await.is_err());
        assert!(outbox.append(&output, 7, b"no-session").await.is_err());
        assert!(live.apply_mutations(&query_id, &[]).await.is_err());
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin rollback session");
            stage_output(&resources, &query_id, 7).await;
            assert_committed_output(&resources, &output, None).await;
            assert!(live
                .read_snapshot(&query_id)
                .await
                .expect("committed rows")
                .is_empty());
        }
        assert_committed_output(&resources, &output, None).await;
        assert!(live
            .read_snapshot(&query_id)
            .await
            .expect("rolled back rows")
            .is_empty());
        assert!(checkpoint
            .read_checkpoint(&source_checkpoint())
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
            stage_output(&resources, &query_id, 8).await;
            assert_committed_output(&resources, &output, None).await;
            session.commit().await.expect("commit sequence");
        }
        assert_committed_output(&resources, &output, Some(8)).await;
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin failed replacement");
            stage_output(&resources, &query_id, 9).await;
            assert_committed_output(&resources, &output, Some(8)).await;
        }
        assert_committed_output(&resources, &output, Some(8)).await;
        assert_eq!(
            live.read_snapshot(&query_id)
                .await
                .expect("rolled back replacement"),
            [(8, b"live".to_vec())]
        );
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin failed clear");
            outbox.clear(&output).await.expect("stage outbox clear");
            live.clear(&query_id).await.expect("stage live clear");
        }
        assert_committed_output(&resources, &output, Some(8)).await;
        assert_eq!(
            live.row_count(&query_id).await.expect("rolled back clear"),
            1
        );
    }
    let reopened = provider
        .create_indexes("computation-test", &query_id)
        .await
        .expect("reopen graph resources");
    assert_committed_output(&reopened, &output, Some(8)).await;
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_checkpoint(&source_checkpoint())
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
            .read_snapshot(&query_id)
            .await
            .expect("reopened rows"),
        [(8, b"live".to_vec())]
    );
    let legacy = GarnetIndexProvider::new(redis.url(), None, false)
        .create_indexes(&query_id)
        .await
        .expect("normal legacy resources");
    assert_eq!(
        legacy
            .checkpoint_store
            .expect("legacy checkpoint")
            .read_result_sequence(&output)
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
        .clear(&output)
        .await
        .expect("clear outbox");
    reopened
        .live_results_writer()
        .expect("live")
        .clear(&query_id)
        .await
        .expect("clear live rows");
    session.commit().await.expect("commit clear");
    assert!(reopened
        .outbox_writer()
        .expect("outbox")
        .read_from(&output, 0)
        .await
        .expect("cleared output")
        .is_empty());
    assert_eq!(
        reopened
            .live_results_writer()
            .expect("live")
            .row_count(&query_id)
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
    let query_id = query_id();
    let output = OutputPartition::Component(query_id.clone());
    let transient = CheckpointKey::Source(
        ComponentId::try_new("00000000-0000-0000-0000-00000000004e")
            .expect("transient fixture UUID"),
    );
    {
        let resources = provider
            .create_indexes("generation", &query_id)
            .await
            .expect("resources");
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        assert_eq!(
            checkpoint
                .read_output_generation(&output)
                .await
                .expect("new lifetime"),
            None
        );
        checkpoint
            .write_result_sequence(&output, 41)
            .await
            .expect("standalone head");
        checkpoint
            .write_output_generation(&output, u64::MAX)
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
                .stage_checkpoint(&source_checkpoint(), 3, Some(&Bytes::from_static(b"pos")))
                .await
                .expect("source checkpoint");
            session.commit().await.expect("commit source");
        }
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin discarded head");
            checkpoint
                .stage_result_sequence(&output, 42)
                .await
                .expect("stage head");
            assert_eq!(
                checkpoint
                    .read_result_sequence(&output)
                    .await
                    .expect("committed head"),
                Some(41)
            );
        }
        assert!(checkpoint.stage_clear_checkpoints().await.is_err());
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin discarded checkpoint clear");
            checkpoint
                .stage_clear_checkpoints()
                .await
                .expect("stage checkpoint clear");
            assert!(checkpoint
                .read_all_checkpoints()
                .await
                .expect("staged clear")
                .is_empty());
            checkpoint
                .stage_checkpoint(&transient, 4, None)
                .await
                .expect("stage after clear");
            assert!(checkpoint
                .read_checkpoint(&source_checkpoint())
                .await
                .expect("staged source deletion")
                .is_none());
        }
        assert_eq!(
            checkpoint
                .read_checkpoint(&source_checkpoint())
                .await
                .expect("rolled-back clear")
                .expect("restored source")
                .sequence,
            3,
        );
        assert!(checkpoint
            .read_checkpoint(&transient)
            .await
            .expect("rolled-back insert")
            .is_none());
        assert_eq!(
            checkpoint.read_config_hash().await.expect("retained hash"),
            Some(123)
        );
        {
            let session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin committed checkpoint clear");
            checkpoint
                .stage_clear_checkpoints()
                .await
                .expect("stage checkpoint clear");
            session.commit().await.expect("commit checkpoint clear");
        }
        assert!(checkpoint
            .read_all_checkpoints()
            .await
            .expect("committed clear")
            .is_empty());
        assert_eq!(
            checkpoint.read_config_hash().await.expect("cleared hash"),
            None
        );
        assert_eq!(
            checkpoint
                .read_result_sequence(&output)
                .await
                .expect("retained head"),
            Some(41)
        );
        assert_eq!(
            checkpoint
                .read_output_generation(&output)
                .await
                .expect("retained generation"),
            Some(u64::MAX)
        );
        checkpoint
            .write_config_hash(123)
            .await
            .expect("restore hash for immediate clear");
        {
            let session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("restore source for immediate clear");
            checkpoint
                .stage_checkpoint(&source_checkpoint(), 3, Some(&Bytes::from_static(b"pos")))
                .await
                .expect("restore source");
            session.commit().await.expect("commit restored source");
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
                .read_result_sequence(&output)
                .await
                .expect("retained head"),
            Some(41)
        );
        assert_eq!(
            checkpoint
                .read_output_generation(&output)
                .await
                .expect("retained lifetime"),
            Some(u64::MAX)
        );
    }
    let reopened = provider
        .create_indexes("generation", &query_id)
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
            .read_result_sequence(&output)
            .await
            .expect("head after reopen"),
        Some(41)
    );
    assert_eq!(
        checkpoint
            .read_output_generation(&output)
            .await
            .expect("generation after reopen"),
        Some(u64::MAX)
    );
    let other = provider
        .create_indexes("other-generation", &query_id)
        .await
        .expect("other graph");
    assert_eq!(
        other
            .checkpoint_store()
            .expect("checkpoint")
            .read_output_generation(&output)
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
    let query_id = query_id();
    let output = OutputPartition::Component(query_id.clone());
    let metadata = output.metadata("recovery:{query}");
    let other_metadata = output.metadata("other-metadata");
    let marker = br#"{"sequence":7,"in_progress":false,"generation":3}"#;
    {
        let resources = provider
            .create_indexes("namespaces", &query_id)
            .await
            .expect("resources");
        let outbox = resources.outbox_writer().expect("outbox");
        let session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin");
        outbox
            .append(&output, 7, b"envelope")
            .await
            .expect("primary output");
        outbox
            .append(&metadata, 1, marker)
            .await
            .expect("metadata output");
        outbox
            .append(&other_metadata, 1, b"other")
            .await
            .expect("other metadata");
        session.commit().await.expect("commit namespaces");
        assert_eq!(
            outbox.read_from(&metadata, 0).await.expect("metadata only"),
            [(1, marker.to_vec())]
        );
        assert_eq!(
            outbox.read_from(&output, 0).await.expect("primary only"),
            [(7, b"envelope".to_vec())]
        );
        let session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin trim");
        assert_eq!(
            outbox
                .append_and_trim(&output, 8, b"new-envelope", 8)
                .await
                .expect("trim primary"),
            1
        );
        session.commit().await.expect("commit trim");
        assert_eq!(
            outbox
                .read_latest_sequence(&metadata)
                .await
                .expect("metadata head"),
            Some(1)
        );
    }
    let reopened = provider
        .create_indexes("namespaces", &query_id)
        .await
        .expect("reopen resources");
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        outbox
            .read_from(&output, 0)
            .await
            .expect("reopened primary"),
        [(8, b"new-envelope".to_vec())]
    );
    assert_eq!(
        outbox
            .read_from(&metadata, 0)
            .await
            .expect("reopened marker"),
        [(1, marker.to_vec())]
    );

    let storage_scope = StorageScope::new("computation").nested(&StorageScope::new("namespaces"));
    let mut connection = redis::Client::open(redis.url())
        .expect("client")
        .get_multiplexed_async_connection()
        .await
        .expect("connection");
    let legacy_reader = GarnetOutboxWriter::new(&storage_scope, &query_id, connection.clone());
    assert_eq!(
        legacy_reader
            .read_from(&output, 0)
            .await
            .expect("unchanged primary storage key"),
        [(8, b"new-envelope".to_vec())]
    );
    let storage_key = storage_scope.component_key(&query_id);
    let sequences: Vec<String> = redis::cmd("ZRANGE")
        .arg(format!("outbox:{{{storage_key}}}"))
        .arg(0)
        .arg(-1)
        .query_async(&mut connection)
        .await
        .expect("physical primary outbox sequences");
    assert_eq!(sequences, ["8"]);
    let data: Vec<u8> = redis::cmd("HGET")
        .arg(format!("outbox_data:{{{storage_key}}}"))
        .arg("8")
        .query_async(&mut connection)
        .await
        .expect("physical primary outbox data");
    assert_eq!(data, b"new-envelope");
    {
        let _session = SessionGuard::begin(reopened.indexes().session_control.clone())
            .await
            .expect("begin rolled back metadata clear");
        outbox.clear(&metadata).await.expect("stage metadata clear");
    }
    assert_eq!(
        outbox
            .read_from(&metadata, 0)
            .await
            .expect("metadata after rollback"),
        [(1, marker.to_vec())]
    );
    let session = SessionGuard::begin(reopened.indexes().session_control.clone())
        .await
        .expect("begin metadata clear");
    outbox.clear(&metadata).await.expect("clear metadata");
    session.commit().await.expect("commit metadata clear");
    assert!(outbox
        .read_from(&metadata, 0)
        .await
        .expect("cleared metadata")
        .is_empty());
    assert_eq!(
        outbox
            .read_from(&output, 0)
            .await
            .expect("primary survives"),
        [(8, b"new-envelope".to_vec())]
    );
    assert_eq!(
        outbox
            .read_from(&other_metadata, 0)
            .await
            .expect("other metadata survives"),
        [(1, b"other".to_vec())]
    );
    let other = provider
        .create_indexes("other-namespaces", &query_id)
        .await
        .expect("other graph");
    assert!(other
        .outbox_writer()
        .expect("outbox")
        .read_from(&metadata, 0)
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
    let query_id = query_id();
    let output = OutputPartition::Component(query_id.clone());
    let resources = provider
        .create_indexes("sequence-boundaries", &query_id)
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
            .append(&output, number, b"boundary")
            .await
            .expect("stage boundary");
    }
    assert_eq!(
        outbox.trim_before(&output, 0).await.expect("retain zero"),
        0
    );
    assert_eq!(
        outbox
            .read_latest_sequence(&output)
            .await
            .expect("committed head"),
        None
    );
    session.commit().await.expect("commit boundaries");
    assert_eq!(
        outbox
            .read_latest_sequence(&output)
            .await
            .expect("largest sequence"),
        Some(u64::MAX)
    );
    for after in numbers {
        assert_eq!(
            outbox
                .read_from(&output, after)
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
                .trim_before(&output, u64::MAX)
                .await
                .expect("trim before maximum"),
            numbers.len() - 1
        );
    }
    assert_eq!(
        outbox
            .read_from(&output, 0)
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
            .append_and_trim(&output, u64::MAX, b"boundary", u64::MAX)
            .await
            .expect("retain highest"),
        numbers.len() - 1
    );
    resources
        .checkpoint_store()
        .expect("checkpoint")
        .stage_result_sequence(&output, u64::MAX)
        .await
        .expect("stage maximum head");
    session.commit().await.expect("commit trim");
    assert_eq!(
        outbox
            .read_from(&output, u64::MAX - 1)
            .await
            .expect("read maximum"),
        [(u64::MAX, b"boundary".to_vec())]
    );
    assert!(outbox
        .read_from(&output, u64::MAX)
        .await
        .expect("past maximum")
        .is_empty());
    drop(resources);
    let reopened = provider
        .create_indexes("sequence-boundaries", &query_id)
        .await
        .expect("reopen maximum sequence");
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(&output)
            .await
            .expect("reopened maximum head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_latest_sequence(&output)
            .await
            .expect("reopened outbox head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_from(&output, 0)
            .await
            .expect("persisted retention"),
        [(u64::MAX, b"boundary".to_vec())]
    );
    assert!(outbox
        .read_from(&output, u64::MAX)
        .await
        .expect("reopened past maximum")
        .is_empty());
    redis.cleanup().await;
}
