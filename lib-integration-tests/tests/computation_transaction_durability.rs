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

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    evaluation::variable_value::VariableValue,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;
use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

const CRASH_EXIT: i32 = 74;

struct ArithmeticStep {
    descriptor: ComponentDescriptor,
    add: i64,
    multiply: i64,
    trace: PathBuf,
    crash_before_commit: bool,
}

#[async_trait]
impl ComputationComponent for ArithmeticStep {
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
impl Transformer for ArithmeticStep {
    async fn transform(&mut self, _: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        anyhow::bail!("the transaction container must call its participants directly")
    }
}

#[async_trait]
impl TransactionalTransformer for ArithmeticStep {
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
    ) -> Result<ChangeEnvelope> {
        let count = match context.get("counter").await? {
            None => 1,
            Some(ElementValue::Integer(previous)) => previous + 1,
            value => anyhow::bail!("invalid counter: {value:?}"),
        };
        context.put("counter", ElementValue::Integer(count)).await?;
        assert_eq!(
            context.get("counter").await?,
            Some(ElementValue::Integer(count))
        );
        let mut trace = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.trace)?;
        writeln!(trace, "{}:{count}", context.step_id())?;
        trace.sync_all()?;
        if self.crash_before_commit && context.step_id().as_str() == "second" {
            // Bypass rollback guards and destructors after both steps staged writes.
            std::process::exit(CRASH_EXIT);
        }
        let mut changes = GraphChangeCodec::decode_changes(&input)?;
        for change in &mut changes {
            let SourceChange::Insert {
                element: Element::Node { properties, .. },
            } = change
            else {
                anyhow::bail!("expected an inserted node")
            };
            let Some(ElementValue::Integer(value)) = properties.get("value") else {
                anyhow::bail!("missing input value")
            };
            let value = (value + self.add) * self.multiply;
            properties.insert("value", ElementValue::Integer(value));
            properties.insert(
                &format!("{}_count", context.step_id()),
                ElementValue::Integer(count),
            );
        }
        Ok(GraphChangeCodec::derive_changes(
            &input,
            &changes,
            StreamId::try_new(format!("{}/out", context.step_id()))?,
            input.system().sequence(),
        )?)
    }
}

struct ArithmeticFactory {
    trace: PathBuf,
    crash_before_commit: bool,
}

impl TransactionalTransformerFactory for ArithmeticFactory {
    fn implementation(&self) -> ImplementationIdentity {
        ImplementationIdentity::try_new("test/crash-arithmetic", "1").expect("implementation")
    }
    fn configuration_version(&self) -> u32 {
        1
    }
    fn create(
        &self,
        step: &TransactionStepDefinition,
    ) -> Result<Box<dyn TransactionalTransformer>> {
        Ok(Box::new(ArithmeticStep {
            descriptor: ComponentDescriptor::try_new(
                step.id.clone(),
                vec![
                    PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    ),
                    PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    ),
                ],
            )?,
            add: step.configuration["add"].as_i64().context("missing add")?,
            multiply: step.configuration["multiply"]
                .as_i64()
                .context("missing multiply")?,
            trace: self.trace.clone(),
            crash_before_commit: self.crash_before_commit,
        }))
    }
}

fn storage(root: &Path) -> Arc<dyn ComputationIndexProvider> {
    LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)))
}

async fn transaction(root: &Path, crash_before_commit: bool) -> Result<TransactionTransformer> {
    let factory = Arc::new(ArithmeticFactory {
        trace: root.join("steps.log"),
        crash_before_commit,
    });
    let mut registry = TransactionalTransformerRegistry::default();
    registry.register(factory.clone())?;
    let mut subject = TransactionTransformer::new(
        TransactionTransformerDefinition {
            graph_id: "transaction-crash".into(),
            id: ComponentId::try_new("sequence")?,
            output_stream: StreamId::try_new("sequence/out")?,
            steps: [("first", 2, 1), ("second", 0, 4)]
                .into_iter()
                .map(|(name, add, multiply)| {
                    Ok(TransactionStepDefinition {
                        id: ComponentId::try_new(name)?,
                        implementation: factory.implementation(),
                        configuration_version: 1,
                        configuration: json!({"add": add, "multiply": multiply}),
                    })
                })
                .collect::<Result<_>>()?,
            outbox_capacity: NonZeroUsize::new(4).expect("transaction capacity"),
        },
        Arc::new(registry),
        storage(&root.join("transaction")),
    )
    .await?;
    subject.start().await?;
    Ok(subject)
}

fn input(sequence: u64, value: i64) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: sequence,
                    },
                    properties: ElementPropertyMap::from(json!({"value": value})),
                },
            },
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

async fn query(root: &Path, name: &str) -> Result<ContinuousQueryTransformer> {
    let mut subject = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "transaction-crash".into(),
            id: ComponentId::try_new(name)?,
            query: "MATCH (n:Item) RETURN n.value AS value, n.first_count AS first_count, n.second_count AS second_count".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{name}/out"))?,
            outbox_capacity: NonZeroUsize::new(8).expect("query capacity"),
        },
        storage(&root.join(name)),
    )
    .await?;
    subject.start().await?;
    Ok(subject)
}

async fn deliver(
    query: &mut ContinuousQueryTransformer,
    output: &[OutputEnvelope],
) -> Result<usize> {
    assert_eq!(output.len(), 1);
    Ok(query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output[0].envelope.clone(),
        })
        .await?
        .len())
}

fn rows(query: &ContinuousQueryTransformer) -> Result<Vec<(i64, i64, i64)>> {
    let mut result = Vec::new();
    for record in query.results().snapshot()?.rows.values() {
        let row = QueryChangeCodec::decode_row(record)?;
        let integer = |name: &str| match row.values.get(name) {
            Some(VariableValue::Integer(value)) => value.as_i64().context("integer range"),
            value => anyhow::bail!("missing integer {name}: {value:?}"),
        };
        result.push((
            integer("value")?,
            integer("first_count")?,
            integer("second_count")?,
        ));
    }
    result.sort_unstable();
    Ok(result)
}

#[tokio::test]
#[ignore = "worker explicitly invoked by transaction crash tests"]
async fn transaction_crash_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var("DRASI_TRANSACTION_CRASH_ROOT")?);
    let boundary = std::env::var("DRASI_TRANSACTION_CRASH_BOUNDARY")?;
    let phase = std::env::var("DRASI_TRANSACTION_CRASH_PHASE")?;
    std::fs::write(
        root.join(format!("{phase}.pid")),
        std::process::id().to_string(),
    )?;
    let precommit = boundary == "before-commit";
    let mut subject = transaction(&root, precommit && phase == "seed").await?;
    let mut first = query(&root, "first-query").await?;
    let mut second = query(&root, "second-query").await?;
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("codec limit"));
    codec.register_schema(GraphChangeCodec::schema())?;

    if phase == "seed" {
        let output = subject.transform(input(1, 3)?).await?;
        assert!(
            !precommit,
            "the second step must exit before the container commits"
        );
        let mut saved = std::fs::File::create(root.join("committed.bin"))?;
        saved.write_all(&codec.encode(&output[0].envelope)?)?;
        saved.sync_all()?;
        if boundary != "before-delivery" {
            assert_eq!(deliver(&mut first, &output).await?, 1);
        }
        if matches!(boundary.as_str(), "after-delivery" | "after-confirmation") {
            assert_eq!(deliver(&mut second, &output).await?, 1);
        }
        if boundary == "after-confirmation" {
            subject.delivery_completed(&output).await?;
        }
        std::process::exit(CRASH_EXIT);
    }

    anyhow::ensure!(phase == "recover", "unknown phase");
    assert_eq!(
        std::fs::read_to_string(root.join("steps.log"))?,
        "first:1\nsecond:1\n"
    );
    if precommit {
        assert!(!subject.has_pending_emissions());
        assert!(rows(&first)?.is_empty());
        assert!(rows(&second)?.is_empty());
        let output = subject.transform(input(1, 3)?).await?;
        assert_eq!(deliver(&mut first, &output).await?, 1);
        assert_eq!(deliver(&mut second, &output).await?, 1);
        subject.delivery_completed(&output).await?;
        assert_eq!(
            std::fs::read_to_string(root.join("steps.log"))?,
            "first:1\nsecond:1\nfirst:1\nsecond:1\n",
            "both uncommitted counters and input progress must roll back after process death"
        );
    } else if boundary == "after-confirmation" {
        assert!(!subject.has_pending_emissions());
    } else {
        assert!(subject.has_pending_emissions());
        let original = codec.decode(&std::fs::read(root.join("committed.bin"))?)?;
        let replay = subject.on_wakeup().await?;
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].envelope.changes().id(), original.changes().id());
        assert_eq!(
            replay[0].envelope.changes().operations(),
            original.changes().operations()
        );
        assert!(replay[0].envelope.system().sequence() > original.system().sequence());
        assert_eq!(
            GraphProducerProgress::from_envelope(&replay[0].envelope)?,
            GraphProducerProgress::from_envelope(&original)?
        );
        assert_eq!(
            deliver(&mut first, &replay).await?,
            usize::from(boundary == "before-delivery")
        );
        assert_eq!(
            deliver(&mut second, &replay).await?,
            usize::from(boundary != "after-delivery")
        );
        subject.delivery_completed(&replay).await?;
    }
    assert_eq!(rows(&first)?, [(20, 1, 1)]);
    assert_eq!(rows(&second)?, [(20, 1, 1)]);
    let trace = std::fs::read_to_string(root.join("steps.log"))?;
    if !precommit {
        assert_eq!(
            trace, "first:1\nsecond:1\n",
            "committed steps must not run during replay"
        );
    }
    let duplicate = subject.transform(input(1, 3)?).await?;
    assert_eq!(deliver(&mut first, &duplicate).await?, 0);
    assert_eq!(deliver(&mut second, &duplicate).await?, 0);
    subject.delivery_completed(&duplicate).await?;
    assert_eq!(std::fs::read_to_string(root.join("steps.log"))?, trace);
    let next = subject.transform(input(2, 4)?).await?;
    assert_eq!(deliver(&mut first, &next).await?, 1);
    assert_eq!(deliver(&mut second, &next).await?, 1);
    subject.delivery_completed(&next).await?;
    assert_eq!(rows(&first)?, [(20, 1, 1), (24, 2, 2)]);
    assert_eq!(rows(&second)?, [(20, 1, 1), (24, 2, 2)]);
    assert_eq!(
        std::fs::read_to_string(root.join("steps.log"))?,
        format!("{trace}first:2\nsecond:2\n")
    );
    subject.stop().await?;
    first.stop().await?;
    second.stop().await?;
    Ok(())
}

#[tokio::test]
async fn process_crashes_preserve_transaction_atomicity_and_partial_fanout_recovery() -> Result<()>
{
    let executable = std::env::current_exe()?;
    for boundary in [
        "before-commit",
        "before-delivery",
        "partial-fanout",
        "after-delivery",
        "after-confirmation",
    ] {
        let directory = tempfile::tempdir()?;
        for phase in ["seed", "recover"] {
            let child = tokio::process::Command::new(&executable)
                .args([
                    "--ignored",
                    "--exact",
                    "transaction_crash_worker",
                    "--nocapture",
                ])
                .env("DRASI_TRANSACTION_CRASH_ROOT", directory.path())
                .env("DRASI_TRANSACTION_CRASH_BOUNDARY", boundary)
                .env("DRASI_TRANSACTION_CRASH_PHASE", phase)
                .kill_on_drop(true)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let pid = child.id().context("worker PID")?;
            assert_ne!(pid, std::process::id());
            let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
                .await
                .with_context(|| format!("{phase}/{boundary} timed out"))??;
            let expected = if phase == "seed" { CRASH_EXIT } else { 0 };
            anyhow::ensure!(
                output.status.code() == Some(expected),
                "{phase}/{boundary}: expected exit {expected}, got {}; stdout={}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(directory.path().join(format!("{phase}.pid")))?,
                pid.to_string()
            );
        }
    }
    Ok(())
}
