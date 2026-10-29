// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::evaluation::variable_value::VariableValue;
use drasi_lib::DrasiLib;
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

#[path = "../computation_support/persistent_faults.rs"]
mod persistent_faults;
use persistent_faults::{
    process::{write_boundary, OwnedChild, WATCHDOG},
    provider, Fault, FaultProvider,
};

const GRAPH: &str = "__drasi_lib_runtime__";
const INSTANCE: &str = "query-child";
const WORKER: &str = "processes::query_child_crash_worker";
const ORIGINAL: &str = "MATCH (n:Person) RETURN n.name AS name";
const CHANGED: &str = "MATCH (n:Person) RETURN n.name AS renamed";
const RESET: &str = "computation-reset-v1:query";

struct Input {
    descriptor: ComponentDescriptor,
    live: Option<OutputEnvelope>,
}

struct Output(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Output {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Output {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for Input {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Input {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(live) = self.live.take() {
            Ok(Some(live))
        } else {
            std::future::pending().await
        }
    }
}

async fn native_query(
    path: &Path,
    text: &str,
    recovery: QueryRecoveryPolicy,
    fault: Arc<Fault>,
) -> ContinuousQueryTransformer {
    let mut definition = definition(text);
    definition.graph_id = GRAPH.into();
    ContinuousQueryTransformer::new_with_options(
        definition,
        Arc::new(FaultProvider {
            inner: provider(path),
            fault,
        }),
        QueryOptions {
            recovery,
            publication: QueryPublicationMode::Atomic,
        },
    )
    .await
    .expect("construct actual persistent query")
    .with_recovery_scope(INSTANCE)
    .expect("set stable query recovery scope")
    .with_bootstrap(Arc::new(Bootstrap {
        calls: Arc::new(AtomicUsize::new(0)),
        fail: false,
    }))
}

async fn fixture(
    path: &Path,
    text: &str,
    recovery: QueryRecoveryPolicy,
    fault: Arc<Fault>,
    live: bool,
) -> (Arc<DrasiLib>, QueryResults) {
    let query = native_query(path, text, recovery, fault).await;
    let results = query.results();
    let output = PortId::try_new("out").expect("output port id");
    let source = ComponentId::try_new("source").expect("source component id");
    let batch = ComponentBatch::builder()
        .source(Box::new(Input {
            descriptor: ComponentDescriptor::try_new(
                source.clone(),
                vec![PortDescriptor::new(
                    output.clone(),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("actual source descriptor"),
            live: live.then(|| OutputEnvelope {
                port: output.clone(),
                envelope: envelope(2, "Alicia", true),
            }),
        }))
        .query(Box::new(query))
        .sink(Box::new(Output(
            ComponentDescriptor::try_new(
                ComponentId::try_new("output").expect("output component id"),
                vec![PortDescriptor::new(
                    PortId::try_new("in").expect("input port id"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("actual sink descriptor"),
        )))
        .bind_stream(
            Endpoint::new(source.clone(), output.clone()),
            StreamId::try_new("source/out").expect("source stream id"),
        )
        .bind_stream(
            Endpoint::new(
                ComponentId::try_new("query").expect("query component id"),
                output.clone(),
            ),
            StreamId::try_new("query/out").expect("query stream id"),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source, output),
                Endpoint::new(
                    ComponentId::try_new("query").expect("query component id"),
                    PortId::try_new("in").expect("input port id"),
                ),
            ),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(
                    ComponentId::try_new("query").expect("query component id"),
                    PortId::try_new("out").expect("output port id"),
                ),
                Endpoint::new(
                    ComponentId::try_new("output").expect("output component id"),
                    PortId::try_new("in").expect("input port id"),
                ),
            ),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .build()
        .expect("build actual query pipeline");
    let core = Arc::new(
        DrasiLib::builder()
            .with_id(INSTANCE)
            .with_components(batch)
            .build()
            .await
            .expect("construct normal query DrasiLib"),
    );
    assert_eq!(
        core.computation_control()
            .expect("normal query graph control")
            .desired_snapshot()
            .id
            .as_ref(),
        GRAPH
    );
    (core, results)
}

fn rows(results: &QueryResults) -> Vec<Value> {
    results
        .snapshot()
        .expect("actual query result snapshot")
        .rows
        .values()
        .map(|bytes| {
            Value::Object(
                QueryChangeCodec::decode_row(bytes)
                    .expect("decode actual query result row")
                    .values
                    .into_iter()
                    .map(|(column, value)| {
                        let VariableValue::String(value) = value else {
                            panic!("expected an actual string result, got {value:?}");
                        };
                        (column.to_string(), json!(value.to_string()))
                    })
                    .collect(),
            )
        })
        .collect()
}

async fn remove_output(core: &DrasiLib) {
    let control = core
        .computation_control()
        .expect("normal query graph control");
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![
                DesiredMutation::Unbind {
                    edge: EdgeDefinition::new(
                        Endpoint::new(
                            ComponentId::try_new("query").expect("query component id"),
                            PortId::try_new("out").expect("output port id"),
                        ),
                        Endpoint::new(
                            ComponentId::try_new("output").expect("output component id"),
                            PortId::try_new("in").expect("input port id"),
                        ),
                    ),
                    policy: RemovalPolicy::Reject,
                },
                DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![
                        ComponentId::try_new("output").expect("output component id")
                    ]),
                    policy: RemovalPolicy::Reject,
                },
            ],
        )
        .await
        .expect("preview actual sink detachment");
    let report = control
        .reconcile(preview, TopologyBindings::default())
        .await
        .expect("reconcile actual sink detachment");
    assert!(report.failures.is_empty(), "{report:?}");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked and externally killed only by the bounded query process-crash driver"]
async fn query_child_crash_worker() {
    let path = std::path::PathBuf::from(std::env::var_os("DRASI_CHILD_STATE").unwrap());
    let phase = std::env::var("DRASI_CHILD_PHASE").unwrap();
    let reset = phase.contains("reset");
    let text = if reset { CHANGED } else { ORIGINAL };
    let fault = Arc::new(Fault::default());
    let (core, results) = fixture(
        &path,
        text,
        if reset {
            QueryRecoveryPolicy::AutoReset
        } else {
            QueryRecoveryPolicy::Strict
        },
        fault.clone(),
        phase == "seed",
    )
    .await;
    if phase.starts_with("resume") || phase == "seed" {
        let (core, results) = if phase == "resume-deprovision-partial" {
            remove_output(&core).await;
            core.remove_query("query").await.unwrap();
            assert!(core
                .computation_control()
                .unwrap()
                .desired_snapshot()
                .nodes
                .iter()
                .all(|node| node.descriptor.id().as_str() != "query"));
            core.shutdown().await.unwrap();
            fixture(
                &path,
                ORIGINAL,
                QueryRecoveryPolicy::Strict,
                Arc::new(Fault::default()),
                false,
            )
            .await
        } else {
            (core, results)
        };
        core.start().await.unwrap();
        if phase == "seed" {
            tokio::time::timeout(WATCHDOG, async {
                while results.snapshot().unwrap().as_of_sequence != 1 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the normal graph must commit the seeded live update");
        }
        let snapshot = results.snapshot().unwrap();
        let result = json!({
            "pid": std::process::id(),
            "graph": core.computation_control().unwrap().desired_snapshot().id.as_ref(),
            "sequence": snapshot.as_of_sequence,
            "generation": snapshot.generation,
            "rows": rows(&results),
        });
        core.shutdown().await.unwrap();
        std::fs::write(
            path.join(format!("{phase}.json")),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        return;
    }
    if !reset {
        core.start().await.unwrap();
        assert_eq!(rows(&results), [json!({"name":"Alicia"})]);
        core.stop_query("query").await.unwrap();
        remove_output(&core).await;
    }
    fault
        .skip
        .store(usize::from(phase.contains("wipe")), Ordering::SeqCst);
    fault.mode.store(
        if phase.ends_with("-after") { 3 } else { 4 },
        Ordering::SeqCst,
    );
    let operation_core = core.clone();
    let mut operation = tokio::spawn(async move {
        if reset {
            operation_core.start().await
        } else {
            operation_core.remove_query("query").await
        }
    });
    tokio::time::timeout(WATCHDOG, async {
        tokio::select! {
            _ = fault.committed.notified() => {}
            result = &mut operation => panic!("boundary {phase} did not hold its real commit: {result:?}"),
        }
    })
    .await
    .unwrap();
    write_boundary(
        &path,
        &phase,
        core.computation_control()
            .unwrap()
            .desired_snapshot()
            .id
            .as_ref(),
    );
    std::future::pending::<()>().await;
}

#[test]
fn normal_drasi_query_recovers_after_child_death_during_reset_and_deprovision() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for phase in [
        "reset-intent-after",
        "reset-wipe-before",
        "reset-wipe-after",
        "deprovision-intent-after",
        "deprovision-wipe-before",
        "deprovision-wipe-after",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let mut seed = OwnedChild::start(path, WORKER, "seed", GRAPH);
        seed.wait(path, "seed", false);
        let seeded: Value =
            serde_json::from_slice(&std::fs::read(path.join("seed.json")).unwrap()).unwrap();
        assert_eq!(seeded["sequence"], 1);
        assert_eq!(seeded["generation"], 0);
        assert_eq!(seeded["rows"], json!([{"name":"Alicia"}]));
        let mut child = OwnedChild::start(path, WORKER, phase, GRAPH);
        child.wait(path, phase, true);
        runtime.block_on(async {
            match provider(path).create_indexes(GRAPH, "query").await {
                Err(error) => assert!(
                    error.to_string().to_ascii_lowercase().contains("lock"),
                    "unexpected storage rejection instead of exclusive ownership: {error:#}"
                ),
                Ok(resources) => {
                    resources.cleanup().unwrap().shutdown().await.unwrap();
                    panic!("the live child lost exclusive ownership of its actual storage");
                }
            }
        });
        child.kill();
        let reset = phase.starts_with("reset");
        let completed = phase == "deprovision-wipe-after";
        runtime.block_on(async {
            let resources = provider(path).create_indexes(GRAPH, "query").await.unwrap();
            let marker = resources
                .outbox_writer()
                .unwrap()
                .read_from(RESET, 0)
                .await
                .unwrap();
            assert_eq!(marker.len(), 1, "{phase}");
            assert_eq!(marker[0].0, 1);
            let marker: Value = serde_json::from_slice(&marker[0].1).unwrap();
            assert_eq!(marker["in_progress"], !completed, "{phase}");
            assert_eq!(marker["generation"], 1, "{phase}");
            assert_eq!(marker["sequence"], u64::from(reset), "{phase}");
            assert_eq!(
                resources
                    .checkpoint_store()
                    .unwrap()
                    .read_result_sequence("query")
                    .await
                    .unwrap(),
                Some(u64::from(!completed)),
                "{phase}"
            );
            assert_eq!(
                resources
                    .live_results_writer()
                    .unwrap()
                    .read_snapshot("query")
                    .await
                    .unwrap()
                    .len(),
                usize::from(!phase.ends_with("wipe-after")),
                "{phase}"
            );
            resources.cleanup().unwrap().shutdown().await.unwrap();
            drop(resources);
            if !completed {
                let mut strict = native_query(
                    path,
                    if reset { CHANGED } else { ORIGINAL },
                    QueryRecoveryPolicy::Strict,
                    Arc::new(Fault::default()),
                )
                .await;
                let error = strict
                    .start()
                    .await
                    .expect_err("partial clearing cannot resume");
                if phase == "reset-wipe-after" {
                    assert!(
                        matches!(
                            error.downcast_ref(),
                            Some(QueryRecoveryError::IncompleteBootstrap)
                        ),
                        "{phase}: {error:#}"
                    );
                } else {
                    assert!(
                        matches!(
                            error.downcast_ref(),
                            Some(QueryRecoveryError::IncompleteReset)
                        ),
                        "{phase}: {error:#}"
                    );
                }
                strict.stop().await.unwrap();
            }
        });
        let resume = if reset {
            "resume-reset"
        } else if completed {
            "resume-deprovision-complete"
        } else {
            "resume-deprovision-partial"
        };
        let mut recovery = OwnedChild::start(path, WORKER, resume, GRAPH);
        recovery.wait(path, resume, false);
        let result: Value =
            serde_json::from_slice(&std::fs::read(path.join(format!("{resume}.json"))).unwrap())
                .unwrap();
        assert_ne!(result["pid"], child.id());
        assert_eq!(result["pid"], recovery.id());
        assert_eq!(result["graph"], GRAPH);
        assert_eq!(result["sequence"], u64::from(reset), "{phase}");
        assert_eq!(
            result["generation"],
            if completed { 1 } else { 2 },
            "{phase}"
        );
        assert_eq!(
            result["rows"],
            if reset {
                json!([{"renamed":"Alice"}])
            } else {
                json!([{"name":"Alice"}])
            },
            "{phase}"
        );
        eprintln!(
            "{phase}: killed PID {}, recovered in PID {}, exact sequence {}, generation {}",
            child.id(),
            recovery.id(),
            result["sequence"],
            result["generation"]
        );
    }
}
