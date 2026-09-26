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

use super::*;
use async_trait::async_trait;
use drasi_core::interface::FutureQueue;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::sync::watch;

/// A weak, generation-replaced view of a query's active scheduled-work queue.
/// It does not retain persistent index handles after query shutdown.
pub struct QuerySchedulingResource {
    queue: watch::Sender<Option<Weak<dyn FutureQueue>>>,
    closed: AtomicBool,
}

impl Default for QuerySchedulingResource {
    fn default() -> Self {
        Self {
            queue: watch::channel(None).0,
            closed: AtomicBool::new(false),
        }
    }
}
impl QuerySchedulingResource {
    pub(super) fn ready(&self, queue: Arc<dyn FutureQueue>) {
        self.queue.send_replace(Some(Arc::downgrade(&queue)));
    }
    pub(super) fn stopped(&self) {
        self.queue.send_replace(None);
    }
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::FutureQueue, self.clone()).with_cleanup(self.clone())
    }
}
#[async_trait]
impl ResourceCleanup for QuerySchedulingResource {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        self.stopped();
        Ok(())
    }
}

/// Independent, non-destructive source of due-work notifications. The consumer
/// completes actual scheduled work in its processing transaction.
pub struct QueryScheduledSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    scheduling: Arc<QuerySchedulingResource>,
    sequence: u64,
    not_before: Option<tokio::time::Instant>,
}

fn source_descriptor(id: ComponentId) -> super::Result<ComponentDescriptor> {
    ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
}

impl QueryScheduledSource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        scheduling: Arc<QuerySchedulingResource>,
    ) -> super::Result<Self> {
        Ok(Self {
            descriptor: source_descriptor(id)?,
            stream,
            scheduling,
            sequence: 0,
            not_before: None,
        })
    }
}
#[async_trait]
impl ComputationComponent for QueryScheduledSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.not_before = None;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for QueryScheduledSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let mut updates = self.scheduling.queue.subscribe();
        loop {
            if self.scheduling.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            let queue = updates.borrow_and_update().as_ref().and_then(Weak::upgrade);
            let Some(queue) = queue else {
                updates.changed().await?;
                continue;
            };
            if let Some(not_before) = self.not_before.take() {
                drop(queue);
                tokio::select! {
                    changed = updates.changed() => { changed?; continue; },
                    _ = tokio::time::sleep_until(not_before) => {},
                }
                continue;
            }
            let due = queue.peek_due_time().await?;
            drop(queue);
            let Some(due) = due else {
                tokio::select! {
                    changed = updates.changed() => changed?,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
                continue;
            };
            let timestamp = i64::try_from(due)
                .ok()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .ok_or_else(|| anyhow::anyhow!("scheduled event time is out of range"))?;
            let now = chrono::Utc::now().timestamp_millis();
            if timestamp.timestamp_millis() > now {
                let wait = (timestamp.timestamp_millis() - now).min(5000) as u64;
                tokio::select! {
                    changed = updates.changed() => changed?,
                    _ = tokio::time::sleep(Duration::from_millis(wait)) => {},
                }
                continue;
            }
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("scheduled source sequence exhausted"))?;
            self.not_before = Some(tokio::time::Instant::now() + Duration::from_millis(50));
            return Ok(Some(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: GraphChangeCodec::encode_futures_due(
                    self.descriptor.id(),
                    self.stream.clone(),
                    self.sequence,
                    timestamp,
                )?,
            }));
        }
    }
}

pub struct QueryScheduledSourceFactory {
    descriptor: FactoryDescriptor,
}
impl Default for QueryScheduledSourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    "drasi/query-scheduled-source",
                    "1",
                )
                .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("stream"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("scheduling"),
                    ResourceRequirement::exactly_one::<QuerySchedulingResource>(
                        ResourceRole::FutureQueue,
                    ),
                )]),
            },
        }
    }
}
#[async_trait]
impl ComponentFactory for QueryScheduledSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = source_descriptor(spec.descriptor.id().clone())?;
        anyhow::ensure!(
            spec.descriptor == expected,
            "scheduled source requires typed graph input"
        );
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let scheduling = context
            .resources::<QuerySchedulingResource>("scheduling")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing scheduling resource"))
            })?;
        let stream = context
            .configuration()
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing scheduled stream"))
            })?;
        let stream = StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?;
        Ok(ConstructedComponent::source(Box::new(
            QueryScheduledSource::new(context.component_id.clone(), stream, scheduling)
                .map_err(ComponentCreationError::terminal)?,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        channels::{ResultDiff, SourceControl, SourceEvent, SourceEventWrapper},
        queries::priority_queue::QueryEventQueue,
    };
    use drasi_core::{
        evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
        interface::PushType,
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };
    use drasi_functions_cypher::CypherFunctionSet;
    use std::num::NonZeroUsize;

    #[tokio::test]
    async fn scheduled_source_reports_due_time_and_maintains_its_own_sequence() {
        let queue: Arc<dyn FutureQueue> = Arc::new(
            drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue::new(),
        );
        queue
            .push(
                PushType::Always,
                0,
                1,
                &ElementReference::new("original", "one"),
                1,
                2000,
            )
            .await
            .unwrap();
        let scheduling = Arc::new(QuerySchedulingResource::default());
        scheduling.ready(queue.clone());
        let mut source = QueryScheduledSource {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("scheduled").unwrap(),
                vec![],
            )
            .unwrap(),
            stream: StreamId::try_new("scheduled/out").unwrap(),
            scheduling: scheduling.clone(),
            sequence: 0,
            not_before: None,
        };
        let first = source.next().await.unwrap().unwrap().envelope;
        assert_eq!(first.system().sequence(), 1);
        assert_eq!(first.system().timestamp().unwrap().timestamp_millis(), 2000);
        assert!(GraphChangeCodec::is_futures_due(&first));
        source.stop().await.unwrap();
        source.start().await.unwrap();
        let second = source.next().await.unwrap().unwrap().envelope;
        assert_eq!(second.system().sequence(), 2);
        assert_eq!(second.system().timestamp(), first.system().timestamp());
        scheduling.shutdown().await.unwrap();
        assert!(source.next().await.unwrap().is_none());
    }

    fn change(name: &str, ready: bool, insert: bool) -> SourceChange {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", name),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: if insert {
                    match name {
                        "one" => 1_000,
                        "two" => 1_001,
                        _ => 1_002,
                    }
                } else if name == "one" {
                    5_000
                } else {
                    7_000
                },
            },
            properties: ElementPropertyMap::from(serde_json::json!({"name":name,"ready":ready})),
        };
        if insert {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        }
    }

    fn legacy_diffs(results: Vec<QueryPartEvaluationContext>) -> Vec<ResultDiff> {
        results
            .into_iter()
            .filter_map(|result| match result {
                QueryPartEvaluationContext::Adding {
                    after,
                    row_signature,
                } => Some(ResultDiff::Add {
                    data: serde_json::json!({"name": after["name"].to_string()}),
                    row_signature,
                }),
                QueryPartEvaluationContext::Removing {
                    before,
                    row_signature,
                } => Some(ResultDiff::Delete {
                    data: serde_json::json!({"name": before["name"].to_string()}),
                    row_signature,
                }),
                QueryPartEvaluationContext::Noop => None,
                other => panic!("unexpected scheduled result {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn queued_source_updates_and_scheduled_batches_share_the_same_order_in_both_engines() {
        let text = "MATCH (n:Item) WHERE drasi.trueFor(n.ready, duration({ seconds: 5 })) RETURN n.name AS name";
        let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let legacy = drasi_core::query::QueryBuilder::new(
            text,
            Arc::new(drasi_query_cypher::CypherParser::new(functions.clone())),
        )
        .with_function_registry(functions)
        .build()
        .await;
        let mut native = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "scheduled-order".into(),
                id: ComponentId::try_new("query").unwrap(),
                query: text.into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(16).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await
        .unwrap();
        native.start().await.unwrap();
        for (sequence, name) in ["one", "two", "three"].into_iter().enumerate() {
            let change = change(name, true, true);
            assert!(legacy
                .process_source_change(change.clone())
                .await
                .unwrap()
                .is_empty());
            assert!(native
                .transform(InputEnvelope {
                    port: PortId::try_new("in").unwrap(),
                    envelope: GraphChangeCodec::encode_source_event(
                        Arc::new(SourceEventWrapper::with_sequence(
                            "source".into(),
                            SourceEvent::Change(change),
                            chrono::DateTime::from_timestamp_millis(1_000).unwrap(),
                            sequence as u64 + 1,
                            None,
                        )),
                        &ComponentId::try_new("adapter").unwrap(),
                        StreamId::try_new("input").unwrap(),
                        sequence as u64 + 10,
                        None
                    )
                    .unwrap(),
                })
                .await
                .unwrap()
                .is_empty());
        }
        let queue_id = ResourceId::try_new("ordered").unwrap();
        let inbox = RankedInputQueue::new(8).unwrap();
        let resources = BTreeMap::from([(queue_id.clone(), inbox.resource())]);
        let mut inputs = RankedInputPipeConfig {
            queue: queue_id.clone(),
            capacity: 8,
            source_rank: 0,
            source_id: Some("source".into()),
            drop_when_full: false,
        }
        .create_with_resources(&resources)
        .unwrap();
        let mut scheduled = RankedInputPipeConfig {
            queue: queue_id,
            capacity: 8,
            source_rank: 1,
            source_id: None,
            drop_when_full: false,
        }
        .create_with_resources(&resources)
        .unwrap();
        let legacy_queue = QueryEventQueue::new(8, ["source"]).unwrap();
        let future = crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID;
        for (ordinal, event) in [
            SourceEventWrapper::with_sequence(
                "source".into(),
                SourceEvent::Change(change("two", false, false)),
                chrono::DateTime::from_timestamp_millis(7_000).unwrap(),
                5,
                None,
            ),
            SourceEventWrapper::with_sequence(
                future.into(),
                SourceEvent::Control(SourceControl::FuturesDue),
                chrono::DateTime::from_timestamp_millis(6_000).unwrap(),
                1,
                None,
            ),
            SourceEventWrapper::with_sequence(
                "source".into(),
                SourceEvent::Change(change("one", false, false)),
                chrono::DateTime::from_timestamp_millis(5_000).unwrap(),
                4,
                None,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let event = Arc::new(event);
            legacy_queue.enqueue_wait(event.clone()).await.unwrap();
            let envelope = GraphChangeCodec::encode_source_event(
                event.clone(),
                &ComponentId::try_new("adapter").unwrap(),
                StreamId::try_new(if event.source_id == future {
                    "future/input"
                } else {
                    "input"
                })
                .unwrap(),
                ordinal as u64 + 20,
                None,
            )
            .unwrap();
            if event.source_id == future {
                scheduled.pipe.sender().send(envelope).await.unwrap();
            } else {
                inputs.pipe.sender().send(envelope).await.unwrap();
            }
        }
        let mut input_receiver = inputs.pipe.take_receiver().unwrap();
        let mut scheduled_receiver = scheduled.pipe.take_receiver().unwrap();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        for scheduled in [false, true, false] {
            let event = legacy_queue.dequeue().await;
            match &event.event {
                SourceEvent::Change(change) => expected.extend(legacy_diffs(
                    legacy.process_source_change(change.clone()).await.unwrap(),
                )),
                SourceEvent::Control(SourceControl::FuturesDue) => {
                    while let Some(due) = legacy.process_due_futures().await.unwrap() {
                        expected.extend(legacy_diffs(due.results));
                    }
                }
                _ => unreachable!(),
            }
            let receiver = if scheduled {
                &mut scheduled_receiver
            } else {
                &mut input_receiver
            };
            let delivered = tokio::time::timeout(Duration::from_secs(2), receiver.receive())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let (envelope, _) = delivered.into_parts();
            let mut output = native
                .transform(InputEnvelope {
                    port: PortId::try_new("in").unwrap(),
                    envelope,
                })
                .await
                .unwrap();
            loop {
                for envelope in output {
                    actual.extend(
                        QueryChangeCodec::to_legacy_result(&envelope.envelope)
                            .unwrap()
                            .results,
                    );
                }
                if !native.has_pending_emissions() {
                    break;
                }
                output = native.continue_transform().await.unwrap();
                assert!(
                    output.len() <= 1,
                    "scheduled batches must stream bounded emissions"
                );
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(
            actual.len(),
            3,
            "two scheduled additions, then the queued deletion"
        );
        assert!(
            matches!(&actual[2], ResultDiff::Delete { data, .. } if data == &serde_json::json!({"name":"two"}))
        );
        native.stop().await.unwrap();
    }
}
