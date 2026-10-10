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

#![cfg(test)]
#![cfg(feature = "computation-rocksdb-tests")]

use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{IndexError, IndexSet, SessionControl},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

#[path = "computation_transaction_transformer/reliability.rs"]
mod reliability;

type TestResult<T> = anyhow::Result<T>;

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream")
}
fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero")
}

fn provider(path: &std::path::Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
        ),
    ))
}

#[derive(Default)]
struct Probe {
    fail: AtomicBool,
    wait: AtomicBool,
    invalid_output: AtomicBool,
    entered: Notify,
    trace: Mutex<Vec<(String, i64, i64)>>,
    standalone_calls: AtomicUsize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepConfig {
    #[serde(default)]
    add: i64,
    #[serde(default = "one")]
    multiply: i64,
    #[serde(default)]
    filter: bool,
    #[serde(default)]
    expand: bool,
    #[serde(default)]
    second: bool,
}
fn one() -> i64 {
    1
}

struct Arithmetic {
    descriptor: ComponentDescriptor,
    config: StepConfig,
    probe: Arc<Probe>,
}

fn descriptor(id: ComponentId, extra_port: bool) -> ComponentDescriptor {
    let mut ports = vec![
        PortDescriptor::new(
            port("in"),
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        ),
        PortDescriptor::new(
            port("out"),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        ),
    ];
    if extra_port {
        ports.push(PortDescriptor::new(
            port("another"),
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        ));
    }
    ComponentDescriptor::try_new(id, ports).expect("descriptor")
}

impl Arithmetic {
    fn change(&self, input: &ChangeEnvelope, count: i64) -> TestResult<ChangeEnvelope> {
        let mut output = Vec::new();
        for mut change in GraphChangeCodec::decode_changes(input)? {
            if let SourceChange::Insert { element } | SourceChange::Update { element } = &mut change
            {
                let Element::Node { properties, .. } = element else {
                    anyhow::bail!("expected node")
                };
                let Some(ElementValue::Integer(value)) = properties.get("value") else {
                    anyhow::bail!("expected value")
                };
                let value = (value + self.config.add) * self.config.multiply;
                properties.insert("value", ElementValue::Integer(value));
                properties.insert(
                    &format!("{}_count", self.descriptor.id()),
                    ElementValue::Integer(count),
                );
                self.probe.trace.lock().expect("trace").push((
                    self.descriptor.id().to_string(),
                    value,
                    count,
                ));
                if self.config.expand {
                    let mut duplicate = element.clone();
                    let Element::Node { metadata, .. } = &mut duplicate else {
                        unreachable!()
                    };
                    metadata.reference.element_id =
                        format!("{}-copy", metadata.reference.element_id).into();
                    output.push(SourceChange::Insert { element: duplicate });
                }
            }
            if !self.config.filter {
                output.push(change);
            }
        }
        Ok(GraphChangeCodec::derive_changes(
            input,
            &output,
            stream(&format!("{}/out", self.descriptor.id())),
            input.system().sequence(),
        )?)
    }
}

#[async_trait]
impl ComputationComponent for Arithmetic {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl Transformer for Arithmetic {
    async fn transform(&mut self, input: InputEnvelope) -> TestResult<Vec<OutputEnvelope>> {
        self.probe.standalone_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: self.change(&input.envelope, 0)?,
        }])
    }
}
#[async_trait]
impl TransactionalTransformer for Arithmetic {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }

    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> TestResult<ChangeEnvelope> {
        assert_eq!(context.step_id(), self.descriptor.id());
        let count = match context.get("counter").await? {
            None => 1,
            Some(ElementValue::Integer(value)) => value + 1,
            _ => anyhow::bail!("counter is corrupt"),
        };
        context.put("counter", ElementValue::Integer(count)).await?;
        assert_eq!(
            context.get("counter").await?,
            Some(ElementValue::Integer(count)),
            "read own write"
        );
        let reference = ElementReference::new("shared-source", "same-id");
        if let Some(previous) = context.get_element(&reference).await? {
            assert_eq!(
                previous.get_property("owner"),
                &ElementValue::String(self.descriptor.id().to_string().into())
            );
        }
        context
            .put_element(&Element::Node {
                metadata: ElementMetadata {
                    reference: reference.clone(),
                    labels: Arc::from([]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(
                    json!({"owner":self.descriptor.id().as_str()}),
                ),
            })
            .await?;
        assert_eq!(
            context
                .get_element(&reference)
                .await?
                .expect("step element")
                .get_reference(),
            &reference
        );
        let result = self.change(&input, count)?;
        if self.config.second {
            self.probe.entered.notify_one();
            if self.probe.wait.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            anyhow::ensure!(
                !self.probe.fail.load(Ordering::Acquire),
                "injected second-step failure"
            );
            if self.probe.invalid_output.load(Ordering::Acquire) {
                return Ok(number_event(7, input.system().sequence()));
            }
        }
        Ok(result)
    }
}

struct ArithmeticFactory(Arc<Probe>);
impl TransactionalTransformerFactory for ArithmeticFactory {
    fn implementation(&self) -> ImplementationIdentity {
        ImplementationIdentity::try_new("test/arithmetic", "1").expect("implementation")
    }
    fn configuration_version(&self) -> u32 {
        1
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> TestResult<Box<dyn TransactionalTransformer>> {
        Ok(Box::new(Arithmetic {
            descriptor: descriptor(definition.id.clone(), false),
            config: serde_json::from_value(definition.configuration.clone())?,
            probe: self.0.clone(),
        }))
    }
}

fn step(name: &str, config: serde_json::Value) -> TransactionStepDefinition {
    TransactionStepDefinition {
        id: id(name),
        implementation: ImplementationIdentity::try_new("test/arithmetic", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: config,
    }
}

fn definition() -> TransactionTransformerDefinition {
    TransactionTransformerDefinition {
        graph_id: "transactions".into(),
        id: id("sequence"),
        output_stream: stream("sequence/out"),
        steps: vec![
            step("first", json!({"add":2})),
            step("second", json!({"multiply":4,"second":true})),
        ],
        outbox_capacity: size(8),
    }
}

fn registry(probe: Arc<Probe>) -> Arc<TransactionalTransformerRegistry> {
    let mut registry = TransactionalTransformerRegistry::default();
    registry
        .register(Arc::new(ArithmeticFactory(probe)))
        .expect("register");
    Arc::new(registry)
}

fn input(sequence: u64, value: i64) -> InputEnvelope {
    InputEnvelope {
        port: port("in"),
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: sequence,
                    },
                    properties: ElementPropertyMap::from(json!({"value":value})),
                },
            },
            stream("source/out"),
            sequence,
            None,
        )
        .expect("input"),
    }
}

fn values(envelope: &ChangeEnvelope, property: &str) -> Vec<i64> {
    GraphChangeCodec::decode_changes(envelope)
        .expect("decode")
        .iter()
        .map(|change| {
            let SourceChange::Insert { element } = change else {
                panic!("insert")
            };
            let ElementValue::Integer(value) = element.get_property(property) else {
                panic!("integer")
            };
            *value
        })
        .collect()
}

async fn open(
    definition: TransactionTransformerDefinition,
    registry: Arc<TransactionalTransformerRegistry>,
    provider: Arc<dyn ComputationIndexProvider>,
) -> TransactionTransformer {
    let mut subject = TransactionTransformer::new(definition, registry, provider)
        .await
        .expect("construct");
    subject.start().await.expect("start");
    subject
}

#[derive(Default)]
struct Counts {
    begins: AtomicUsize,
    commits: AtomicUsize,
    rollbacks: AtomicUsize,
    fail_commit: AtomicBool,
    wait_after_commit: AtomicBool,
    wait_before_commit: AtomicBool,
    committing: Notify,
    committed: Notify,
}
struct CountSession {
    inner: Arc<dyn SessionControl>,
    counts: Arc<Counts>,
}
#[async_trait]
impl SessionControl for CountSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.counts.begins.fetch_add(1, Ordering::SeqCst);
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        self.counts.commits.fetch_add(1, Ordering::SeqCst);
        if self.counts.wait_before_commit.swap(false, Ordering::AcqRel) {
            self.counts.committing.notify_one();
            std::future::pending::<()>().await;
        }
        if self.counts.fail_commit.swap(false, Ordering::AcqRel) {
            return Err(IndexError::IOError);
        }
        self.inner.commit().await?;
        if self.counts.wait_after_commit.swap(false, Ordering::AcqRel) {
            self.counts.committed.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.counts.rollbacks.fetch_add(1, Ordering::SeqCst);
        self.inner.rollback()
    }
}
struct CountProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    counts: Arc<Counts>,
}
#[async_trait]
impl ComputationIndexProvider for CountProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let set = original.indexes();
        let control: Arc<dyn SessionControl> = Arc::new(CountSession {
            inner: set.session_control.clone(),
            counts: self.counts.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        Ok(ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().expect("checkpoints").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().expect("live").clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?
        .with_cleanup(original.cleanup().expect("cleanup").clone()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn one_input_uses_one_transaction_and_separate_step_state_without_standalone_calls() {
    let directory = tempfile::tempdir().expect("temp");
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let provider = Arc::new(CountProvider {
        inner: provider(directory.path()),
        counts: counts.clone(),
    });
    let mut subject = open(definition(), registry(probe.clone()), provider).await;
    let starts = counts.begins.load(Ordering::SeqCst);
    let commits = counts.commits.load(Ordering::SeqCst);
    let output = subject.transform(input(1, 3)).await.expect("transform");
    assert_eq!(values(&output[0].envelope, "value"), [20]);
    assert_eq!(values(&output[0].envelope, "first_count"), [1]);
    assert_eq!(values(&output[0].envelope, "second_count"), [1]);
    assert_eq!(counts.begins.load(Ordering::SeqCst) - starts, 1);
    assert_eq!(counts.commits.load(Ordering::SeqCst) - commits, 1);
    assert_eq!(probe.standalone_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        *probe.trace.lock().expect("trace"),
        [("first".into(), 5, 1), ("second".into(), 20, 1)]
    );
    subject.delivery_completed(&output).await.expect("confirm");
    let output = subject.transform(input(2, 4)).await.expect("next");
    assert_eq!(values(&output[0].envelope, "value"), [24]);
    assert_eq!(values(&output[0].envelope, "first_count"), [2]);
    assert_eq!(values(&output[0].envelope, "second_count"), [2]);
    subject.delivery_completed(&output).await.expect("confirm");
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn failed_or_cancelled_later_step_rolls_back_every_step_and_input_progress() {
    for cancel in [false, true] {
        let directory = tempfile::tempdir().expect("temp");
        let provider = provider(directory.path());
        let probe = Arc::new(Probe::default());
        let registry = registry(probe.clone());
        {
            let mut subject = open(definition(), registry.clone(), provider.clone()).await;
            if cancel {
                probe.wait.store(true, Ordering::Release);
                let pending = subject.transform(input(1, 3));
                tokio::pin!(pending);
                tokio::select! {
                    result = &mut pending => panic!("unexpected completion: {result:?}"),
                    _ = probe.entered.notified() => {}
                }
            } else {
                probe.fail.store(true, Ordering::Release);
                assert!(subject
                    .transform(input(1, 3))
                    .await
                    .expect_err("step failure")
                    .to_string()
                    .contains("second"));
            }
            assert!(
                subject.transform(input(2, 8)).await.is_err(),
                "failed participant state cannot be reused"
            );
            subject.stop().await.expect("rollback and join");
        }
        probe.wait.store(false, Ordering::Release);
        probe.fail.store(false, Ordering::Release);
        let mut subject = open(definition(), registry, provider).await;
        assert!(
            !subject.has_pending_emissions(),
            "failed transaction has no saved output"
        );
        let output = subject
            .transform(input(1, 3))
            .await
            .expect("retry same input");
        assert_eq!(
            values(&output[0].envelope, "first_count"),
            [1],
            "first step write rolled back"
        );
        assert_eq!(
            values(&output[0].envelope, "second_count"),
            [1],
            "later step write rolled back"
        );
        assert_eq!(
            GraphProducerProgress::from_envelope(&output[0].envelope)
                .expect("progress")
                .expect("present")
                .sequence(),
            1
        );
        subject.delivery_completed(&output).await.expect("confirm");
        subject.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn invalid_intermediate_schema_rolls_back_before_the_next_step_can_run() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let probe = Arc::new(Probe::default());
    let registry = registry(probe.clone());
    let mut definition = definition();
    definition.steps.push(step("third", json!({"add":5})));
    {
        let mut subject = open(definition.clone(), registry.clone(), provider.clone()).await;
        probe.invalid_output.store(true, Ordering::Release);
        let error = subject
            .transform(input(1, 3))
            .await
            .expect_err("foreign schema");
        assert!(format!("{error:#}").contains("schema"), "{error:#}");
        assert_eq!(
            *probe.trace.lock().expect("trace"),
            [("first".into(), 5, 1), ("second".into(), 20, 1)],
            "the third step must never see the invalid intermediate result"
        );
        assert!(subject.transform(input(2, 4)).await.is_err());
        subject.stop().await.expect("rollback");
    }
    probe.invalid_output.store(false, Ordering::Release);
    let mut reopened = open(definition, registry, provider).await;
    assert!(!reopened.has_pending_emissions());
    let output = reopened
        .transform(input(1, 3))
        .await
        .expect("retry rolled-back input");
    assert_eq!(values(&output[0].envelope, "value"), [25]);
    for name in ["first_count", "second_count", "third_count"] {
        assert_eq!(values(&output[0].envelope, name), [1], "{name}");
    }
    reopened.delivery_completed(&output).await.expect("confirm");
    reopened.stop().await.expect("stop");
}

#[tokio::test]
async fn every_participant_position_rolls_back_failure_and_cancellation_in_variable_length_sequences(
) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
            for length in [1, 3, 5] {
                for failure_at in 0..length {
                    for cancel in [false, true] {
                        let directory = tempfile::tempdir().expect("temp");
                        let provider = provider(directory.path());
                        let probe = Arc::new(Probe::default());
                        let registry = registry(probe.clone());
                        let mut definition = definition();
                        definition.steps = (0..length).map(|position| {
                            step(&format!("stage-{position}"), json!({"add": 1, "second": position == failure_at}))
                        }).collect();
                        {
                            let mut subject = open(definition.clone(), registry.clone(), provider.clone()).await;
                            if cancel {
                                probe.wait.store(true, Ordering::Release);
                                let pending = subject.transform(input(1, 10));
                                tokio::pin!(pending);
                                tokio::select! {
                                    result = &mut pending => panic!("completed before cancellation: {result:?}"),
                                    _ = probe.entered.notified() => {}
                                }
                            } else {
                                probe.fail.store(true, Ordering::Release);
                                let failure = subject.transform(input(1, 10)).await.expect_err("injected failure");
                                assert!(format!("{failure:#}").contains("injected second-step failure"));
                            }
                            assert_eq!(probe.trace.lock().expect("trace").len(), failure_at + 1,
                                "no participant after the failed one may run");
                            assert!(subject.transform(input(2, 20)).await.is_err(), "fenced until reconstruction");
                            subject.stop().await.expect("rollback and release");
                        }
                        probe.wait.store(false, Ordering::Release);
                        probe.fail.store(false, Ordering::Release);
                        probe.trace.lock().expect("trace").clear();
                        let mut subject = open(definition, registry, provider).await;
                        assert!(!subject.has_pending_emissions(), "rollback must leave no committed output");
                        for (sequence, value) in [(1, 10), (2, 20)] {
                            let output = subject.transform(input(sequence, value)).await.expect("commit input");
                            assert_eq!(output.len(), 1);
                            assert_eq!(values(&output[0].envelope, "value"), [value + length as i64]);
                            for position in 0..length {
                                assert_eq!(values(&output[0].envelope, &format!("stage-{position}_count")), [sequence as i64],
                                    "every participant must roll back, including writes before the fault");
                            }
                            subject.delivery_completed(&output).await.expect("confirm output");
                        }
                        assert_eq!(probe.standalone_calls.load(Ordering::Acquire), 0,
                            "participants must use the shared transaction, not standalone execution");
                        subject.stop().await.expect("stop");
                    }
                }
            }
        }).await.expect("all failure positions must terminate");
}

#[tokio::test]
async fn equal_sequence_numbers_from_different_inputs_get_distinct_step_and_output_identities() {
    let directory = tempfile::tempdir().expect("temp");
    let mut subject = open(
        definition(),
        registry(Arc::new(Probe::default())),
        provider(directory.path()),
    )
    .await;
    let first = subject
        .transform(input(1, 3))
        .await
        .expect("first producer");
    subject.delivery_completed(&first).await.expect("confirm");
    let source = input(1, 4).envelope;
    let other = source.derive(
        emission_id(&stream("other/out"), 1).expect("id"),
        source.changes().clone(),
        SystemMetadata::new(stream("other/out"), 1),
    );
    let second = subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: other,
        })
        .await
        .expect("second producer");
    assert_ne!(first[0].envelope.id(), second[0].envelope.id());
    assert_ne!(
        first[0]
            .envelope
            .lineage()
            .expect("last step")
            .envelope_id(),
        second[0]
            .envelope
            .lineage()
            .expect("last step")
            .envelope_id(),
    );
    assert_eq!(values(&second[0].envelope, "first_count"), [2]);
    assert_eq!(values(&second[0].envelope, "second_count"), [2]);
    subject.delivery_completed(&second).await.expect("confirm");
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn committed_state_and_pending_output_reopen_without_running_steps_again() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let probe = Arc::new(Probe::default());
    let registry = registry(probe.clone());
    let original = {
        let mut subject = open(definition(), registry.clone(), provider.clone()).await;
        let output = subject.transform(input(1, 3)).await.expect("transform");
        subject.stop().await.expect("stop before delivery");
        output[0].envelope.clone()
    };
    assert_eq!(probe.trace.lock().expect("trace").len(), 2);
    let mut subject = open(definition(), registry, provider).await;
    assert!(subject.has_pending_emissions());
    let replay = subject.on_wakeup().await.expect("replay");
    assert_eq!(
        replay[0].envelope.changes().operations(),
        original.changes().operations()
    );
    assert_eq!(replay[0].envelope.changes().id(), original.changes().id());
    assert!(replay[0].envelope.system().sequence() > original.system().sequence());
    assert_eq!(
        probe.trace.lock().expect("trace").len(),
        2,
        "do not re-evaluate committed input"
    );
    subject.delivery_completed(&replay).await.expect("confirm");
    let duplicate = subject
        .transform(input(1, 3))
        .await
        .expect("duplicate input");
    assert_eq!(values(&duplicate[0].envelope, "first_count"), [1]);
    assert_eq!(probe.trace.lock().expect("trace").len(), 2);
    subject
        .delivery_completed(&duplicate)
        .await
        .expect("confirm duplicate");
    let output = subject.transform(input(2, 3)).await.expect("new input");
    assert_eq!(values(&output[0].envelope, "first_count"), [2]);
    assert_eq!(values(&output[0].envelope, "second_count"), [2]);
    subject.delivery_completed(&output).await.expect("confirm");
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn failed_commit_rolls_back_but_cancelled_commit_confirmation_recovers_saved_output() {
    for committed in [false, true] {
        let directory = tempfile::tempdir().expect("temp");
        let probe = Arc::new(Probe::default());
        let registry = registry(probe.clone());
        let counts = Arc::new(Counts::default());
        let provider: Arc<dyn ComputationIndexProvider> = Arc::new(CountProvider {
            inner: provider(directory.path()),
            counts: counts.clone(),
        });
        {
            let mut subject = open(definition(), registry.clone(), provider.clone()).await;
            if committed {
                counts.wait_after_commit.store(true, Ordering::Release);
                {
                    let processing = subject.transform(input(1, 3));
                    tokio::pin!(processing);
                    tokio::select! {
                        result = &mut processing => panic!("unexpected completion: {result:?}"),
                        _ = counts.committed.notified() => {}
                    }
                }
            } else {
                counts.fail_commit.store(true, Ordering::Release);
                assert!(subject.transform(input(1, 3)).await.is_err());
            }
            assert!(subject.transform(input(2, 3)).await.is_err());
            subject.stop().await.expect("await cleanup");
        }
        let previous_calls = probe.trace.lock().expect("trace").len();
        let mut reopened = open(definition(), registry, provider).await;
        assert_eq!(reopened.has_pending_emissions(), committed);
        let output = if committed {
            let replay = reopened.on_wakeup().await.expect("replay after commit");
            assert_eq!(probe.trace.lock().expect("trace").len(), previous_calls);
            replay
        } else {
            reopened
                .transform(input(1, 3))
                .await
                .expect("retry rolled back work")
        };
        assert_eq!(values(&output[0].envelope, "first_count"), [1]);
        assert_eq!(values(&output[0].envelope, "second_count"), [1]);
        assert_eq!(values(&output[0].envelope, "value"), [20]);
        reopened.delivery_completed(&output).await.expect("confirm");
        let next = reopened.transform(input(2, 3)).await.expect("continue");
        assert_eq!(values(&next[0].envelope, "first_count"), [2]);
        assert_eq!(values(&next[0].envelope, "second_count"), [2]);
        reopened.delivery_completed(&next).await.expect("confirm");
        reopened.stop().await.expect("stop");
    }
}

fn tracked_output_bindings(journal: u128) -> OutputBindings {
    OutputBindings::try_new([OutputDestination {
        output: port("out"),
        consumer: id("sink"),
        input: port("in"),
        journal: uuid::Uuid::from_u128(journal),
        subscriber: "sink".into(),
    }])
    .expect("output bindings")
}

#[tokio::test]
async fn output_bindings_commit_with_the_existing_owner_and_resume_without_recomputation() {
    for committed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe::default());
        let registry = registry(probe.clone());
        let counts = Arc::new(Counts::default());
        let provider: Arc<dyn ComputationIndexProvider> = Arc::new(CountProvider {
            inner: provider(directory.path()),
            counts: counts.clone(),
        });
        let bindings = tracked_output_bindings(1);
        {
            let mut subject = open(definition(), registry.clone(), provider.clone()).await;
            if committed {
                counts.wait_after_commit.store(true, Ordering::Release);
                let mut binding = Box::pin(subject.bind_output_destinations(&bindings));
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::select! {
                        _ = counts.committed.notified() => {},
                        result = &mut binding => panic!("commit did not pause: {result:?}"),
                    }
                })
                .await
                .unwrap();
                drop(binding);
            } else {
                counts.fail_commit.store(true, Ordering::Release);
                assert!(subject.bind_output_destinations(&bindings).await.is_err());
            }
            assert!(subject.transform(input(1, 3)).await.is_err());
            subject.stop().await.unwrap();
        }
        let indexes = provider
            .create_indexes("transactions", "sequence")
            .await
            .unwrap();
        let saved = indexes
            .checkpoint_store()
            .unwrap()
            .read_checkpoint("\0computation:output-bindings:v1")
            .await
            .unwrap();
        assert_eq!(saved.is_some(), committed);
        if let Some(saved) = saved {
            let record: serde_json::Value =
                serde_json::from_slice(saved.source_position.as_ref().unwrap()).unwrap();
            assert_eq!(
                record["destinations"],
                serde_json::to_value(bindings.destinations()).unwrap()
            );
        }
        let owner = drasi_core::computation::ComputationTransaction::try_new(indexes).unwrap();
        owner.shutdown().await.unwrap();
        drop(owner);
        let mut subject = open(definition(), registry.clone(), provider.clone()).await;
        subject.bind_output_destinations(&bindings).await.unwrap();
        let original = subject.transform(input(1, 3)).await.unwrap();
        let calls = probe.trace.lock().unwrap().len();
        subject.bind_output_destinations(&bindings).await.unwrap();
        assert!(subject.has_pending_emissions());
        let replay = subject.on_wakeup().await.unwrap();
        assert_eq!(probe.trace.lock().unwrap().len(), calls);
        assert_ne!(original[0].envelope.id(), replay[0].envelope.id());
        assert_eq!(values(&replay[0].envelope, "first_count"), [1]);
        subject.delivery_completed(&replay).await.unwrap();
        subject
            .bind_output_destinations(&tracked_output_bindings(2))
            .await
            .unwrap();
        subject.stop().await.unwrap();
    }
}

const BINDING_CRASH_ROOT: &str = "DRASI_OUTPUT_BINDING_CRASH_ROOT";
const BINDING_CRASH_BEFORE: &str = "DRASI_OUTPUT_BINDING_CRASH_BEFORE";

#[tokio::test]
async fn output_bindings_cannot_disappear_from_committed_progress() {
    const MEMBERSHIP: &str = "\0computation:output-bindings:v1";
    const HEAD: &str = "\0computation:middleware-head:v1";
    for corruption in [
        "missing-membership",
        "missing-fingerprint",
        "changed-membership",
        "changed-fingerprint",
        "missing-progress",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let provider = provider(directory.path());
        let registry = registry(Arc::new(Probe::default()));
        {
            let mut subject = open(definition(), registry.clone(), provider.clone()).await;
            subject
                .bind_output_destinations(&tracked_output_bindings(1))
                .await
                .unwrap();
            subject.transform(input(1, 3)).await.unwrap();
            subject.stop().await.unwrap();
        }
        let indexes = provider
            .create_indexes("transactions", "sequence")
            .await
            .unwrap();
        let checkpoint = indexes.checkpoint_store().unwrap().clone();
        let mut saved = checkpoint.read_all_checkpoints().await.unwrap();
        assert_eq!(saved[HEAD].sequence, 1);
        assert_eq!(saved[HEAD].source_position.as_ref().unwrap().len(), 32);
        match corruption {
            "missing-membership" => {
                saved.remove(MEMBERSHIP);
            }
            "missing-progress" => {
                saved.remove(HEAD);
            }
            "missing-fingerprint" => saved.get_mut(HEAD).unwrap().source_position = None,
            "changed-fingerprint" => {
                saved.get_mut(HEAD).unwrap().source_position = Some(Bytes::from_static(b"wrong"))
            }
            "changed-membership" => {
                let record = saved.get_mut(MEMBERSHIP).unwrap();
                let mut content: serde_json::Value =
                    serde_json::from_slice(record.source_position.as_ref().unwrap()).unwrap();
                content["destinations"][0]["consumer"] = "different".into();
                record.source_position = Some(serde_json::to_vec(&content).unwrap().into());
            }
            _ => unreachable!(),
        }
        let owner = drasi_core::computation::ComputationTransaction::try_new(indexes).unwrap();
        owner
            .run(async {
                checkpoint.clear_checkpoints().await?;
                for (key, record) in &saved {
                    checkpoint
                        .stage_checkpoint(key, record.sequence, record.source_position.as_ref())
                        .await?;
                }
                Ok(())
            })
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
        drop((owner, checkpoint));
        let mut subject = TransactionTransformer::new(definition(), registry, provider.clone())
            .await
            .unwrap();
        let error = subject.start().await.expect_err(corruption);
        assert!(
            error.chain().any(|cause| cause.is::<OutputBindingError>()),
            "{corruption}: {error:#}"
        );
        subject.stop().await.unwrap();
        drop(subject);
        let indexes = provider
            .create_indexes("transactions", "sequence")
            .await
            .unwrap();
        let after = indexes
            .checkpoint_store()
            .unwrap()
            .read_all_checkpoints()
            .await
            .unwrap();
        assert_eq!(after.len(), saved.len(), "{corruption}");
        for (key, record) in saved {
            assert_eq!(after[&key].sequence, record.sequence, "{corruption}: {key}");
            assert_eq!(
                after[&key].source_position, record.source_position,
                "{corruption}: {key}"
            );
        }
        let owner = drasi_core::computation::ComputationTransaction::try_new(indexes).unwrap();
        owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "invoked by output_bindings_survive_process_exit_with_pending_output"]
async fn output_bindings_crash_child() {
    let directory = std::path::PathBuf::from(std::env::var(BINDING_CRASH_ROOT).unwrap());
    let before = std::env::var(BINDING_CRASH_BEFORE).unwrap() == "true";
    let counts = Arc::new(Counts::default());
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(CountProvider {
        inner: provider(&directory),
        counts: counts.clone(),
    });
    let mut subject = open(definition(), registry(Arc::new(Probe::default())), provider).await;
    subject
        .bind_output_destinations(&tracked_output_bindings(1))
        .await
        .unwrap();
    if before {
        counts.wait_before_commit.store(true, Ordering::Release);
    } else {
        counts.wait_after_commit.store(true, Ordering::Release);
    }
    let mut processing = Box::pin(subject.transform(input(1, 3)));
    let reached = if before {
        &counts.committing
    } else {
        &counts.committed
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            _ = reached.notified() => {},
            result = &mut processing => panic!("commit did not pause: {result:?}"),
        }
    })
    .await
    .unwrap();
    std::process::exit(75);
}

#[tokio::test]
async fn output_bindings_survive_process_exit_with_pending_output() {
    for before in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "output_bindings_crash_child", "--ignored", "--nocapture"])
            .env(BINDING_CRASH_ROOT, directory.path())
            .env(BINDING_CRASH_BEFORE, before.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("output-binding crash child timed out: {output:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(75), "{output:?}");
        let provider = provider(directory.path());
        let probe = Arc::new(Probe::default());
        let registry = registry(probe.clone());
        let mut subject = open(definition(), registry, provider).await;
        assert_eq!(subject.has_pending_emissions(), !before);
        subject
            .bind_output_destinations(&tracked_output_bindings(1))
            .await
            .unwrap();
        let replay = if before {
            subject.transform(input(1, 3)).await.unwrap()
        } else {
            let replay = subject.on_wakeup().await.unwrap();
            assert!(probe.trace.lock().unwrap().is_empty());
            replay
        };
        assert_eq!(values(&replay[0].envelope, "first_count"), [1]);
        assert_eq!(values(&replay[0].envelope, "second_count"), [1]);
        subject.delivery_completed(&replay).await.unwrap();
        subject
            .bind_output_destinations(&tracked_output_bindings(2))
            .await
            .unwrap();
        subject.stop().await.unwrap();
    }
}

#[tokio::test]
async fn expansion_and_filtering_stay_inside_the_same_transaction_batch() {
    for filter in [false, true] {
        let directory = tempfile::tempdir().expect("temp");
        let probe = Arc::new(Probe::default());
        let mut definition = definition();
        definition.steps[0].configuration = json!({"add":2,"expand":true});
        definition.steps[1].configuration = json!({"multiply":4,"filter":filter});
        let mut subject = open(
            definition,
            registry(probe.clone()),
            provider(directory.path()),
        )
        .await;
        let output = subject.transform(input(1, 3)).await.expect("transform");
        assert_eq!(output.len(), 1);
        assert_eq!(
            output[0].envelope.changes().operations().len(),
            if filter { 0 } else { 2 }
        );
        let trace = probe.trace.lock().expect("trace").clone();
        assert_eq!(
            trace,
            [("first".into(), 5, 1), ("second".into(), 20, 1), ("second".into(), 20, 1)]
        );
        subject.delivery_completed(&output).await.expect("confirm");
        subject.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn changed_configuration_and_unconfirmed_retention_are_not_silently_accepted() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let registry = registry(Arc::new(Probe::default()));
    let mut configured = definition();
    configured.outbox_capacity = size(1);
    {
        let mut subject = open(configured.clone(), registry.clone(), provider.clone()).await;
        let first = subject.transform(input(1, 3)).await.expect("first");
        let error = subject.transform(input(2, 4)).await.expect_err("retention");
        assert!(matches!(
            error.downcast_ref::<MiddlewareRecoveryError>(),
            Some(MiddlewareRecoveryError::RetentionExhausted)
        ));
        subject.delivery_completed(&first).await.expect("confirm");
        let next = subject.transform(input(2, 4)).await.expect("retry");
        assert_eq!(
            values(&next[0].envelope, "first_count"),
            [2],
            "rejected input made no state changes"
        );
        subject.delivery_completed(&next).await.expect("confirm");
        subject.stop().await.expect("stop");
    }
    configured.steps.swap(0, 1);
    let mut changed = TransactionTransformer::new(configured, registry, provider)
        .await
        .expect("construct");
    let error = changed.start().await.expect_err("different sequence");
    assert!(
        format!("{error:#}").contains("configuration or ownership changed"),
        "{error:#}"
    );
    changed.stop().await.expect("cleanup");
}

#[tokio::test]
async fn construction_rejects_empty_duplicate_unregistered_and_non_atomic_definitions() {
    let registry = registry(Arc::new(Probe::default()));
    let valid = definition();
    let mut empty = valid.clone();
    empty.steps.clear();
    assert!(empty.descriptor(&registry).is_err());
    let mut duplicate = valid.clone();
    duplicate.steps[1].id = duplicate.steps[0].id.clone();
    assert!(duplicate.descriptor(&registry).is_err());
    let mut unknown = valid.clone();
    unknown.steps[0].implementation =
        ImplementationIdentity::try_new("source/not-a-transactional-transformer", "1")
            .expect("identity");
    assert!(unknown.descriptor(&registry).is_err());
    let mut version = valid.clone();
    version.steps[0].configuration_version = 2;
    assert!(version.descriptor(&registry).is_err());
    assert!(TransactionTransformer::new(
        valid,
        registry,
        Arc::new(drasi_core::computation::InMemoryComputationProvider)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn participating_implementation_remains_usable_as_an_ordinary_transformer() {
    let probe = Arc::new(Probe::default());
    let factory = ArithmeticFactory(probe.clone());
    let mut ordinary = factory
        .create(&step("ordinary", json!({"add":2})))
        .expect("construct");
    ordinary.start().await.expect("start");
    let output = ordinary.transform(input(1, 3)).await.expect("standalone");
    assert_eq!(values(&output[0].envelope, "value"), [5]);
    assert_eq!(probe.standalone_calls.load(Ordering::SeqCst), 1);
    ordinary.stop().await.expect("stop");
}

struct FiniteSource {
    descriptor: ComponentDescriptor,
    inputs: VecDeque<ChangeEnvelope>,
}

struct JournalSource {
    descriptor: ComponentDescriptor,
    store: Arc<IndexedEnvelopeStore>,
    progress: Arc<QuerySourceProgress>,
    position: u64,
}

#[async_trait]
impl ComputationComponent for JournalSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::admitted(self.store.durability())
            .replay_until(self.progress.component_id().clone())
    }
    async fn start(&mut self) -> TestResult<()> {
        self.position = 0;
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for JournalSource {
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        Some(self.progress.clone())
    }
    async fn next(&mut self) -> TestResult<Option<OutputEnvelope>> {
        let Some(stored) = self
            .store
            .next(self.store.generation(), self.position)
            .await?
        else {
            return Ok(None);
        };
        self.position = stored.position;
        Ok(Some(OutputEnvelope {
            port: port("out"),
            envelope: stored.envelope,
        }))
    }
}

struct ReplayRelay {
    descriptor: ComponentDescriptor,
    stateless: bool,
}

#[async_trait]
impl ComputationComponent for ReplayRelay {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        if self.stateless {
            ComponentRecovery::stateless()
        } else {
            ComponentRecovery::default()
        }
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for ReplayRelay {
    async fn transform(&mut self, input: InputEnvelope) -> TestResult<Vec<OutputEnvelope>> {
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: GraphChangeCodec::derive_changes(
                &input.envelope,
                &GraphChangeCodec::decode_changes(&input.envelope)?,
                stream("relay/out"),
                input.envelope.system().sequence(),
            )?,
        }])
    }
}

async fn replay_contract_builder(
    provider: Arc<dyn ComputationIndexProvider>,
    store: Arc<IndexedEnvelopeStore>,
    progress: Arc<QuerySourceProgress>,
    stateless: bool,
    mismatched_progress: bool,
    probe: Arc<Probe>,
    output: Arc<Mutex<Vec<ChangeEnvelope>>>,
) -> GraphResult<ComputationGraphBuilder> {
    let delivery_id = ResourceId::try_new("delivery")?;
    let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    let delivery = Arc::new(
        IndexedEnvelopeStore::try_new(
            provider
                .create_indexes("transactions", "delivery")
                .await
                .expect("delivery indexes"),
            Arc::new(codec),
            "delivered-output",
            size(2),
            RetentionPolicy::Backpressure,
        )
        .expect("delivery store"),
    );
    let owner = TransactionTransformer::new(definition(), registry(probe), provider)
        .await
        .expect("real transaction")
        .with_source_progress(progress.clone())
        .expect("progress owner");
    let supplied = if mismatched_progress {
        Arc::new(QuerySourceProgress::new("transactions", id("sequence")).expect("same names"))
    } else {
        progress
    };
    let requirement = RecoveryRequirement {
        consumer: id("sequence"),
        scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
        guarantees: [RecoveryGuarantee::Replay, RecoveryGuarantee::CommittedProcessing].into(),
    };
    Ok(ComputationGraph::builder("transactions")
        .declare_resource(ResourceSpecification {
            id: delivery_id.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "delivery".into(),
        })?
        .provide_resource(
            delivery_id.clone(),
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(RetainedStoreResource(delivery.clone())),
            )
            .with_cleanup(delivery),
        )?
        .source(Box::new(JournalSource {
            descriptor: ComponentDescriptor::try_new(
                id("source"),
                vec![PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source descriptor"),
            store,
            progress: supplied,
            position: 0,
        }))
        .transformer(Box::new(ReplayRelay {
            descriptor: descriptor(id("relay"), false),
            stateless,
        }))
        .transformer(Box::new(owner))
        .sink(Box::new(Collector {
            descriptor: ComponentDescriptor::try_new(
                id("sink"),
                vec![PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("sink descriptor"),
            output,
        }))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .bind_stream(endpoint("relay", "out"), stream("relay/out"))
        .bind_stream(endpoint("sequence", "out"), stream("sequence/out"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("relay", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("relay", "out"), endpoint("sequence", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("sequence", "out"), endpoint("sink", "in")),
            Box::new(RetainedPipeConfig {
                resource: delivery_id,
                capacity: size(2),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                gap_policy: ReplayGapPolicy::Strict,
            }),
        )
        .require_recovery(requirement))
}

async fn reopen_through_stateless_relay() {
    let directory = tempfile::tempdir().expect("temp");
    let probe = Arc::new(Probe::default());
    let mut committed_change = None;
    for iteration in 0..2 {
        let provider = provider(directory.path());
        let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
        codec
            .register_schema(GraphChangeCodec::schema())
            .expect("schema");
        let store = Arc::new(
            IndexedEnvelopeStore::try_new(
                provider
                    .create_indexes("transactions", "admission")
                    .await
                    .expect("admission indexes"),
                Arc::new(codec),
                "accepted-input",
                size(1),
                RetentionPolicy::Backpressure,
            )
            .expect("journal"),
        );
        let generation = store.acquire_generation().expect("journal lease");
        if iteration == 0 {
            store
                .append(generation, &input(1, 3).envelope)
                .await
                .expect("durable admission");
        }
        let progress =
            Arc::new(QuerySourceProgress::new("transactions", id("sequence")).expect("progress"));
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut graph = replay_contract_builder(
            provider.clone(),
            store.clone(),
            progress.clone(),
            true,
            false,
            probe.clone(),
            output.clone(),
        )
        .await
        .expect("builder")
        .build()
        .expect("verified stateless path");
        let publication = graph
            .recovery_report(&RecoveryRequirement {
                consumer: id("sequence"),
                scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
                guarantees: [RecoveryGuarantee::Publication].into(),
            })
            .expect("publication assessment");
        assert!(
            publication.satisfied(),
            "committed output has durable delivery"
        );
        graph
            .start()
            .expect("scope")
            .await
            .expect("processed graph");
        assert_eq!(
            probe.trace.lock().expect("trace").len(),
            2,
            "committed steps are not repeated"
        );
        {
            let output = output.lock().expect("output");
            assert_eq!(output.len(), 1);
            assert_eq!(values(&output[0], "value"), [20]);
            assert_eq!(
                GraphProducerProgress::from_envelope(&output[0])
                    .expect("logical output")
                    .expect("producer")
                    .sequence(),
                1
            );
            if let Some(original) = &committed_change {
                assert_eq!(
                    output[0].changes().id(),
                    original,
                    "replay reuses committed output"
                );
            } else {
                committed_change = Some(output[0].changes().id().clone());
            }
        }
        assert_eq!(
            progress.snapshot().checkpoints[&SourceProgressKey::Stream(stream("relay/out"))]
                .sequence,
            1,
        );
        assert!(store
            .next(generation, 0)
            .await
            .expect("retained input")
            .is_some());
        graph.dispose().await.expect("dispose");
        drop(graph);
        store.shutdown().await.expect("release admission indexes");
        drop(store);
        drop(provider);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_atomic_progress_covers_a_stateless_relay_current_thread() {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        reopen_through_stateless_relay(),
    )
    .await
    .expect("replay path stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_atomic_progress_covers_a_stateless_relay_multi_thread() {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        reopen_through_stateless_relay(),
    )
    .await
    .expect("replay path stalled");
}

#[tokio::test]
async fn publication_assertion_rejects_unbinding_its_output_before_mutation() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    let store = Arc::new(
        IndexedEnvelopeStore::try_new(
            provider
                .create_indexes("transactions", "admission")
                .await
                .expect("admission indexes"),
            Arc::new(codec),
            "accepted-input",
            size(1),
            RetentionPolicy::Backpressure,
        )
        .expect("journal"),
    );
    let publication = RecoveryRequirement {
        consumer: id("sequence"),
        scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
        guarantees: [RecoveryGuarantee::Publication].into(),
    };
    let output = EdgeDefinition::new(endpoint("sequence", "out"), endpoint("sink", "in"));
    let mut graph = replay_contract_builder(
        provider.clone(),
        store.clone(),
        Arc::new(QuerySourceProgress::new("transactions", id("sequence")).expect("progress")),
        true,
        false,
        Arc::new(Probe::default()),
        Arc::new(Mutex::new(Vec::new())),
    )
    .await
    .expect("builder")
    .relationship_policy(
        output.clone(),
        RelationshipPolicy {
            required_for_binding: false,
            orphan_permitted: true,
            ..RelationshipPolicy::default()
        },
    )
    .require_recovery(publication.clone())
    .build()
    .expect("verified publication");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(run, async {
            control.deployment_report().await.expect("deployment");
            assert!(control
                .recovery_report(publication)
                .await
                .expect("publication")
                .satisfied());
            let revision = control.desired_snapshot().revision;
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::Unbind {
                        edge: output.clone(),
                        policy: RemovalPolicy::Orphan,
                    }],
                )
                .await
                .expect("orphaning is structurally permitted");
            let error = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect_err("publication must prevent losing its delivery boundary");
            let GraphError::Recovery(error) = error else {
                panic!("expected a typed recovery rejection: {error}");
            };
            assert!(error.issues.iter().any(|issue| {
                issue.participant == RecoveryParticipant::Connection(output.clone())
                    && issue.reason == RecoveryIncompatibility::MissingPublication
            }));
            assert_eq!(control.desired_snapshot().revision, revision);
            assert!(control
                .desired_snapshot()
                .edges
                .iter()
                .any(|edge| edge.definition == output));
            control.cancel();
        })
    })
    .await
    .expect("publication reconfiguration stalled");
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
    store.shutdown().await.expect("close admission indexes");
}

#[tokio::test]
async fn stateful_intermediates_and_matching_progress_names_do_not_prove_recovery() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    for (stateless, mismatch, diamond, expected) in [
        (
            false,
            false,
            false,
            RecoveryIncompatibility::UnknownProcessing,
        ),
        (
            true,
            true,
            false,
            RecoveryIncompatibility::MismatchedProgressResource,
        ),
        (
            true,
            false,
            true,
            RecoveryIncompatibility::AmbiguousReplayPath,
        ),
    ] {
        let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
        codec
            .register_schema(GraphChangeCodec::schema())
            .expect("schema");
        let store = Arc::new(
            IndexedEnvelopeStore::try_new(
                provider
                    .create_indexes("transactions", "admission")
                    .await
                    .expect("indexes"),
                Arc::new(codec),
                "accepted-input",
                size(1),
                RetentionPolicy::Backpressure,
            )
            .expect("journal"),
        );
        let progress =
            Arc::new(QuerySourceProgress::new("transactions", id("sequence")).expect("progress"));
        let mut builder = replay_contract_builder(
            provider.clone(),
            store.clone(),
            progress,
            stateless,
            mismatch,
            Arc::new(Probe::default()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .await
        .expect("builder");
        if diamond {
            builder = builder.connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("sequence", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        }
        let Err(GraphError::Recovery(error)) = builder.build() else {
            panic!("invalid progress path must be rejected");
        };
        assert!(error.issues.iter().any(|issue| issue.reason == expected));
        store.shutdown().await.expect("release journal");
    }
}

#[tokio::test]
async fn another_branch_cannot_borrow_the_transaction_owners_checkpoint() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    let store = Arc::new(
        IndexedEnvelopeStore::try_new(
            provider
                .create_indexes("transactions", "admission")
                .await
                .expect("indexes"),
            Arc::new(codec),
            "accepted-input",
            size(1),
            RetentionPolicy::Backpressure,
        )
        .expect("journal"),
    );
    let progress =
        Arc::new(QuerySourceProgress::new("transactions", id("sequence")).expect("progress"));
    let direct = EdgeDefinition::new(endpoint("source", "out"), endpoint("sink", "in"));
    let mut graph = replay_contract_builder(
        provider.clone(),
        store.clone(),
        progress,
        true,
        false,
        Arc::new(Probe::default()),
        Arc::new(Mutex::new(Vec::new())),
    )
    .await
    .expect("builder")
    .connect(direct.clone(), Box::new(BoundedPipeConfig { capacity: 1 }))
    .build()
    .expect("transaction branch remains supported");
    let report = graph
        .recovery_report(&RecoveryRequirement {
            consumer: id("sink"),
            scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
            guarantees: [RecoveryGuarantee::Replay].into(),
        })
        .expect("combined path");
    assert!(report.issues.iter().any(|issue| {
        issue.participant == RecoveryParticipant::Connection(direct.clone())
            && issue.reason == RecoveryIncompatibility::MissingReplayBoundary
    }));
    graph.dispose().await.expect("dispose");
    drop(graph);
    store.shutdown().await.expect("journal cleanup");
}
#[async_trait]
impl ComputationComponent for FiniteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for FiniteSource {
    async fn next(&mut self) -> TestResult<Option<OutputEnvelope>> {
        Ok(self.inputs.pop_front().map(|envelope| OutputEnvelope {
            port: port("out"),
            envelope,
        }))
    }
}
struct Collector {
    descriptor: ComponentDescriptor,
    output: Arc<Mutex<Vec<ChangeEnvelope>>>,
}
#[async_trait]
impl ComputationComponent for Collector {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Collector {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> TestResult<()> {
        self.output.lock().expect("output").push(input.envelope);
        Ok(())
    }
}

fn endpoint(component: &str, name: &str) -> Endpoint {
    Endpoint::new(id(component), port(name))
}

#[tokio::test]
async fn factory_constructs_one_graph_node_and_durable_external_delivery_confirms_the_batch() {
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let registry = registry(Arc::new(Probe::default()));
    let definition = definition();
    let indexes = ResourceId::try_new("indexes").expect("resource");
    let transformers = ResourceId::try_new("transformers").expect("resource");
    let store_id = ResourceId::try_new("delivery").expect("resource");
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    let delivery_indexes = provider
        .create_indexes("transactions", "delivery")
        .await
        .expect("delivery indexes");
    let store = Arc::new(
        IndexedEnvelopeStore::try_new(
            delivery_indexes,
            Arc::new(codec),
            "transaction-delivery",
            size(4),
            RetentionPolicy::Backpressure,
        )
        .expect("store"),
    );
    let mut graph = ComputationGraph::builder("transactions")
        .declare_resource(ResourceSpecification {
            id: indexes.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: "indexes".into(),
        })
        .expect("declare")
        .provide_resource(
            indexes.clone(),
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(provider.clone())),
            ),
        )
        .expect("provider")
        .declare_resource(ResourceSpecification {
            id: transformers.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Borrowed,
            binding: "transformers".into(),
        })
        .expect("declare")
        .provide_resource(
            transformers.clone(),
            ResourceHandle::new(
                ResourceRole::Component,
                Arc::new(TransactionalTransformerRegistryResource(registry.clone())),
            ),
        )
        .expect("registry")
        .declare_resource(ResourceSpecification {
            id: store_id.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "delivery".into(),
        })
        .expect("declare")
        .provide_resource(
            store_id.clone(),
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(RetainedStoreResource(store.clone())),
            ),
        )
        .expect("store resource")
        .source(Box::new(FiniteSource {
            descriptor: ComponentDescriptor::try_new(
                id("source"),
                vec![PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("descriptor"),
            inputs: VecDeque::from([input(1, 3).envelope]),
        }))
        .component(
            definition
                .specification(&registry, transformers, indexes)
                .expect("spec"),
            Arc::new(TransactionTransformerFactory::default()),
        )
        .sink(Box::new(Collector {
            descriptor: ComponentDescriptor::try_new(
                id("collector"),
                vec![PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("descriptor"),
            output: output.clone(),
        }))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .bind_stream(endpoint("sequence", "out"), stream("sequence/out"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("sequence", "in")),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("sequence", "out"), endpoint("collector", "in")),
            Box::new(RetainedPipeConfig {
                resource: store_id,
                capacity: size(4),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                gap_policy: ReplayGapPolicy::Strict,
            }),
        )
        .build()
        .expect("graph");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        graph.start().expect("start"),
    )
    .await
    .expect("timeout")
    .expect("run");
    assert_eq!(output.lock().expect("output").len(), 1);
    assert_eq!(values(&output.lock().expect("output")[0], "value"), [20]);
    graph.dispose().await.expect("dispose");
    drop(graph);
    store.shutdown().await.expect("delivery shutdown");
    drop(store);
    let mut reopened = open(definition, registry, provider).await;
    assert!(
        !reopened.has_pending_emissions(),
        "graph confirmed every external durable branch"
    );
    reopened.stop().await.expect("stop");
}

#[cfg(any(feature = "middleware-unwind", feature = "middleware-all"))]
#[tokio::test]
async fn builtin_middleware_participates_and_unwind_state_survives_reconstruction() {
    use drasi_core::middleware::MiddlewareTypeRegistry;
    let directory = tempfile::tempdir().expect("temp");
    let provider = provider(directory.path());
    let mut middleware = MiddlewareTypeRegistry::new();
    middleware.register(Arc::new(drasi_middleware::unwind::UnwindFactory::new()));
    let registry = Arc::new(TransactionalTransformerRegistry::standard(Arc::new(
        middleware,
    )));
    let mut definition = definition();
    definition.steps = vec![TransactionStepDefinition {
        id: id("expand"),
        implementation: ImplementationIdentity::try_new("drasi/middleware-transformer", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: json!({
            "middleware":[{"kind":"unwind","name":"children","config":{
                "Parent":[{"selector":"$.items[*]","label":"Child","key":"$.id"}]
            }}],
            "pipeline":["children"]
        }),
    }];
    let parent_input = |sequence: u64, update| InputEnvelope {
        port: port("in"),
        envelope: GraphChangeCodec::encode_change(
            {
                let element = Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "parent"),
                        labels: Arc::from([Arc::from("Parent")]),
                        effective_from: sequence,
                    },
                    properties: ElementPropertyMap::from(if update {
                        json!({"items":[{"id":"a"}]})
                    } else {
                        json!({"items":[{"id":"a"},{"id":"b"}]})
                    }),
                };
                if update {
                    SourceChange::Update { element }
                } else {
                    SourceChange::Insert { element }
                }
            },
            stream("source/out"),
            sequence,
            None,
        )
        .expect("input"),
    };
    {
        let mut subject = open(definition.clone(), registry.clone(), provider.clone()).await;
        let first = subject
            .transform(parent_input(1, false))
            .await
            .expect("insert");
        assert_eq!(
            GraphChangeCodec::decode_changes(&first[0].envelope)
                .expect("decode")
                .len(),
            3
        );
        subject.delivery_completed(&first).await.expect("confirm");
        subject.stop().await.expect("stop");
    }
    let mut subject = open(definition, registry, provider).await;
    let output = subject
        .transform(parent_input(2, true))
        .await
        .expect("update");
    let changes = GraphChangeCodec::decode_changes(&output[0].envelope).expect("decode");
    assert_eq!(changes.len(), 3);
    assert!(changes.iter().any(|change| matches!(
        change, SourceChange::Delete { metadata } if metadata.reference.element_id.ends_with("-b")
    )), "previous child b must be deleted after restart");
    subject.delivery_completed(&output).await.expect("confirm");
    subject.stop().await.expect("stop");
}

#[cfg(all(
    any(feature = "middleware-decoder", feature = "middleware-all"),
    any(feature = "middleware-parse-json", feature = "middleware-all"),
))]
#[tokio::test]
async fn documented_decoder_then_parser_configuration_runs_as_two_transactional_steps() {
    let mut middleware = drasi_core::middleware::MiddlewareTypeRegistry::new();
    middleware.register(Arc::new(drasi_middleware::decoder::DecoderFactory::new()));
    middleware.register(Arc::new(
        drasi_middleware::parse_json::ParseJsonFactory::new(),
    ));
    let registry = Arc::new(TransactionalTransformerRegistry::standard(Arc::new(
        middleware,
    )));
    let mut configured = definition();
    configured.steps = [
        (
            "decode",
            "decoder",
            json!({
                "encoding_type":"base64", "target_property":"encoded",
                "output_property":"decoded", "on_error":"fail",
            }),
        ),
        (
            "parse",
            "parse_json",
            json!({
                "target_property":"decoded", "output_property":"parsed", "on_error":"fail",
            }),
        ),
    ]
    .into_iter()
    .map(|(name, kind, config)| TransactionStepDefinition {
        id: id(name),
        implementation: ImplementationIdentity::try_new("drasi/middleware-transformer", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: json!({
            "middleware":[{"name":"step","kind":kind,"config":config}],
            "pipeline":["step"],
        }),
    })
    .collect();
    let directory = tempfile::tempdir().expect("temp");
    let mut subject = open(configured, registry, provider(directory.path())).await;
    let envelope = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "encoded"),
                    labels: Arc::from([]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(json!({"encoded":"eyJ2YWx1ZSI6NDJ9"})),
            },
        },
        stream("source/out"),
        1,
        None,
    )
    .expect("input");
    let output = subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope,
        })
        .await
        .expect("both steps");
    let changes = GraphChangeCodec::decode_changes(&output[0].envelope).expect("decode");
    let SourceChange::Insert { element } = &changes[0] else {
        panic!("insert")
    };
    assert_eq!(
        element.get_property("parsed"),
        &ElementValue::Object(ElementPropertyMap::from(json!({"value":42})))
    );
    subject.delivery_completed(&output).await.expect("confirm");
    subject.stop().await.expect("stop");
}

struct NumberValidator;
impl RecordValidator for NumberValidator {
    fn validate_identity(
        &self,
        _: &SchemaDescriptor,
        identity: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        if identity.namespace() != "numbers" || identity.value().as_ref() != b"one" {
            return Err(RecordValidationError::new(
                "identity",
                "expected number identity",
            ));
        }
        Ok(())
    }
    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> std::result::Result<(), RecordValidationError> {
        self.validate_identity(schema, identity)?;
        if image != RecordImage::Full || payload.len() != 8 {
            return Err(RecordValidationError::new(
                "number",
                "expected full eight-byte number",
            ));
        }
        Ok(())
    }
}
fn number_schema() -> Arc<Schema> {
    Arc::new(Schema::new(
        SchemaDescriptor::try_new(
            SchemaId::try_new("test.number").expect("id"),
            SchemaVersion::try_new(1).expect("version"),
            "u64be",
            Bytes::from_static(b"eight-byte-number"),
        )
        .expect("descriptor"),
        Arc::new(NumberValidator),
    ))
}
fn number_event(value: u64, sequence: u64) -> ChangeEnvelope {
    let schema = number_schema();
    ChangeEnvelope::new(
        emission_id(&stream("numbers/out"), sequence).expect("id"),
        ChangeSet::try_new(
            ChangeSetId::try_new("numbers", Bytes::copy_from_slice(&sequence.to_be_bytes()))
                .expect("changes"),
            schema.descriptor().clone(),
            vec![ChangeOperation::Added {
                ordinal: 0,
                after: Record::try_new(
                    &schema,
                    RecordId::try_new("numbers", Bytes::from_static(b"one")).expect("record"),
                    RecordImage::Full,
                    Bytes::copy_from_slice(&value.to_be_bytes()),
                )
                .expect("value"),
            }],
        )
        .expect("changes"),
        SystemMetadata::new(stream("numbers/out"), sequence),
    )
}
struct NumberStep {
    descriptor: ComponentDescriptor,
}
#[async_trait]
impl ComputationComponent for NumberStep {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl Transformer for NumberStep {
    async fn transform(&mut self, input: InputEnvelope) -> TestResult<Vec<OutputEnvelope>> {
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: input.envelope,
        }])
    }
}
#[async_trait]
impl TransactionalTransformer for NumberStep {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        number_schema()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        number_schema()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> TestResult<ChangeEnvelope> {
        let ChangeOperation::Added { after, .. } = &input.changes().operations()[0] else {
            anyhow::bail!("number")
        };
        let value = u64::from_be_bytes(after.payload().as_ref().try_into()?) + 1;
        context
            .put("seen", ElementValue::Integer(value as i64))
            .await?;
        let next = number_event(value, input.system().sequence());
        Ok(input.derive(
            next.id().clone(),
            next.changes().clone(),
            next.system().as_ref().clone(),
        ))
    }
}
struct NumberFactory {
    extra_port: bool,
}
impl TransactionalTransformerFactory for NumberFactory {
    fn implementation(&self) -> ImplementationIdentity {
        ImplementationIdentity::try_new("test/number", "1").expect("implementation")
    }
    fn configuration_version(&self) -> u32 {
        1
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> TestResult<Box<dyn TransactionalTransformer>> {
        let schema = number_schema();
        let mut ports = vec![
            PortDescriptor::new(
                port("in"),
                PortDirection::Input,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            ),
            PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            ),
        ];
        if self.extra_port {
            ports.push(PortDescriptor::new(
                port("branch"),
                PortDirection::Output,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            ));
        }
        Ok(Box::new(NumberStep {
            descriptor: ComponentDescriptor::try_new(definition.id.clone(), ports)?,
        }))
    }
}

struct QueryRows {
    descriptor: ComponentDescriptor,
}
#[async_trait]
impl ComputationComponent for QueryRows {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl Transformer for QueryRows {
    async fn transform(&mut self, input: InputEnvelope) -> TestResult<Vec<OutputEnvelope>> {
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: input.envelope,
        }])
    }
}
#[async_trait]
impl TransactionalTransformer for QueryRows {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        QueryChangeCodec::schema()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        QueryChangeCodec::schema()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> TestResult<ChangeEnvelope> {
        let count = match context.get("calls").await? {
            None => 1,
            Some(ElementValue::Integer(value)) => value + 1,
            _ => anyhow::bail!("invalid state"),
        };
        context.put("calls", ElementValue::Integer(count)).await?;
        Ok(input)
    }
}
struct QueryRowsFactory;
impl TransactionalTransformerFactory for QueryRowsFactory {
    fn implementation(&self) -> ImplementationIdentity {
        ImplementationIdentity::try_new("test/query-rows", "1").expect("implementation")
    }
    fn configuration_version(&self) -> u32 {
        1
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> TestResult<Box<dyn TransactionalTransformer>> {
        let schema = QueryChangeCodec::schema();
        Ok(Box::new(QueryRows {
            descriptor: ComponentDescriptor::try_new(
                definition.id.clone(),
                vec![
                    PortDescriptor::new(
                        port("in"),
                        PortDirection::Input,
                        schema.descriptor().clone(),
                        PipeRequirements::default(),
                    ),
                    PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        schema.descriptor().clone(),
                        PipeRequirements::default(),
                    ),
                ],
            )?,
        }))
    }
}

#[tokio::test]
async fn query_inputs_use_query_identity_and_position_and_can_feed_another_transaction() {
    let directory = tempfile::tempdir().expect("temp");
    let storage = provider(directory.path());
    let query_definition = ContinuousQueryDefinition {
        graph_id: "upstream".into(),
        id: id("query"),
        query: "MATCH (n:Item) RETURN n.value AS value".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream("query/out"),
        outbox_capacity: size(8),
    };
    let mut query = ContinuousQueryTransformer::new(query_definition.clone(), storage.clone())
        .await
        .expect("query");
    query.start().await.expect("start query");
    let mut input_from_query = query.transform(input(1, 3)).await.expect("query output");
    let input_from_query = input_from_query.remove(0).envelope;
    let mut registry = TransactionalTransformerRegistry::default();
    registry
        .register(Arc::new(QueryRowsFactory))
        .expect("register");
    let registry = Arc::new(registry);
    let mut configured = definition();
    configured.steps = vec![TransactionStepDefinition {
        id: id("rows"),
        implementation: ImplementationIdentity::try_new("test/query-rows", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: json!({}),
    }];
    let mut first = open(configured.clone(), registry.clone(), storage.clone()).await;
    configured.id = id("second-container");
    configured.output_stream = stream("second-container/out");
    let mut second = open(configured.clone(), registry.clone(), storage.clone()).await;
    let first_output = first
        .transform(InputEnvelope {
            port: port("in"),
            envelope: input_from_query.clone(),
        })
        .await
        .expect("first");
    let second_output = second
        .transform(InputEnvelope {
            port: port("in"),
            envelope: first_output[0].envelope.clone(),
        })
        .await
        .expect("second");
    assert_eq!(
        first_output[0].envelope.changes().operations(),
        second_output[0].envelope.changes().operations()
    );
    first
        .delivery_completed(&first_output)
        .await
        .expect("confirm");
    second
        .delivery_completed(&second_output)
        .await
        .expect("confirm");

    // A transport re-emission is still the same query result.
    let repeated = input_from_query.derive(
        emission_id(&stream("query/out"), 99).expect("emission"),
        input_from_query.changes().clone(),
        SystemMetadata::new(stream("query/out"), 99),
    );
    let replay = first
        .transform(InputEnvelope {
            port: port("in"),
            envelope: repeated,
        })
        .await
        .expect("duplicate");
    assert_eq!(
        GraphProducerProgress::from_envelope(&replay[0].envelope)
            .expect("progress")
            .expect("present")
            .sequence(),
        1
    );
    first.delivery_completed(&replay).await.expect("confirm");

    let second_query_output = query.transform(input(2, 4)).await.expect("second result");
    let third_query_output = query.transform(input(3, 5)).await.expect("third result");
    let mut gap_definition = configured.clone();
    gap_definition.id = id("detect-gap");
    gap_definition.output_stream = stream("detect-gap/out");
    {
        let mut gap = open(gap_definition.clone(), registry.clone(), storage.clone()).await;
        let output = gap
            .transform(InputEnvelope {
                port: port("in"),
                envelope: input_from_query.clone(),
            })
            .await
            .expect("first result");
        gap.delivery_completed(&output).await.expect("confirm");
        let error = gap
            .transform(InputEnvelope {
                port: port("in"),
                envelope: third_query_output[0].envelope.clone(),
            })
            .await
            .expect_err("do not skip missing query result");
        assert!(format!("{error:#}").contains("missing result"), "{error:#}");
        gap.stop().await.expect("cleanup gap");
    }
    let mut gap = open(gap_definition, registry.clone(), storage.clone()).await;
    let output = gap
        .transform(InputEnvelope {
            port: port("in"),
            envelope: second_query_output[0].envelope.clone(),
        })
        .await
        .expect("missing result recovered without cursor advancement");
    assert_eq!(
        GraphProducerProgress::from_envelope(&output[0].envelope)
            .expect("progress")
            .expect("present")
            .sequence(),
        2
    );
    gap.delivery_completed(&output).await.expect("confirm");
    gap.stop().await.expect("stop");

    let mut reset = input_from_query.clone();
    reset
        .append_annotation(
            ContextEntry::try_new(
                id("query"),
                "drasi.query-generation.v1",
                ContextValue::Unsigned(1),
            )
            .expect("annotation"),
        )
        .expect("append");
    assert!(
        first
            .transform(InputEnvelope {
                port: port("in"),
                envelope: reset
            })
            .await
            .is_err(),
        "a reset cannot reuse the previous input owner"
    );
    first.stop().await.expect("cleanup");
    second.stop().await.expect("stop");
    query.stop().await.expect("stop query");

    let mut volatile = ContinuousQueryTransformer::new(
        query_definition,
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .expect("volatile");
    volatile.start().await.expect("start");
    let output = volatile.transform(input(1, 3)).await.expect("query output");
    configured.id = id("reject-volatile");
    configured.output_stream = stream("reject-volatile/out");
    let mut reject = open(configured, registry, storage).await;
    let error = reject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: output[0].envelope.clone(),
        })
        .await
        .expect_err("volatile boundary");
    assert!(format!("{error:#}").contains("volatile query"), "{error:#}");
    reject.stop().await.expect("cleanup");
    volatile.stop().await.expect("stop");
}

#[tokio::test]
async fn custom_schemas_flow_directly_and_incompatible_or_branched_steps_are_rejected() {
    let mut registry = TransactionalTransformerRegistry::default();
    registry
        .register(Arc::new(NumberFactory { extra_port: false }))
        .expect("register");
    registry
        .register(Arc::new(ArithmeticFactory(Arc::new(Probe::default()))))
        .expect("arithmetic");
    let mut definition = definition();
    let number_step = |name| TransactionStepDefinition {
        id: id(name),
        implementation: ImplementationIdentity::try_new("test/number", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: json!({}),
    };
    definition.steps = vec![number_step("first"), number_step("second")];
    let directory = tempfile::tempdir().expect("temp");
    let mut subject = open(
        definition.clone(),
        Arc::new(registry),
        provider(directory.path()),
    )
    .await;
    let first = number_event(5, 1);
    let output = subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: first,
        })
        .await
        .expect("transform");
    let ChangeOperation::Added { after, .. } = &output[0].envelope.changes().operations()[0] else {
        panic!("number")
    };
    assert_eq!(
        u64::from_be_bytes(after.payload().as_ref().try_into().expect("number")),
        7
    );
    subject.delivery_completed(&output).await.expect("confirm");
    subject.stop().await.expect("stop");
    let mut registry = TransactionalTransformerRegistry::default();
    registry
        .register(Arc::new(NumberFactory { extra_port: true }))
        .expect("register");
    assert!(
        definition.descriptor(&registry).is_err(),
        "cannot branch inside the sequence"
    );
    let mut registry = TransactionalTransformerRegistry::default();
    registry
        .register(Arc::new(NumberFactory { extra_port: false }))
        .expect("register");
    registry
        .register(Arc::new(ArithmeticFactory(Arc::new(Probe::default()))))
        .expect("register");
    definition.steps[1] = step("second", json!({}));
    assert!(
        definition.descriptor(&registry).is_err(),
        "adjacent formats must agree"
    );
}
