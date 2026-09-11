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

//! Temporal reads on an index created with enable_archive = false must return
//! IndexError::ArchiveNotEnabled instead of silently returning nothing (issue #699).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use drasi_core::interface::{ElementArchiveIndex, IndexError, SessionControl};
use drasi_core::models::{ElementReference, TimestampBound, TimestampRange};
use drasi_index_garnet::{
    element_index::GarnetElementIndex, GarnetSessionControl, GarnetSessionState,
};
use shared_tests::redis_helpers::{setup_redis, RedisGuard};
use tokio::sync::OnceCell;

static SHARED_REDIS: OnceCell<RedisGuard> = OnceCell::const_new();

async fn shared_redis() -> &'static RedisGuard {
    SHARED_REDIS
        .get_or_init(|| async { setup_redis().await })
        .await
}

async fn build_index(query_id: &str, archive_enabled: bool) -> GarnetElementIndex {
    let redis = shared_redis().await;
    let client = redis::Client::open(redis.url()).unwrap();
    let connection = client.get_multiplexed_async_connection().await.unwrap();
    let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
    GarnetElementIndex::new(query_id, connection, archive_enabled, session_state)
}

#[tokio::test]
async fn get_element_as_at_archive_disabled() {
    let index = build_index("archive-disabled-as-at", false).await;
    let result = index
        .get_element_as_at(&ElementReference::new("source1", "node1"), 1000)
        .await;
    assert!(matches!(result, Err(IndexError::ArchiveNotEnabled)));
}

#[tokio::test]
async fn get_element_versions_archive_disabled() {
    let index = build_index("archive-disabled-versions", false).await;
    for from in [
        TimestampBound::Included(0),
        TimestampBound::StartFromPrevious(0),
    ] {
        let result = index
            .get_element_versions(
                &ElementReference::new("source1", "node1"),
                TimestampRange { from, to: 2000 },
            )
            .await;
        assert!(matches!(result, Err(IndexError::ArchiveNotEnabled)));
    }
}

#[tokio::test]
async fn archive_reads_with_archive_enabled() {
    let redis = shared_redis().await;
    let client = redis::Client::open(redis.url()).unwrap();
    let connection = client.get_multiplexed_async_connection().await.unwrap();
    let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
    let session_control = GarnetSessionControl::new(session_state.clone());
    let index = GarnetElementIndex::new("archive-enabled", connection, true, session_state);

    // Reads require an active session.
    session_control.begin().await.unwrap();

    let as_at = index
        .get_element_as_at(&ElementReference::new("source1", "node1"), 1000)
        .await;
    assert!(matches!(as_at, Ok(None)));

    let versions = index
        .get_element_versions(
            &ElementReference::new("source1", "node1"),
            TimestampRange {
                from: TimestampBound::Included(0),
                to: 2000,
            },
        )
        .await;
    assert!(versions.is_ok());
}
