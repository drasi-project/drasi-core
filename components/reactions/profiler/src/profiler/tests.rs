// Copyright 2025 The Drasi Authors.
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

use super::*;
use drasi_lib::channels::QueryResult;
use std::time::Duration;

#[tokio::test]
async fn stop_and_restart_reopens_the_result_queue() {
    let reaction = ProfilerReaction::new(
        "profiler-restart",
        vec!["q1".to_string()],
        ProfilerReactionConfig::default(),
    );
    for sequence in [100, 1] {
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(QueryResult::new(
                "q1".to_string(),
                sequence,
                chrono::Utc::now(),
                vec![],
                Default::default(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !reaction.base.priority_queue.is_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), reaction.stop())
            .await
            .expect("profiler must use graceful shutdown, not the two-second abort timeout")
            .unwrap();
        assert!(reaction.base.processing_task.read().await.is_none());
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }
}
