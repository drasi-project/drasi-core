// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Port-scoped snapshot consumption through the exported native tables.

use super::*;
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk as sdk;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy)]
enum Claim {
    /// `a` has a handler; `b` has none.
    Exact,
    /// Metadata omits the handler `a` actually registered.
    Missing,
    /// Metadata claims `b`, which has no handler.
    Extra,
    Duplicate,
    Unknown,
    /// A sink cannot have an output port to claim.
    OutputPort,
    NonSink,
}

struct Factory(Claim);

fn ids(names: &[&str]) -> Vec<PortId> {
    names
        .iter()
        .map(|name| PortId::try_new(*name).unwrap())
        .collect()
}

fn port(name: &str, direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(name).unwrap(),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

impl sdk::Factory for Factory {
    fn metadata(&self) -> FactoryMetadata {
        let (role, ports, snapshot_inputs) = match self.0 {
            Claim::Exact => (
                ComponentRole::Sink,
                vec![
                    port("a", PortDirection::Input),
                    port("b", PortDirection::Input),
                ],
                ids(&["a"]),
            ),
            Claim::Missing => (
                ComponentRole::Sink,
                vec![
                    port("a", PortDirection::Input),
                    port("b", PortDirection::Input),
                ],
                Vec::new(),
            ),
            Claim::Extra => (
                ComponentRole::Sink,
                vec![
                    port("a", PortDirection::Input),
                    port("b", PortDirection::Input),
                ],
                ids(&["a", "b"]),
            ),
            Claim::Duplicate => (
                ComponentRole::Sink,
                vec![port("a", PortDirection::Input)],
                ids(&["a", "a"]),
            ),
            Claim::Unknown => (
                ComponentRole::Sink,
                vec![port("a", PortDirection::Input)],
                ids(&["a", "missing"]),
            ),
            Claim::OutputPort => (
                ComponentRole::Sink,
                vec![
                    port("a", PortDirection::Input),
                    port("out", PortDirection::Output),
                ],
                ids(&["out"]),
            ),
            Claim::NonSink => (
                ComponentRole::Transformer,
                vec![
                    port("a", PortDirection::Input),
                    port("out", PortDirection::Output),
                ],
                ids(&["a"]),
            ),
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/snapshot-sink", "1").unwrap(),
            role,
            configuration_version: 1,
            configuration: ConfigSchema {
                fields: BTreeMap::new(),
                allow_additional: true,
            },
            ports,
            completion: (role == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            capabilities: Capabilities {
                snapshot_inputs,
                ..Capabilities::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::ensure!(
            matches!(
                self.0,
                Claim::Exact | Claim::Missing | Claim::Extra | Claim::Duplicate | Claim::Unknown
            ),
            "fixture claim is rejected before construction"
        );
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    request.id.clone(),
                    sdk::Factory::metadata(self).ports,
                )?,
                snapshots: 0,
                handled: 0,
            })),
            control_handler: None,
        })
    }
}

/// Snapshot replacement is registered on input `a` only.
struct Sink {
    descriptor: ComponentDescriptor,
    snapshots: u64,
    handled: u64,
}
#[async_trait]
impl sdk::NativeComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(json!({"snapshots": self.snapshots, "handled": self.handled}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl sdk::NativeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn snapshot_consumer(&mut self, port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        // A misbehaving plugin cannot add an undeclared capability after creation.
        (port.as_str() == "a" || (port.as_str() == "b" && self.snapshots > 0))
            .then_some(self as &mut dyn SnapshotConsumer)
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        self.handled += 1;
        Ok(())
    }
}
#[async_trait]
impl SnapshotConsumer for Sink {
    async fn replace_snapshot(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        self.snapshots += 1;
        Ok(())
    }
}

fn plugin(claim: Claim) -> anyhow::Result<Arc<NativePlugin>> {
    NativePlugin::from_definition(sdk::PluginDefinition::new(
        "native-snapshot-test",
        "1",
        vec![Arc::new(Factory(claim))],
        vec![GraphChangeCodec::schema()],
    )?)
}

fn input(port: &str) -> anyhow::Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new(port)?,
        envelope: super::envelope(&ComponentId::try_new("input")?, 1),
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_snapshot_replacement_is_registered_per_input_and_reaches_the_handler(
) -> anyhow::Result<()> {
    let plugin = plugin(Claim::Exact)?;
    let factory = plugin.factories()[0].clone();
    assert_eq!(factory.metadata().capabilities.snapshot_inputs, ids(&["a"]));
    let mut sink = factory.create_component(ComponentId::try_new("snapshot")?, json!({}))?;
    sink.start().await?;
    let (a, b) = (PortId::try_new("a")?, PortId::try_new("b")?);
    assert!(sink.snapshot_consumer(&b).is_none());
    sink.snapshot_consumer(&a)
        .expect("input a registered a handler")
        .replace_snapshot(input("a")?)
        .await?;
    assert_eq!(sink.configuration()?["snapshots"], 1);

    assert!(SnapshotConsumer::replace_snapshot(&mut sink, input("b")?)
        .await
        .is_err());
    let bytes = sdk::wire::encode(&sdk::wire::Envelope::input(&input("b")?, &sink.codec)?)?;
    assert!(
        sink.inner
            .unit(abi::operation::SNAPSHOT, &bytes)
            .await
            .is_err(),
        "unsupported input snapshot must fail explicitly"
    );

    sink.handle(input("b")?).await?;
    let state = sink.configuration()?;
    assert_eq!(state["snapshots"], 1);
    assert_eq!(state["handled"], 1);
    sink.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sink_without_snapshot_handlers_has_no_snapshot_capability() -> anyhow::Result<()> {
    let plugin = super::plugin();
    let factory = plugin.factories()[2].clone();
    assert!(factory.metadata().capabilities.snapshot_inputs.is_empty());
    let mut sink = factory.create_component(ComponentId::try_new("plain")?, json!({}))?;
    assert!(sink.snapshot_consumer(&PortId::try_new("in")?).is_none());
    let bytes = sdk::wire::encode(&sdk::wire::Envelope::input(&input("in")?, &sink.codec)?)?;
    assert!(sink
        .inner
        .unit(abi::operation::SNAPSHOT, &bytes)
        .await
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_snapshot_claims_must_match_registered_input_handlers() -> anyhow::Result<()> {
    for claim in [Claim::Missing, Claim::Extra] {
        let plugin = plugin(claim)?;
        assert!(
            plugin.factories()[0]
                .create_component(ComponentId::try_new("mismatch")?, json!({}))
                .is_err(),
            "mismatched snapshot metadata created a component"
        );
    }
    for claim in [
        Claim::Duplicate,
        Claim::Unknown,
        Claim::OutputPort,
        Claim::NonSink,
    ] {
        assert!(
            plugin(claim).is_err(),
            "invalid snapshot input claim was accepted"
        );
    }
    Ok(())
}
