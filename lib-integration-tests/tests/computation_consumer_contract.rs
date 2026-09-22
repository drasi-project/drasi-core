// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{channels::ResultDiff, computation::v1::*};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
struct Applied {
    rows: Mutex<BTreeMap<u64, serde_json::Value>>,
    calls: Mutex<Vec<(bool, u64)>>,
    fail_second: AtomicBool,
}

impl Applied {
    fn apply(&self, envelope: &ChangeEnvelope, replace: bool) -> Result<()> {
        anyhow::ensure!(
            !QueryChangeCodec::is_progress_only(envelope),
            "progress decisions must not reach the row handler"
        );
        QueryChangeCodec::metadata(envelope)?;
        let diffs = if replace
            && QueryChangeCodec::is_snapshot(envelope)
            && envelope.changes().is_empty()
        {
            Vec::new()
        } else {
            QueryChangeCodec::to_legacy_result(envelope)
                .with_context(|| format!("sink projection: replace={replace}"))?
                .results
        };
        let sequence = QueryChangeCodec::query_sequence(envelope)?;
        if !replace && sequence == 2 && self.fail_second.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected handling failure");
        }
        let mut rows = self.rows.lock().expect("rows");
        if replace {
            rows.clear();
        }
        for diff in diffs {
            match diff {
                ResultDiff::Add {
                    row_signature,
                    data,
                } => {
                    rows.insert(row_signature, data);
                }
                ResultDiff::Update {
                    row_signature,
                    after,
                    ..
                }
                | ResultDiff::Aggregation {
                    row_signature,
                    after,
                    ..
                } => {
                    rows.insert(row_signature, after);
                }
                ResultDiff::Delete { row_signature, .. } => {
                    rows.remove(&row_signature);
                }
                ResultDiff::Noop => {}
            }
        }
        self.calls.lock().expect("calls").push((replace, sequence));
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .rows
            .lock()
            .expect("rows")
            .values()
            .map(|row| row["name"].as_str().expect("name").to_owned())
            .collect();
        names.sort();
        names
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    applied: Arc<Applied>,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn supports_snapshot(&self) -> bool {
        true
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.applied.apply(&input.envelope, false)
    }
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> Result<()> {
        self.applied.apply(&input.envelope, true)
    }
}

async fn producer(capacity: usize) -> Result<ContinuousQueryTransformer> {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "consumer-contract".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(capacity).expect("capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?;
    query.start().await?;
    Ok(query)
}

async fn insert(
    query: &mut ContinuousQueryTransformer,
    sequence: u64,
    name: &str,
) -> Result<ChangeEnvelope> {
    let envelope = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", name),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 1000 + sequence,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"name":name})),
            },
        },
        StreamId::try_new("source/out")?,
        sequence,
        None,
    )?;
    let mut output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
        .await?;
    assert_eq!(output.len(), 1);
    Ok(output.remove(0).envelope)
}

async fn consumer(
    query: &ContinuousQueryTransformer,
    progress: Arc<dyn ConsumerProgressStore>,
    applied: Arc<Applied>,
    policy: ConsumerRecoveryPolicy,
) -> Result<(QueryReplayTransformer, CheckpointedSink)> {
    let mut replay = QueryReplayTransformer::new(
        ComponentId::try_new("replay")?,
        "query".into(),
        StreamId::try_new("replay/out")?,
        query.results(),
        progress.clone(),
        policy,
    );
    let mut sink = CheckpointedSink::new(
        Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("sink")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            applied,
        }),
        progress,
    )?;
    if policy == ConsumerRecoveryPolicy::AutoSkipGap {
        sink = sink.allow_skipped_resets();
    }
    replay.start().await?;
    sink.start().await?;
    Ok((replay, sink))
}

async fn deliver(sink: &mut CheckpointedSink, output: Vec<OutputEnvelope>) -> Result<()> {
    for output in output {
        sink.handle(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.envelope,
        })
        .await?;
    }
    Ok(())
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope,
    }
}

#[tokio::test]
async fn recreated_volatile_query_cannot_reuse_a_consumers_same_numbered_checkpoint() -> Result<()>
{
    for (policy, empty_replacement) in [
        ConsumerRecoveryPolicy::Strict,
        ConsumerRecoveryPolicy::AutoReset,
        ConsumerRecoveryPolicy::AutoSkipGap,
    ]
    .into_iter()
    .flat_map(|policy| [false, true].map(|empty| (policy, empty)))
    {
        let progress = Arc::new(MemoryConsumerProgress::default());
        let applied = Arc::new(Applied::default());
        let mut old_query = producer(8).await?;
        let stale = insert(&mut old_query, 1, "Alice").await?;
        let (mut replay, mut sink) =
            consumer(&old_query, progress.clone(), applied.clone(), policy).await?;
        deliver(&mut sink, replay.on_wakeup().await?).await?;
        assert_eq!(applied.names(), ["Alice"]);
        assert_eq!(
            progress.load("query").await?.expect("checkpoint").sequence,
            1
        );
        replay.stop().await?;
        sink.stop().await?;
        old_query.stop().await?;
        drop((replay, sink, old_query));

        let mut replacement = producer(8).await?;
        if !empty_replacement {
            insert(&mut replacement, 1, "Bob").await?;
        }
        assert_eq!(replacement.results().snapshot()?.generation, 0);
        let (mut replay, mut sink) =
            consumer(&replacement, progress.clone(), applied.clone(), policy).await?;
        let recovered = replay.on_wakeup().await;
        if policy == ConsumerRecoveryPolicy::Strict {
            assert!(
                recovered.is_err(),
                "same sequence/generation must not hide a different query instance"
            );
            assert_eq!(applied.names(), ["Alice"]);
        } else {
            deliver(&mut sink, recovered?)
                .await
                .with_context(|| format!("replacement {policy:?}, empty={empty_replacement}"))?;
            assert_eq!(
                applied.names(),
                if policy == ConsumerRecoveryPolicy::AutoReset {
                    if empty_replacement {
                        vec![]
                    } else {
                        vec!["Bob"]
                    }
                } else {
                    vec!["Alice"]
                }
            );
            assert!(
                replay.transform(input(stale)).await.is_err(),
                "old producer output must be rejected"
            );
            let next_sequence = if empty_replacement { 1 } else { 2 };
            let next = insert(&mut replacement, next_sequence, "Charlie").await?;
            deliver(&mut sink, replay.transform(input(next)).await?).await?;
            assert_eq!(
                applied.names(),
                if policy == ConsumerRecoveryPolicy::AutoReset {
                    if empty_replacement {
                        vec!["Charlie"]
                    } else {
                        vec!["Bob", "Charlie"]
                    }
                } else {
                    vec!["Alice", "Charlie"]
                }
            );
            assert_eq!(
                progress
                    .load("query")
                    .await?
                    .expect("new checkpoint")
                    .sequence,
                next_sequence
            );
        }
        replay.stop().await?;
        sink.stop().await?;
        replacement.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn live_sequence_gaps_recover_or_apply_the_explicit_policy_before_advancing() -> Result<()> {
    for capacity in [1, 8] {
        for policy in [
            ConsumerRecoveryPolicy::Strict,
            ConsumerRecoveryPolicy::AutoReset,
            ConsumerRecoveryPolicy::AutoSkipGap,
        ] {
            let progress = Arc::new(MemoryConsumerProgress::default());
            let applied = Arc::new(Applied::default());
            let mut query = producer(capacity).await?;
            insert(&mut query, 1, "Alice").await?;
            let (mut replay, mut sink) =
                consumer(&query, progress.clone(), applied.clone(), policy).await?;
            deliver(&mut sink, replay.on_wakeup().await?).await?;
            insert(&mut query, 2, "Bob").await?;
            let third = insert(&mut query, 3, "Charlie").await?;
            let output = replay.transform(input(third.clone())).await;
            if capacity == 1 && policy == ConsumerRecoveryPolicy::Strict {
                assert!(output.is_err(), "evicted missing output cannot be ignored");
                assert_eq!(applied.names(), ["Alice"]);
                assert_eq!(
                    progress
                        .load("query")
                        .await?
                        .expect("unchanged checkpoint")
                        .sequence,
                    1
                );
            } else {
                deliver(&mut sink, output?).await?;
                assert_eq!(
                    applied.names(),
                    if capacity == 1 && policy == ConsumerRecoveryPolicy::AutoSkipGap {
                        vec!["Alice", "Charlie"]
                    } else {
                        vec!["Alice", "Bob", "Charlie"]
                    }
                );
                assert_eq!(
                    progress
                        .load("query")
                        .await?
                        .expect("handled checkpoint")
                        .sequence,
                    3
                );
                let count = applied.calls.lock().expect("calls").len();
                assert!(replay.transform(input(third)).await?.is_empty());
                assert_eq!(applied.calls.lock().expect("calls").len(), count);
            }
            replay.stop().await?;
            sink.stop().await?;
            query.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn checkpointed_sink_rejects_an_unexplained_live_gap_without_running_the_handler(
) -> Result<()> {
    let progress = Arc::new(MemoryConsumerProgress::default());
    let applied = Arc::new(Applied::default());
    let mut query = producer(8).await?;
    insert(&mut query, 1, "Alice").await?;
    let (mut replay, mut sink) = consumer(
        &query,
        progress.clone(),
        applied.clone(),
        ConsumerRecoveryPolicy::Strict,
    )
    .await?;
    deliver(&mut sink, replay.on_wakeup().await?).await?;
    insert(&mut query, 2, "Bob").await?;
    let third = insert(&mut query, 3, "Charlie").await?;
    let count = applied.calls.lock().expect("calls").len();
    assert!(
        sink.handle(input(third)).await.is_err(),
        "a handling checkpoint cannot silently jump from 1 to 3"
    );
    assert_eq!(applied.calls.lock().expect("calls").len(), count);
    assert_eq!(applied.names(), ["Alice"]);
    assert_eq!(
        progress.load("query").await?.expect("checkpoint").sequence,
        1
    );
    replay.stop().await?;
    sink.stop().await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test]
async fn failed_gap_replay_does_not_checkpoint_the_unhandled_suffix() -> Result<()> {
    let progress = Arc::new(MemoryConsumerProgress::default());
    let applied = Arc::new(Applied::default());
    let mut query = producer(8).await?;
    insert(&mut query, 1, "Alice").await?;
    let (mut replay, mut sink) = consumer(
        &query,
        progress.clone(),
        applied.clone(),
        ConsumerRecoveryPolicy::Strict,
    )
    .await?;
    deliver(&mut sink, replay.on_wakeup().await?).await?;
    insert(&mut query, 2, "Bob").await?;
    let third = insert(&mut query, 3, "Charlie").await?;
    applied.fail_second.store(true, Ordering::SeqCst);
    let outputs = replay.transform(input(third)).await?;
    let error = deliver(&mut sink, outputs)
        .await
        .expect_err("failed handler");
    assert!(error.to_string().contains("injected handling failure"));
    assert_eq!(applied.names(), ["Alice"]);
    assert_eq!(
        progress.load("query").await?.expect("checkpoint").sequence,
        1
    );

    replay.stop().await?;
    sink.stop().await?;
    replay.start().await?;
    sink.start().await?;
    deliver(&mut sink, replay.on_wakeup().await?).await?;
    assert_eq!(applied.names(), ["Alice", "Bob", "Charlie"]);
    assert_eq!(
        progress.load("query").await?.expect("checkpoint").sequence,
        3
    );
    replay.stop().await?;
    sink.stop().await?;
    query.stop().await?;
    Ok(())
}
