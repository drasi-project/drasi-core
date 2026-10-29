// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::persistent_faults::process::{write_boundary, OwnedChild, WATCHDOG};
use super::*;
use drasi_core::{computation::ComputationIndexProvider, interface::FailureMode};
use drasi_lib::DrasiLib;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::AtomicBool};

const GRAPH: &str = "__drasi_lib_runtime__";
const INSTANCE: &str = "merger-child";
const WORKER: &str = "recovery::processes::merger_child_crash_worker";

fn config() -> SourceTimeMergeDefinition {
    let mut config = definition();
    config.graph_id = GRAPH.into();
    config
}

fn codec() -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(size(128 * 1024));
    codec
        .register_schema(schema())
        .expect("register merger schema");
    codec
}

async fn output_channel(path: &Path, fault: Arc<Fault>) -> Arc<QosChannel> {
    let provider = FaultProvider {
        inner: provider(path),
        fault,
    };
    QosChannel::persistent_with_recovery(
        QosChannelDefinition {
            stream: stream("merge"),
            capacity: size(8),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: std::collections::BTreeMap::from([(
                "sink".into(),
                SubscriptionStart::Earliest,
            )]),
        },
        provider
            .create_indexes(GRAPH, "output")
            .await
            .expect("open persistent output indexes"),
        codec(),
        "output",
        QosRecoveryOptions::Replay(ReplayOptions {
            failure_scope: FailureMode::ProcessRestart,
            receipt_capacity: size(8),
        }),
    )
    .await
    .expect("recover persistent output channel")
}

struct Ingress(Arc<SourceAdmission>, ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Ingress {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.1
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Ingress {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.0.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("the normal graph must drive actual journal admission")
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    received: Received,
    hold: Arc<AtomicBool>,
}

#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if self.hold.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        self.received
            .lock()
            .expect("lock actual merger capture")
            .push(input);
        Ok(())
    }
}

struct Fixture {
    core: DrasiLib,
    input: Arc<QosChannel>,
    output: Arc<QosChannel>,
    admission: Arc<SourceAdmission>,
    merger: tokio::sync::watch::Receiver<SourceTimeMergeSnapshot>,
    received: Received,
    hold: Arc<AtomicBool>,
}

impl Fixture {
    async fn open(path: &Path, merge_fault: Arc<Fault>, journal_fault: Arc<Fault>) -> Self {
        let input = QosChannel::persistent_with_recovery(
            QosChannelDefinition {
                stream: stream("a"),
                capacity: size(8),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: std::collections::BTreeMap::from([(
                    "merge".into(),
                    SubscriptionStart::Earliest,
                )]),
            },
            provider(path)
                .create_indexes(GRAPH, "input")
                .await
                .expect("open persistent input indexes"),
            codec(),
            "input",
            QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: INSTANCE.into(),
                graph_id: GRAPH.into(),
                component_id: component("a"),
                failure_scope: FailureMode::ProcessRestart,
                max_producers: size(1),
                receipts_per_producer: size(8),
            }),
        )
        .await
        .expect("recover persistent admission channel");
        let admission = SourceAdmission::new(input.clone(), port("out"))
            .await
            .expect("construct actual source admission");
        let output = output_channel(path, journal_fault).await;
        let merger = SourceTimeMergeTransformer::new_durable(
            config(),
            schema(),
            Arc::new(FaultProvider {
                inner: provider(path),
                fault: merge_fault,
            }),
        )
        .await
        .expect("recover actual durable merger");
        let observations = merger.subscribe();
        let received = Received::default();
        let hold = Arc::new(AtomicBool::new(false));
        let mut batch = ComponentBatch::builder()
            .source(Box::new(Ingress(
                admission.clone(),
                descriptor("a", &[], &["out"]),
            )))
            .transformer(Box::new(merger))
            .sink(Box::new(Sink {
                descriptor: descriptor("sink", &["in"], &[]),
                received: received.clone(),
                hold: hold.clone(),
            }));
        for (name, channel) in [("input", &input), ("output", &output)] {
            let id = ResourceId::try_new(name).expect("channel resource id");
            batch = batch
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: name.into(),
                })
                .expect("declare actual channel resource")
                .provide_resource(id, channel.resource())
                .expect("provide actual channel resource");
        }
        let batch = batch
            .bind_stream(endpoint("a", "out"), stream("a"))
            .bind_stream(endpoint("merge", "out"), stream("merge"))
            .connect(
                edge("a", "merge"),
                Box::new(input.definition().pipe(
                    ResourceId::try_new("input").expect("input resource id"),
                    "merge",
                )),
            )
            .connect(
                edge("merge", "sink"),
                Box::new(output.definition().pipe(
                    ResourceId::try_new("output").expect("output resource id"),
                    "sink",
                )),
            )
            .build()
            .expect("build actual merger pipeline");
        let core = DrasiLib::builder()
            .with_id(INSTANCE)
            .with_components(batch)
            .build()
            .await
            .expect("construct normal merger DrasiLib");
        core.start().await.expect("start normal merger DrasiLib");
        assert_eq!(
            core.computation_control()
                .expect("normal merger graph control")
                .desired_snapshot()
                .id
                .as_ref(),
            GRAPH
        );
        Self {
            core,
            input,
            output,
            admission,
            merger: observations,
            received,
            hold,
        }
    }

    async fn admit(&self, sequence: u64, time: i64) {
        let session = self
            .admission
            .register_producer(component("caller"))
            .await
            .expect("register actual merger producer");
        let input = event("a", sequence, time);
        let deadline = tokio::time::Instant::now() + WATCHDOG;
        loop {
            match self.admission.admit(&session, sequence, &input).await {
                Err(PipeError::Backend(error))
                    if error.downcast_ref::<AdmissionRejection>()
                        == Some(&AdmissionRejection::Busy)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => {
                    result.expect("admit actual merger input");
                    break;
                }
            }
        }
    }

    async fn close(&self) {
        self.core
            .shutdown()
            .await
            .expect("shutdown merger DrasiLib");
        self.input.shutdown().await.expect("close input storage");
        self.output.shutdown().await.expect("close output storage");
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked and externally killed only by the bounded merger process-crash driver"]
async fn merger_child_crash_worker() {
    let path = PathBuf::from(std::env::var_os("DRASI_CHILD_STATE").unwrap());
    let phase = std::env::var("DRASI_CHILD_PHASE").unwrap();
    let merger = Arc::new(Fault::default());
    let journal = Arc::new(Fault::default());
    let fixture = Fixture::open(&path, merger.clone(), journal.clone()).await;
    if phase == "resume" {
        fixture.admit(2, 10).await;
        tokio::time::timeout(WATCHDOG, async {
            loop {
                let complete = {
                    let merger = fixture.merger.borrow();
                    merger.emitted == 2
                        && merger.buffered_events == 0
                        && fixture.received.lock().unwrap().len() == 1
                };
                if complete {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "resume failed: {:?}",
                fixture.core.computation_control().unwrap().observed()
            )
        });
        let snapshot = fixture.merger.borrow().clone();
        let results: Vec<_> = fixture
            .received
            .lock()
            .unwrap()
            .iter()
            .map(|input| {
                let progress = GraphProducerProgress::from_envelope(&input.envelope)
                    .unwrap()
                    .unwrap();
                json!({
                    "timestamp": input.envelope.system().timestamp().unwrap().timestamp_millis(),
                    "sequence": progress.sequence(),
                    "values": values(&input.envelope),
                })
            })
            .collect();
        let result = json!({
            "pid": std::process::id(),
            "graph": fixture.core.computation_control().unwrap().desired_snapshot().id.as_ref(),
            "buffered": snapshot.buffered_events,
            "emitted": snapshot.emitted,
            "input_accepted": fixture.input.progress().await.unwrap().accepted,
            "output_accepted": fixture.output.progress().await.unwrap().accepted,
            "results": results,
        });
        fixture.close().await;
        std::fs::write(
            path.join("result.json"),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        return;
    }
    fixture.admit(1, 1).await;
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let initialized = {
                let snapshot = fixture.merger.borrow();
                snapshot.emitted == 1 && snapshot.buffered_events == 0
            };
            if initialized
                && fixture.output.progress().await.unwrap().processed["sink"] == 1
                && fixture.input.progress().await.unwrap().processed["merge"] == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first actual input must finish binding, processing and both journal acknowledgements");
    fixture.received.lock().unwrap().clear();
    fixture.hold.store(true, Ordering::Release);
    let (fault, skip) = if phase.starts_with("admission-") {
        (&merger, 0)
    } else if phase.starts_with("emission-") {
        (&merger, 1)
    } else if phase.starts_with("handoff-") {
        (&journal, 0)
    } else if phase.starts_with("confirmation-") {
        (&merger, 2)
    } else {
        panic!("unknown crash boundary {phase}");
    };
    fault.skip.store(skip, Ordering::SeqCst);
    fault.mode.store(
        if phase.ends_with("-after") { 3 } else { 4 },
        Ordering::SeqCst,
    );
    fixture.admit(2, 10).await;
    tokio::time::timeout(WATCHDOG, fault.committed.notified())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "boundary {phase} failed: {:?}",
                fixture.core.computation_control().unwrap().observed()
            )
        });
    write_boundary(
        &path,
        &phase,
        fixture
            .core
            .computation_control()
            .unwrap()
            .desired_snapshot()
            .id
            .as_ref(),
    );
    std::future::pending::<()>().await;
}

#[test]
fn normal_drasi_merger_recovers_after_child_death_at_persistence_and_handoff() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for phase in [
        "admission-before",
        "admission-after",
        "emission-before",
        "emission-after",
        "handoff-before",
        "handoff-after",
        "confirmation-before",
        "confirmation-after",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut child = OwnedChild::start(directory.path(), WORKER, phase, GRAPH);
        child.wait(directory.path(), phase, true);
        child.kill();
        runtime.block_on(async {
            let mut merger = SourceTimeMergeTransformer::new_durable(
                config(),
                schema(),
                provider(directory.path()),
            )
            .await
            .unwrap();
            merger.start().await.unwrap();
            let snapshot = merger.subscribe().borrow().clone();
            assert_eq!(
                snapshot.buffered_events,
                usize::from(phase != "admission-before" && phase != "confirmation-after"),
                "{phase}"
            );
            assert_eq!(
                snapshot.emitted,
                1 + u64::from(!matches!(
                    phase,
                    "admission-before" | "admission-after" | "emission-before"
                )),
                "{phase}"
            );
            merger.stop().await.unwrap();
            drop(merger);
            let output = output_channel(directory.path(), Arc::new(Fault::default())).await;
            assert_eq!(
                output.progress().await.unwrap().accepted,
                1 + u64::from(matches!(
                    phase,
                    "handoff-after" | "confirmation-before" | "confirmation-after"
                )),
                "{phase}"
            );
            output.shutdown().await.unwrap();
        });
        let mut resume = OwnedChild::start(directory.path(), WORKER, "resume", GRAPH);
        resume.wait(directory.path(), "resume", false);
        let result: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("result.json")).unwrap())
                .unwrap();
        assert_ne!(result["pid"], child.id());
        assert_eq!(result["pid"], resume.id());
        assert_eq!(result["graph"], GRAPH);
        assert_eq!(result["buffered"], 0, "{phase}");
        assert_eq!(result["emitted"], 2, "{phase}");
        assert_eq!(result["input_accepted"], 2, "{phase}");
        assert_eq!(result["output_accepted"], 2, "{phase}");
        assert_eq!(
            result["results"],
            json!([{"timestamp":10,"sequence":2,"values":[2]}]),
            "{phase}"
        );
    }
}
