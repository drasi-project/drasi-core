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

#![cfg(feature = "computation")]

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    computation::v1::*, ComponentStatus, Source, SourceBase, SourceBaseParams,
    SourceRuntimeContext, SourceSubscriptionSettings, SubscriptionResponse,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Default)]
struct Calls {
    initialize: AtomicUsize,
    start: AtomicUsize,
    stop: AtomicUsize,
    exited: AtomicUsize,
    deprovision: AtomicUsize,
}

struct SourceFixture {
    base: Arc<SourceBase>,
    calls: Arc<Calls>,
}

#[async_trait]
impl Source for SourceFixture {
    fn id(&self) -> &str {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "owned-test-source"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        panic!("source properties can contain secrets and must not be inspected")
    }
    fn supports_replay(&self) -> bool {
        false
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.calls.initialize.fetch_add(1, Ordering::SeqCst);
        self.base.initialize(context).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.calls.start.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.base.set_shutdown_tx(sender).await;
        let calls = self.calls.clone();
        self.base
            .set_task_handle(tokio::spawn(async move {
                receiver.await.expect("owned worker shutdown");
                tokio::task::yield_now().await;
                calls.exited.fetch_add(1, Ordering::SeqCst);
            }))
            .await;
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        self.base.stop_common().await
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.calls.deprovision.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        assert!(!settings.enable_bootstrap);
        assert!(!settings.request_position_handle);
        assert!(settings.resume_from.is_none());
        assert!(settings.resume_sequence.is_none());
        assert!(settings.query_id.starts_with("computation:"));
        self.base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct SinkFixture {
    descriptor: ComponentDescriptor,
    sender: mpsc::Sender<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for SinkFixture {
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
impl EnvelopeSink for SinkFixture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.sender
            .send(input.envelope)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

#[tokio::test]
async fn owned_legacy_source_has_exact_lifecycle_and_awaited_worker_cleanup() {
    let calls = Arc::new(Calls::default());
    let base = Arc::new(SourceBase::new(SourceBaseParams::new("raw-source")).expect("source base"));
    let resource = Arc::new(LegacySourceResource::owned(Box::new(SourceFixture {
        base: base.clone(),
        calls: calls.clone(),
    })));
    let resource_id = ResourceId::try_new("owned-source").expect("id");
    let component = ComponentId::try_new("adapter").expect("id");
    let stream = StreamId::try_new("adapter/out").expect("stream");
    let output = Endpoint::new(component.clone(), PortId::try_new("out").expect("port"));
    let input = Endpoint::new(
        ComponentId::try_new("sink").expect("id"),
        PortId::try_new("in").expect("port"),
    );
    let schema = GraphChangeCodec::schema();
    let factory = Arc::new(LegacySourceFactory::default());
    let (sender, mut receiver) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("owned-legacy")
        .component(
            ComponentSpecification {
                descriptor: ComponentDescriptor::try_new(
                    component,
                    vec![PortDescriptor::new(
                        output.port.clone(),
                        PortDirection::Output,
                        schema.descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )
                .expect("descriptor"),
                role: ComponentRole::Source,
                completion: None,
                implementation: factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::from([(
                    Arc::from("stream"),
                    ConfigurationValue::Literal(serde_json::json!(stream.as_str())),
                )]),
                dependencies: BTreeMap::from([(Arc::from("source"), vec![resource_id.clone()])]),
            },
            factory,
        )
        .declare_resource(ResourceSpecification {
            id: resource_id.clone(),
            role: ResourceRole::LegacySource,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from("fresh-source"),
        })
        .expect("declaration")
        .provide_resource(
            resource_id,
            ResourceHandle::new(ResourceRole::LegacySource, resource.clone())
                .with_cleanup(resource),
        )
        .expect("owned source cleanup registered")
        .sink(Box::new(SinkFixture {
            descriptor: ComponentDescriptor::try_new(
                input.component.clone(),
                vec![PortDescriptor::new(
                    input.port.clone(),
                    PortDirection::Input,
                    schema.descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("sink"),
            sender,
        }))
        .bind_stream(output.clone(), stream.clone())
        .connect(
            EdgeDefinition::new(output, input),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    assert_eq!(calls.initialize.load(Ordering::SeqCst), 0);
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        assert_eq!(calls.initialize.load(Ordering::SeqCst), 1);
        assert_eq!(calls.start.load(Ordering::SeqCst), 1);
        base.dispatch_source_change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("raw-source", "one"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::new(),
            },
        })
        .await
        .expect("dispatch");
        let envelope = receiver.recv().await.expect("wrapped event");
        assert_eq!(envelope.system().stream(), &stream);
        assert_eq!(
            GraphChangeCodec::source_metadata(&envelope)
                .expect("metadata")
                .expect("source")
                .source_id,
            "raw-source"
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(calls.stop.load(Ordering::SeqCst), 1);
    assert_eq!(calls.exited.load(Ordering::SeqCst), 1);
    graph
        .dispose()
        .await
        .expect("owned resource already cleaned once");
    assert_eq!(calls.stop.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.deprovision.load(Ordering::SeqCst),
        0,
        "no implicit permanent external deletion"
    );
}
