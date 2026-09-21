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
use drasi_lib::{
    channels::ResultDiff,
    profiling::{timestamp_ns, ProfilingMetadata},
    ReactionRuntimeContext,
};
use std::time::Duration;

fn query_result(query: &str, sequence: u64) -> QueryResult {
    QueryResult::new(
        query.into(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: serde_json::json!({"value": 42}),
            row_signature: 7,
        }],
        Default::default(),
    )
}

#[tokio::test]
async fn forwarded_profiling_records_reaction_work_without_changing_payload() {
    let (reaction, handle) = ApplicationReaction::new("profiled", vec!["query".into()]);
    let (updates, _events) = mpsc::channel(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "instance", "profiled", None, updates, None,
        ))
        .await;
    let mut receiver = handle.take_receiver().await.unwrap();
    reaction.start().await.unwrap();
    for (index, profiling) in [
        None,
        Some(ProfilingMetadata {
            source_receive_ns: Some(11),
            query_core_return_ns: Some(23),
            query_send_ns: Some(29),
            ..Default::default()
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let mut input = query_result("query", index as u64 + 1);
        input.profiling = profiling;
        let before_enqueue = timestamp_ns();
        reaction.enqueue_query_result(input.clone()).await.unwrap();
        let mut received = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let after_receive = timestamp_ns();
        if let Some(profiling) = &received.profiling {
            let started = profiling.reaction_receive_ns.unwrap();
            let completed = profiling.reaction_complete_ns.unwrap();
            assert!(before_enqueue <= started && started <= completed);
            assert!(completed <= after_receive);
            let mut preserved = profiling.clone();
            preserved.reaction_receive_ns = None;
            preserved.reaction_complete_ns = None;
            assert_eq!(Some(preserved), input.profiling);
        } else {
            assert!(input.profiling.is_none());
        }
        received.profiling = input.profiling.clone();
        assert_eq!(
            serde_json::to_value(received).unwrap(),
            serde_json::to_value(input).unwrap()
        );
    }
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn completion_timestamp_includes_application_channel_backpressure() {
    let (reaction, handle) = ApplicationReaction::new("backpressured", vec!["query".into()]);
    let (updates, _events) = mpsc::channel(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "instance",
            "backpressured",
            None,
            updates,
            None,
        ))
        .await;
    let mut receiver = handle.take_receiver().await.unwrap();
    let capacity = reaction.app_tx.capacity();
    for _ in 0..capacity {
        reaction
            .app_tx
            .try_send(query_result("prefill", 0))
            .unwrap();
    }
    reaction.start().await.unwrap();
    let mut input = query_result("query", 1);
    input.profiling = Some(ProfilingMetadata::default());
    reaction.enqueue_query_result(input).await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(2)).await;
    let released = timestamp_ns();
    for _ in 0..capacity {
        assert_eq!(receiver.recv().await.unwrap().query_id, "prefill");
    }
    let result = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let profiling = result.profiling.unwrap();
    assert!(profiling.reaction_receive_ns.unwrap() <= released);
    assert!(profiling.reaction_complete_ns.unwrap() >= released);
    reaction.stop().await.unwrap();
}
