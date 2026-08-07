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

//! Temporal reads on an index opened with enable_archive = false must return
//! IndexError::ArchiveNotEnabled instead of panicking (issue #699).

#![allow(clippy::unwrap_used)]

use drasi_core::interface::{ElementArchiveIndex, IndexBackendPlugin, IndexError};
use drasi_core::models::{ElementReference, TimestampBound, TimestampRange};
use drasi_index_rocksdb::RocksDbIndexProvider;

#[tokio::test]
async fn get_element_as_at_archive_disabled() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);
    let created = provider.create_indexes("archive-disabled").await.unwrap();

    let result = created
        .set
        .archive_index
        .get_element_as_at(&ElementReference::new("source1", "node1"), 1000)
        .await;
    assert!(matches!(result, Err(IndexError::ArchiveNotEnabled)));
}

#[tokio::test]
async fn get_element_versions_archive_disabled() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);
    let created = provider.create_indexes("archive-disabled").await.unwrap();

    for from in [
        TimestampBound::Included(0),
        TimestampBound::StartFromPrevious(0),
    ] {
        let result = created
            .set
            .archive_index
            .get_element_versions(
                &ElementReference::new("source1", "node1"),
                TimestampRange { from, to: 2000 },
            )
            .await;
        assert!(matches!(result, Err(IndexError::ArchiveNotEnabled)));
    }
}

#[tokio::test]
async fn clear_archive_disabled_is_ok() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);
    let created = provider.create_indexes("archive-disabled").await.unwrap();

    let result = created.set.archive_index.clone().clear().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn archive_reads_with_archive_enabled() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
    let created = provider.create_indexes("archive-enabled").await.unwrap();

    // Reads require an active session.
    created.set.session_control.begin().await.unwrap();

    let as_at = created
        .set
        .archive_index
        .get_element_as_at(&ElementReference::new("source1", "node1"), 1000)
        .await;
    assert!(matches!(as_at, Ok(None)));

    let versions = created
        .set
        .archive_index
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
