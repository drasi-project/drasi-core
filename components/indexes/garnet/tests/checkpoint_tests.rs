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

//! Integration tests for [`GarnetCheckpointWriter`].
//!
//! Exercises the writer through its public trait interface, paired with a
//! `SessionControl` from the same `CreatedIndexes`, against a Redis instance
//! brought up via `shared_tests::redis_helpers::setup_redis`. Each test uses
//! a unique query id so they can run in parallel without interfering.

use std::sync::Arc;

use drasi_core::interface::{CheckpointWriter, IndexBackendPlugin, SessionControl};
use drasi_index_garnet::GarnetIndexProvider;
use shared_tests::redis_helpers::{setup_redis, RedisGuard};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Shared Redis container for all tests in this file. Each test uses a unique
/// query id to avoid interference.
static SHARED_REDIS: OnceCell<RedisGuard> = OnceCell::const_new();

async fn shared_redis() -> &'static RedisGuard {
    SHARED_REDIS
        .get_or_init(|| async { setup_redis().await })
        .await
}

struct CheckpointFixture {
    session_control: Arc<dyn SessionControl>,
    checkpoint_writer: Arc<dyn CheckpointWriter>,
}

async fn fixture() -> CheckpointFixture {
    let redis = shared_redis().await;
    let provider = GarnetIndexProvider::new(redis.url(), None, false);
    let query_id = format!("ckpt-{}", Uuid::new_v4());
    let created = provider
        .create_indexes(&query_id)
        .await
        .expect("create_indexes failed");
    CheckpointFixture {
        session_control: created.set.session_control,
        checkpoint_writer: created
            .checkpoint_writer
            .expect("garnet should produce a checkpoint writer"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_then_read_after_commit() {
    let fx = fixture().await;

    fx.session_control.begin().await.unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-a", 42)
        .await
        .unwrap();
    fx.session_control.commit().await.unwrap();

    assert_eq!(
        fx.checkpoint_writer
            .read_checkpoint("source-a")
            .await
            .unwrap(),
        Some(42)
    );
    assert!(fx
        .checkpoint_writer
        .read_checkpoint("source-b")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_discards_staged_checkpoint() {
    let fx = fixture().await;

    fx.session_control.begin().await.unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-a", 100)
        .await
        .unwrap();
    fx.session_control.rollback().unwrap();

    assert!(fx
        .checkpoint_writer
        .read_checkpoint("source-a")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_without_session_errors() {
    let fx = fixture().await;
    let result = fx.checkpoint_writer.stage_checkpoint("source-a", 1).await;
    assert!(
        result.is_err(),
        "stage_checkpoint without session should error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_all_returns_every_source() {
    let fx = fixture().await;

    fx.session_control.begin().await.unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-a", 10)
        .await
        .unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-b", 20)
        .await
        .unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-c", 30)
        .await
        .unwrap();
    fx.session_control.commit().await.unwrap();

    let all = fx.checkpoint_writer.read_all_checkpoints().await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all.get("source-a"), Some(&10));
    assert_eq!(all.get("source-b"), Some(&20));
    assert_eq!(all.get("source-c"), Some(&30));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_hash_round_trip() {
    let fx = fixture().await;

    assert!(fx
        .checkpoint_writer
        .read_config_hash()
        .await
        .unwrap()
        .is_none());
    fx.checkpoint_writer
        .write_config_hash(0xDEAD_BEEF)
        .await
        .unwrap();
    assert_eq!(
        fx.checkpoint_writer.read_config_hash().await.unwrap(),
        Some(0xDEAD_BEEF)
    );

    fx.checkpoint_writer
        .write_config_hash(0x1234_5678)
        .await
        .unwrap();
    assert_eq!(
        fx.checkpoint_writer.read_config_hash().await.unwrap(),
        Some(0x1234_5678)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_checkpoints_wipes_everything() {
    let fx = fixture().await;

    fx.session_control.begin().await.unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-a", 1)
        .await
        .unwrap();
    fx.checkpoint_writer
        .stage_checkpoint("source-b", 2)
        .await
        .unwrap();
    fx.session_control.commit().await.unwrap();
    fx.checkpoint_writer.write_config_hash(99).await.unwrap();

    fx.checkpoint_writer.clear_checkpoints().await.unwrap();

    assert!(fx
        .checkpoint_writer
        .read_checkpoint("source-a")
        .await
        .unwrap()
        .is_none());
    assert!(fx
        .checkpoint_writer
        .read_checkpoint("source-b")
        .await
        .unwrap()
        .is_none());
    assert!(fx
        .checkpoint_writer
        .read_all_checkpoints()
        .await
        .unwrap()
        .is_empty());
    assert!(fx
        .checkpoint_writer
        .read_config_hash()
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_session_commits_atomically() {
    let fx = fixture().await;

    // Outer begin (lib's wrap), inner begin (core's process_source_change)
    fx.session_control.begin().await.unwrap();
    fx.session_control.begin().await.unwrap();
    // Core would do its index writes here; we simulate just the checkpoint stage.
    fx.session_control.commit().await.unwrap(); // inner no-op
    fx.checkpoint_writer
        .stage_checkpoint("source-a", 555)
        .await
        .unwrap();
    fx.session_control.commit().await.unwrap(); // outer real commit

    assert_eq!(
        fx.checkpoint_writer
            .read_checkpoint("source-a")
            .await
            .unwrap(),
        Some(555)
    );
}
