// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use drasi_computation_bridge::{
    BridgeConfig, BridgeEgressControl, BridgeEndpointControl, Limits, Namespace, PortableSchema,
    Profile, TlsRecipeKind, TlsResourceRecipe, TlsRole, Transport,
};
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::FailureMode,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::DrasiLib;
use sha2::{Digest, Sha256};

const HELPER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/network/peer.py");
const LINUX_HELPER: &str = "/workspace/components/instance-host/tests/network/peer.py";

fn output(command: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("helper input")?
            .write_all(input)?;
    } else {
        drop(child.stdin.take());
    }
    let result = child.wait_with_output()?;
    ensure!(
        result.status.success(),
        "qualification helper failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}

struct LinuxPeer {
    container: String,
    root: String,
    child: Option<Child>,
    diagnostics: PathBuf,
}

impl LinuxPeer {
    fn request(&self, command: ControlCommand) -> Result<ControlReply> {
        let reply = output(
            Command::new("podman").args([
                "exec",
                "-i",
                &self.container,
                "python3",
                LINUX_HELPER,
                "control",
                &format!("{}/state", self.root),
            ]),
            Some(&serde_json::to_vec(&command)?),
        )?;
        Ok(serde_json::from_slice(&reply)?)
    }

    fn status(&self) -> Result<HostObservation> {
        match self.request(ControlCommand::Status)? {
            ControlReply::Status { observation } => Ok(*observation),
            _ => anyhow::bail!("unexpected Linux status reply"),
        }
    }

    fn start(&mut self, config: &HostConfig) -> Result<()> {
        ensure!(self.child.is_none(), "old Linux host has not been joined");
        output(
            Command::new("podman").args([
                "exec",
                "-i",
                &self.container,
                "python3",
                LINUX_HELPER,
                "seed",
                &self.root,
            ]),
            Some(&serde_json::to_vec(config)?),
        )?;
        let credentials = Credentials::new(config)?;
        let launch = serde_json::to_vec(&json!({
            "credentials": credentials,
            "owner_instance": "network-qualification"
        }))?;
        let diagnostics = File::create(&self.diagnostics)?;
        let mut child = Command::new("podman")
            .args([
                "exec",
                "-i",
                &self.container,
                "/build/debug/drasi-instance-host",
                "--config",
                &format!("{}/host.json", self.root),
                "--owned",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(diagnostics.try_clone()?))
            .stderr(Stdio::from(diagnostics))
            .spawn()?;
        let stdin = child.stdin.as_mut().context("owned Linux lifetime pipe")?;
        stdin.write_all(&(launch.len() as u32).to_be_bytes())?;
        stdin.write_all(&launch)?;
        stdin.flush()?;
        self.child = Some(child);
        Ok(())
    }

    fn wait(&mut self, predicate: impl Fn(&HostObservation) -> bool) -> Result<HostObservation> {
        let deadline = Instant::now() + WATCHDOG;
        let mut last = None;
        while Instant::now() < deadline {
            if let Some(exit) = self.child.as_mut().context("Linux owner")?.try_wait()? {
                anyhow::bail!(
                    "Linux host exited {exit}: {}",
                    std::fs::read_to_string(&self.diagnostics)?
                );
            }
            if let Ok(observation) = self.status() {
                if predicate(&observation) {
                    return Ok(observation);
                }
                last = Some(observation);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!(
            "Linux readiness timed out: {last:?}; {}",
            std::fs::read_to_string(&self.diagnostics)?
        )
    }

    fn stop(&mut self) -> Result<()> {
        ensure!(
            matches!(
                self.request(ControlCommand::StopTree)?,
                ControlReply::Accepted { .. }
            ),
            "Linux stop was not accepted"
        );
        let deadline = Instant::now() + WATCHDOG;
        while Instant::now() < deadline {
            if let Some(exit) = self.child.as_mut().context("Linux owner")?.try_wait()? {
                ensure!(
                    exit.success(),
                    "Linux host shutdown failed: {}",
                    std::fs::read_to_string(&self.diagnostics)?
                );
                self.child.take();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!(
            "Linux shutdown timed out: {}",
            std::fs::read_to_string(&self.diagnostics)?
        )
    }

    fn captured(&self) -> Result<Vec<u8>> {
        output(
            Command::new("podman").args([
                "exec",
                &self.container,
                "cat",
                &format!("{}/results", self.root),
            ]),
            None,
        )
    }
}

impl Drop for LinuxPeer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            drop(child.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    self.child.take();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if self.child.is_none() {
            if let Err(error) = output(
                Command::new("podman").args(["exec", &self.container, "rm", "-rf", &self.root]),
                None,
            ) {
                eprintln!("owned network fixture cleanup failed: {error}");
            }
        } else {
            eprintln!(
                "Linux ownership cleanup is incomplete; retaining {}",
                self.root
            );
        }
    }
}

struct Proxy(Child);
impl Drop for Proxy {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn tls(root: &Path, name: &str, role: TlsRole, pin: String) -> TlsResourceRecipe {
    TlsResourceRecipe {
        kind: TlsRecipeKind::BridgeTls,
        version: 1,
        role,
        trust_roots: vec![root.join("ca.der")],
        certificate_chain: vec![root.join(format!("{name}.der"))],
        private_key: root.join(format!("{name}-key.der")),
        peer_certificate_sha256: pin,
    }
}

fn resource(
    config: &mut HostConfig,
    name: &str,
    role: ResourceRole,
    recipe: serde_json::Value,
) -> Result<ResourceId> {
    let id = ResourceId::try_new(name)?;
    config
        .desired
        .topology
        .resources
        .push(ResourceSpecification {
            id: id.clone(),
            role,
            ownership: ResourceOwnership::Graph,
            binding: name.into(),
        });
    config
        .desired
        .topology
        .resource_configurations
        .insert(id.clone(), recipe);
    Ok(id)
}

fn dependency(config: &mut HostConfig, component: &str, name: &str, resource: ResourceId) {
    let ComponentConstruction::Factory(specification) = &mut config
        .desired
        .topology
        .components
        .iter_mut()
        .find(|value| value.descriptor.id().as_str() == component)
        .unwrap()
        .construction
    else {
        panic!("network fixture requires a factory");
    };
    specification
        .dependencies
        .insert(name.into(), vec![resource]);
}

fn definition(stream: &str, subscriber: &str) -> Result<QosChannelDefinition> {
    Ok(QosChannelDefinition {
        stream: StreamId::try_new(stream)?,
        capacity: NonZeroUsize::new(2).unwrap(),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([(subscriber.into(), SubscriptionStart::Earliest)]),
    })
}

struct AdmissionInput {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
}

#[async_trait]
impl ComputationComponent for AdmissionInput {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for AdmissionInput {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        anyhow::bail!("the normal graph owns admission publication")
    }
}

fn input(sequence: u64) -> Result<ChangeEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert("value", ElementValue::Integer(sequence as i64 - 1));
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("plant/input", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Input")]),
                    effective_from: sequence,
                },
                properties,
            },
        },
        StreamId::try_new("sender/out")?,
        sequence,
        None,
    )?)
}

async fn admit_when_idle(
    admission: &SourceAdmission,
    session: &ProducerSession,
    sequence: u64,
    input: &ChangeEnvelope,
) -> std::result::Result<AdmissionReceipt, PipeError> {
    let deadline = Instant::now() + WATCHDOG;
    loop {
        match admission.admit(session, sequence, input).await {
            Err(PipeError::Backend(error))
                if error.downcast_ref::<AdmissionRejection>()
                    == Some(&AdmissionRejection::Busy)
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
}

#[test]
#[ignore = "requires an explicitly prepared Linux Podman peer and published data/control ports"]
fn durable_tls_cross_os_faults_reopen_and_independent_peer_ownership() -> Result<()> {
    let container =
        std::env::var("DRASI_NETWORK_CONTAINER").context("prepare the documented Linux peer")?;
    let data_address = std::env::var("DRASI_NETWORK_DATA_ADDRESS")?;
    let control_address = std::env::var("DRASI_NETWORK_CONTROL_ADDRESS")?;
    let bind_data = std::env::var("DRASI_NETWORK_BIND_DATA_ADDRESS")?;
    let bind_control = std::env::var("DRASI_NETWORK_BIND_CONTROL_ADDRESS")?;
    let directory = Directory::new()?;
    let tls_path = directory.join("tls");
    output(
        Command::new("python3")
            .args([HELPER, "certificates"])
            .arg(&tls_path),
        None,
    )?;
    let pin = |name| -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(std::fs::read(tls_path.join(format!("{name}.der")))?)
        ))
    };
    let client_tls = tls(&tls_path, "client", TlsRole::Client, pin("server")?);
    let root = format!("/nq-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mut peer = LinuxPeer {
        container,
        root: root.clone(),
        child: None,
        diagnostics: directory.join("linux.log"),
    };
    let outgoing = BridgeConfig {
        link_id: "network".into(),
        sender_instance: "left".into(),
        receiver_instance: "right".into(),
        sender_endpoint: "bridge-out".into(),
        receiver_endpoint: "bridge-in".into(),
        stream: "sender/out".into(),
        peer_stream: "receiver/in".into(),
        schema: PortableSchema::GraphChange,
        profile: Profile::Durable,
        failure_scope: FailureMode::ProcessRestart,
        namespace: Namespace {
            version: 1,
            source_prefixes: vec!["plant/".into()],
            query_prefixes: vec![],
        },
        transport: Transport::Tls {
            data_address: data_address.clone(),
            control_address,
            server_name: "localhost".into(),
        },
        limits: Limits {
            max_envelope_bytes: 32 * 1024,
            queue_capacity: 2,
            timeout_ms: 1500,
            retry_attempts: 16,
            retry_delay_ms: 50,
            ..Limits::default()
        },
    };
    let mut incoming = outgoing.clone();
    std::mem::swap(&mut incoming.stream, &mut incoming.peer_stream);
    incoming.transport = Transport::Tls {
        data_address: bind_data,
        control_address: bind_control,
        server_name: "localhost".into(),
    };
    let mut config = HostConfig::empty("right", format!("{root}/state"));
    config.configuration_store = Some(ConfigurationStorage {
        key_file: format!("{root}/key").into(),
    });
    config.indexes.insert(
        "persistent".into(),
        drasi_instance_host::config::IndexStorage::Rocksdb,
    );
    bridge(&mut config, &incoming, ComponentRole::Source)?;
    standard(
        &mut config,
        "arithmetic",
        "drasi.standard/arithmetic",
        ComponentRole::Transformer,
        json!({"stream":"right/output","add":10}),
    )?;
    standard(
        &mut config,
        "capture",
        "drasi.standard/capture",
        ComponentRole::Sink,
        json!({"path":format!("{root}/results")}),
    )?;
    config.plugins[0].path = "/build/debug/libdrasi_computation_standard.so".into();
    let server_tls = tls(
        Path::new(&format!("{root}/tls")),
        "server",
        TlsRole::Server,
        pin("client")?,
    );
    let identity = resource(
        &mut config,
        "tls",
        ResourceRole::Identity,
        serde_json::to_value(server_tls)?,
    )?;
    dependency(&mut config, "bridge-in", "tls", identity);
    let journal_definition = definition("receiver/in", "arithmetic")?;
    let journal = resource(
        &mut config,
        "journal",
        ResourceRole::StateStore,
        json!({
            "kind":"qos", "definition":journal_definition, "provider":"persistent",
            "recovery":{"kind":"admission", "component":"bridge-in", "failureScope":"processRestart",
                "maxProducers":1, "receiptsPerProducer":1}
        }),
    )?;
    dependency(&mut config, "bridge-in", "admission", journal.clone());
    connect(&mut config, "bridge-in", "arithmetic")?;
    config
        .desired
        .topology
        .relationships
        .last_mut()
        .unwrap()
        .pipe = DesiredPipe::Qos(journal_definition.pipe(journal, "arithmetic"));
    connect(&mut config, "arithmetic", "capture")?;
    child(
        &mut config,
        "local-child",
        &HostConfig::empty("leaf", format!("{root}/leaf")),
    )?;
    config.stop_stages = vec![
        StopStage::Quiesce {
            components: vec![id("bridge-in"), id("arithmetic"), id("capture")],
        },
        StopStage::StopChild {
            component: id("local-child"),
        },
    ];
    output(
        Command::new("podman").args([
            "exec",
            "-i",
            &peer.container,
            "python3",
            LINUX_HELPER,
            "seed",
            &root,
        ]),
        Some(&serde_json::to_vec(&config)?),
    )?;
    output(
        Command::new("podman")
            .args(["cp"])
            .arg(&tls_path)
            .arg(format!("{}:{root}/tls", peer.container)),
        None,
    )?;
    peer.start(&config)?;
    let initial = peer.wait(|status| {
        status.ready
            && status
                .children
                .iter()
                .any(|child| child.phase == ChildPhase::Ready)
    })?;
    let mut proxy = Proxy(
        Command::new("python3")
            .args([HELPER, "proxy"])
            .arg(&directory.0)
            .arg(&data_address)
            .stdout(Stdio::null())
            .stderr(File::create(directory.join("proxy.log"))?)
            .spawn()?,
    );
    let deadline = Instant::now() + WATCHDOG;
    let proxy_config = loop {
        if let Some(exit) = proxy.0.try_wait()? {
            anyhow::bail!(
                "TLS proxy exited {exit}: {}",
                std::fs::read_to_string(directory.join("proxy.log"))?
            );
        }
        if directory.join("proxy.json").is_file() {
            break serde_json::from_slice::<serde_json::Value>(&std::fs::read(
                directory.join("proxy.json"),
            )?)?;
        }
        ensure!(Instant::now() < deadline, "TLS proxy did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut sender = outgoing.clone();
    if let Transport::Tls { data_address, .. } = &mut sender.transport {
        *data_address = format!(
            "127.0.0.1:{}",
            proxy_config["port"].as_u64().context("proxy port")?
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let provider: Arc<dyn ComputationIndexProvider> =
            Arc::new(RocksDbComputationProvider::new(
                directory.join("sender"),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
                ),
            ));
        let indexes = provider.create_indexes("upstream", "input").await?;
        let codec =
            FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(32 * 1024).unwrap())?;
        let journal = QosChannel::persistent_with_recovery(
            definition("sender/out", "bridge-out")?,
            indexes,
            FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(32 * 1024).unwrap())?,
            "upstream",
            QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: "left".into(),
                graph_id: "__drasi_lib_runtime__".into(),
                component_id: id("input"),
                failure_scope: FailureMode::ProcessRestart,
                max_producers: NonZeroUsize::MIN,
                receipts_per_producer: NonZeroUsize::MIN,
            }),
        )
        .await?;
        let admission = SourceAdmission::new(journal.clone(), PortId::try_new("out")?).await?;
        let control = Arc::new(BridgeEgressControl::default());
        let mut specification = sender.specification(ComponentRole::Sink)?;
        for (name, resource) in [("state", "mapping"), ("tls", "tls"), ("control", "control")] {
            specification
                .dependencies
                .insert(name.into(), vec![ResourceId::try_new(resource)?]);
        }
        let mut builder = ComponentBatch::builder()
            .source(Box::new(AdmissionInput {
                descriptor: ComponentDescriptor::try_new(
                    id("input"),
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                admission: admission.clone(),
            }))
            .component(
                specification,
                drasi_computation_bridge::factories()[1].clone(),
            );
        for (name, role, handle) in [
            ("upstream", ResourceRole::StateStore, journal.resource()),
            (
                "mapping",
                ResourceRole::IndexBackend,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(provider)),
                ),
            ),
            (
                "tls",
                ResourceRole::Identity,
                ResourceHandle::new(ResourceRole::Identity, client_tls.resolve(&directory.0)?),
            ),
            ("control", ResourceRole::Component, control.resource()),
        ] {
            let resource = ResourceId::try_new(name)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: name.into(),
                })?
                .provide_resource(resource, handle)?;
        }
        let batch = builder
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(id("input"), PortId::try_new("out")?),
                    Endpoint::new(id("bridge-out"), PortId::try_new("in")?),
                ),
                Box::new(
                    journal
                        .definition()
                        .pipe(ResourceId::try_new("upstream")?, "bridge-out"),
                ),
            )
            .bind_stream(
                Endpoint::new(id("input"), PortId::try_new("out")?),
                StreamId::try_new("sender/out")?,
            )
            .build()?;
        let core = DrasiLib::builder()
            .with_id("left")
            .with_components(batch)
            .build()
            .await?;
        core.start().await?;
        let session = admission.register_producer(id("caller")).await?;
        admit_when_idle(&admission, &session, 1, &input(1)?).await?;
        let deadline = Instant::now() + WATCHDOG;
        loop {
            let log = std::fs::read_to_string(directory.join("faults.jsonl")).unwrap_or_default();
            if log.contains("\"receipt_held\": true") {
                break;
            }
            ensure!(
                Instant::now() < deadline && proxy.0.try_wait()?.is_none(),
                "proxy did not observe a committed receipt; sender={:?}; Linux={:?}; proxy={}; Linux log={}",
                core.computation_control()?.observed(),
                peer.status(),
                std::fs::read_to_string(directory.join("proxy.log"))?,
                std::fs::read_to_string(&peer.diagnostics)?
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        admit_when_idle(&admission, &session, 2, &input(2)?).await?;
        let full = admit_when_idle(&admission, &session, 3, &input(3)?)
            .await
            .unwrap_err();
        assert!(matches!(full, PipeError::CapacityExhausted), "{full}");
        assert_eq!(journal.progress().await?.accepted, 2);
        let before = Instant::now();
        let live = peer.status()?;
        ensure!(
            before.elapsed() < Duration::from_secs(2) && live.ready,
            "independent Linux control stalled behind data"
        );
        assert_eq!(live.generation, initial.generation);
        tokio::time::sleep(Duration::from_millis(1800)).await;
        std::fs::write(directory.join("release"), b"release the observed receipt")?;
        for sequence in 3..=12 {
            let deadline = Instant::now() + WATCHDOG;
            loop {
                match admit_when_idle(&admission, &session, sequence, &input(sequence)?).await {
                    Ok(_) => break,
                    Err(PipeError::CapacityExhausted) => {
                        ensure!(Instant::now() < deadline, "sender capacity never resumed");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let deadline = Instant::now() + WATCHDOG;
        while journal.progress().await?.processed["bridge-out"] != 12 {
            ensure!(
                Instant::now() < deadline,
                "durable network handling did not finish: {:?}",
                core.computation_control()?.observed()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for recipe in [
            tls(&tls_path, "client", TlsRole::Client, "00".repeat(32)),
            tls(&tls_path, "server", TlsRole::Client, pin("server")?),
        ] {
            recipe.validate()?;
            assert!(BridgeEndpointControl::connect(
                outgoing.clone(),
                ComponentRole::Sink,
                Some(recipe.resolve(&directory.0)?)
            )
            .await
            .is_err());
        }
        let control_graph = core.computation_control()?;
        control_graph
            .quiesce_components(
                control_graph.desired_snapshot().revision,
                GraphSelection::Exact(vec![id("input"), id("bridge-out")]),
            )
            .await?;
        let cut = control.finish(Duration::from_secs(5)).await?;
        assert_eq!(cut.client_sequence, 12);
        core.shutdown().await?;
        journal.shutdown().await?;
        let after = peer.status()?;
        assert_eq!(after.generation, initial.generation);
        ensure!(
            after
                .children
                .iter()
                .any(|child| child.phase == ChildPhase::Ready),
            "local shutdown affected independent Linux ownership"
        );
        peer.stop()?;
        let original = peer.captured()?;
        let mut values = Vec::new();
        for line in std::str::from_utf8(&original)?.lines() {
            for change in GraphChangeCodec::decode_changes(&codec.decode(line.as_bytes())?)? {
                let SourceChange::Insert {
                    element:
                        Element::Node {
                            metadata,
                            properties,
                        },
                } = change
                else {
                    anyhow::bail!("unexpected remote output");
                };
                assert_eq!(metadata.reference.source_id.as_ref(), "plant/input");
                let ElementValue::Integer(value) =
                    properties.get("value").context("native result")?
                else {
                    anyhow::bail!("result is not an integer");
                };
                values.push(*value);
            }
        }
        assert_eq!(
            values,
            (10..22).collect::<Vec<i64>>(),
            "lost receipt/reconnect duplicated or skipped real native results"
        );
        peer.start(&config)?;
        let reopened = peer.wait(|status| {
            status.phase == HostPhase::Running
                && status
                    .components
                    .iter()
                    .any(|value| value.id == "bridge-in" && value.lifecycle == "Running")
        })?;
        assert_ne!(reopened.generation, initial.generation);
        ensure!(
            reopened
                .children
                .iter()
                .all(|child| child.phase != ChildPhase::Ready),
            "reopen undid explicit child stopped intent"
        );
        let mut endpoint = BridgeEndpointControl::connect(
            outgoing,
            ComponentRole::Sink,
            Some(client_tls.resolve(&directory.0)?),
        )
        .await?;
        let restored = endpoint.seal(Duration::from_secs(5)).await?;
        assert_eq!(
            restored.client_sequence, 12,
            "real Linux RocksDB admission was not preserved"
        );
        peer.stop()?;
        // The standard capture fixture truncates its file on reconstruction.
        ensure!(
            peer.captured()?.is_empty(),
            "reopen re-applied acknowledged output"
        );
        let faults = std::fs::read_to_string(directory.join("faults.jsonl"))?;
        ensure!(
            faults.contains("\"receipt_dropped\": true")
                && faults.contains("\"protocol_rejected\": true"),
            "required network fault was not exercised: {faults}"
        );
        Ok::<_, anyhow::Error>(())
    })
}
