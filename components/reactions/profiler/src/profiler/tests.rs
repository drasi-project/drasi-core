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
use drasi_lib::Reaction;

#[tokio::test]
async fn profiler_accepts_results_after_stop_and_restart() {
    let reaction = ProfilerReaction::new(
        "restart-profiler",
        vec!["q1".to_string()],
        ProfilerReactionConfig {
            window_size: 10,
            report_interval_secs: 60,
        },
    );

    reaction.start().await.unwrap();
    reaction.stop().await.unwrap();
    reaction.start().await.unwrap();
    reaction
        .enqueue_query_result(drasi_lib::channels::QueryResult::with_profiling(
            "q1".to_string(),
            1,
            chrono::Utc::now(),
            Vec::new(),
            HashMap::new(),
            ProfilingMetadata {
                source_send_ns: Some(1),
                query_receive_ns: Some(2),
                query_core_call_ns: Some(3),
                query_core_return_ns: Some(4),
                query_send_ns: Some(5),
                reaction_receive_ns: Some(6),
                reaction_complete_ns: Some(7),
                ..Default::default()
            },
        ))
        .await
        .expect("restarted profiler queue should be open");

    tokio::time::timeout(tokio::time::Duration::from_secs(1), async {
        while reaction.stats.read().await.count == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restarted profiler did not process the result");
    reaction.stop().await.unwrap();
}
