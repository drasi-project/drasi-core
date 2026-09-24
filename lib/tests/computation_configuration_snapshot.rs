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

use async_trait::async_trait;
use drasi_lib::{computation::v1::*, DrasiLib, Query};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component ID")
}

struct Configured {
    descriptor: ComponentDescriptor,
    setting: String,
    reads: Arc<AtomicUsize>,
    entered: Arc<Notify>,
}

impl Configured {
    fn new(name: &str, setting: &str) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(id(name), vec![]).expect("descriptor"),
            setting: setting.into(),
            reads: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl ComputationComponent for Configured {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"setting": self.setting}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn reconfigure(&mut self, context: ConstructionContext) -> anyhow::Result<()> {
        self.setting = context.configuration()["setting"]
            .as_str()
            .expect("validated string")
            .to_owned();
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Configured {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

fn values(snapshot: &GraphConfigurationSnapshot, name: &str) -> serde_json::Value {
    let CapturedComponentConfiguration::Available { values } = &snapshot.configurations[&id(name)]
    else {
        panic!("configuration must be available for {name}");
    };
    values.clone()
}

#[tokio::test]
async fn instance_snapshot_covers_ordinary_native_and_additional_graphs_without_pausing_work() {
    let core = DrasiLib::builder()
        .with_id("configuration-owner")
        .with_query(
            Query::cypher("ordinary")
                .query("RETURN 1")
                .auto_start(false)
                .build(),
        )
        .build()
        .await
        .expect("instance");
    let component = Configured::new("native", "private-property-value");
    let entered = component.entered.clone();
    let reads = component.reads.clone();
    let handle = core
        .add_computation_component(ComponentAddition::new(ConstructedComponent::service(
            Box::new(component),
        )))
        .await
        .expect("native declaration");
    handle.wait_created().await.expect("native creation");
    handle.start().await.expect("native start");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("service is holding its processing lease");
    let extra = ComputationGraph::builder("extra")
        .service(Box::new(Configured::new("extra-service", "second")))
        .build()
        .expect("extra graph");
    core.add_computation_graph(extra, ComputationOptions { auto_start: false })
        .await
        .expect("register extra graph")
        .deployment()
        .await
        .expect("extra creation");

    let before = reads.load(Ordering::SeqCst);
    let snapshot = tokio::time::timeout(
        Duration::from_secs(5),
        core.snapshot_computation_configuration(),
    )
    .await
    .expect("configuration export must not await a processing boundary")
    .expect("configuration");
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.instance.instance_id, "configuration-owner");
    assert_eq!(snapshot.instance.queries[0].config.query, "RETURN 1");
    let root = snapshot.native_components.as_ref().expect("native root");
    assert_eq!(
        root.topology.components.len(),
        1,
        "ordinary components are not duplicated"
    );
    assert_eq!(
        values(root, "native"),
        json!({"setting": "private-property-value"})
    );
    assert_eq!(snapshot.graphs.len(), 1);
    assert!(!snapshot.graphs[0].options.auto_start);
    assert_eq!(snapshot.graphs[0].graph.topology.graph_id, "extra");
    assert_eq!(
        values(&snapshot.graphs[0].graph, "extra-service"),
        json!({"setting": "second"})
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "export reads a coherent publication"
    );
    let inspected = format!(
        "{:?}",
        core.computation_control().unwrap().inspector().topology()
    );
    assert!(!inspected.contains("private-property-value"));
    let encoded = serde_json::to_string(&snapshot).expect("encode privileged configuration");
    let decoded: InstanceConfigurationSnapshot = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(
        values(decoded.native_components.as_ref().unwrap(), "native"),
        values(root, "native")
    );
    core.shutdown().await.expect("shutdown");
}

struct Factory {
    descriptor: FactoryDescriptor,
    fail: bool,
}

impl Factory {
    fn new(fail: bool) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/configuration", "1")
                    .expect("implementation"),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        "setting".into(),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            fail,
        }
    }
    fn specification(&self, setting: &str) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id("configured"), vec![]).expect("descriptor"),
            role: ComponentRole::Service,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                "setting".into(),
                ConfigurationValue::Literal(setting.into()),
            )]),
            dependencies: BTreeMap::new(),
        }
    }
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn supports_reconfiguration(&self) -> bool {
        true
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        if self.fail {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "construction rejected"
            )));
        }
        Ok(ConstructedComponent::service(Box::new(Configured::new(
            context.component_id.as_str(),
            context.configuration()["setting"]
                .as_str()
                .expect("setting"),
        ))))
    }
}

#[tokio::test]
async fn factory_definition_survives_creation_failure_and_exports_without_an_instance() {
    let factory = Arc::new(Factory::new(true));
    let definition = factory.specification("retain-me");
    let graph = ComputationGraph::builder("failed-config")
        .component(definition.clone(), factory)
        .build()
        .unwrap();
    assert!(matches!(
        graph.configuration_snapshot().unwrap().configurations[&id("configured")],
        CapturedComponentConfiguration::Declared { .. }
    ));
    let core = DrasiLib::builder().build().await.unwrap();
    let handle = core
        .add_computation_graph(graph, ComputationOptions { auto_start: false })
        .await
        .unwrap();
    let report = handle.deployment().await.unwrap();
    assert!(matches!(
        report.components[&id("configured")],
        CreationOutcome::CreationFailed(_)
    ));
    let snapshot = core.snapshot_computation_configuration().await.unwrap();
    let captured = &snapshot.graphs[0].graph;
    assert!(matches!(
        &captured.configurations[&id("configured")],
        CapturedComponentConfiguration::Declared { values } if values == &definition.configuration
    ));
    assert!(matches!(
        &captured.topology.components[0].construction,
        ComponentConstruction::Factory(spec) if spec == &definition
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn in_place_configuration_changes_publish_with_the_new_revision_not_old_snapshots() {
    let factory = Arc::new(Factory::new(false));
    let mut graph = ComputationGraph::builder("updates")
        .component(factory.specification("before"), factory.clone())
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let before = control.configuration_snapshot().unwrap();
        assert_eq!(values(&before, "configured"), json!({"setting":"before"}));
        let mut updated = before.topology.components[0].clone();
        updated.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                before.topology.revision,
                vec![DesiredMutation::UpdateComponent(updated)],
            )
            .await
            .unwrap();
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        let after = control.configuration_snapshot().unwrap();
        assert!(after.topology.revision > before.topology.revision);
        assert_eq!(values(&after, "configured"), json!({"setting":"after"}));
        assert_eq!(values(&before, "configured"), json!({"setting":"before"}));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn builtin_config_getters_preserve_definitions_and_do_not_include_processing_state() {
    let definition = MiddlewareTransformerDefinition {
        id: id("middleware"),
        output_stream: StreamId::try_new("middleware/out").unwrap(),
        middleware: vec![],
        pipeline: vec![],
    };
    let mut middleware = MiddlewareTransformer::new(
        definition,
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
    )
    .unwrap();
    let before = middleware.configuration().unwrap();
    middleware.start().await.unwrap();
    assert_eq!(before, middleware.configuration().unwrap());
    assert_eq!(
        before,
        json!({"stream":"middleware/out","middleware":[],"pipeline":[]})
    );
    middleware.stop().await.unwrap();

    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "config".into(),
            id: id("query"),
            query: "MATCH (n) RETURN n".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").unwrap(),
            outbox_capacity: std::num::NonZeroUsize::new(9).unwrap(),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .unwrap();
    let before = query.configuration().unwrap();
    query.start().await.unwrap();
    assert_eq!(before, query.configuration().unwrap());
    assert_eq!(before["query"], "MATCH (n) RETURN n");
    assert_eq!(before["outbox_capacity"], 9);
    assert!(before.get("rows").is_none());
    query.stop().await.unwrap();
}

#[test]
fn resource_recipes_roundtrip_with_desired_topology_and_require_declared_resources() {
    let resource = ResourceId::try_new("indexes").unwrap();
    let recipe = json!({"kind":"rocksdbIndexes","path":"data/native"});
    let graph = ComputationGraph::builder("resource-config")
        .service(Box::new(Configured::new("service", "value")))
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "indexes".into(),
        })
        .unwrap()
        .resource_configuration(resource.clone(), recipe.clone())
        .unwrap()
        .build()
        .unwrap();
    let exported = graph.configuration_snapshot().unwrap();
    assert_eq!(exported.topology.resource_configurations[&resource], recipe);
    let encoded = exported.topology.to_json().unwrap();
    let decoded = DesiredTopology::from_json(&encoded).unwrap();
    assert_eq!(decoded.resource_configurations[&resource], recipe);
    assert!(ComputationGraph::builder("undeclared")
        .service(Box::new(Configured::new("service", "value")))
        .resource_configuration(resource, recipe)
        .unwrap()
        .build()
        .is_err());
}

#[test]
fn opaque_components_export_an_explicit_configuration_error_not_an_empty_configuration() {
    struct Opaque(ComponentDescriptor);
    #[async_trait]
    impl ComputationComponent for Opaque {
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
    impl ComputationService for Opaque {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }
    let graph = ComputationGraph::builder("opaque")
        .service(Box::new(Opaque(
            ComponentDescriptor::try_new(id("opaque"), vec![]).unwrap(),
        )))
        .build()
        .unwrap();
    let snapshot = graph.configuration_snapshot().unwrap();
    assert!(matches!(
        &snapshot.configurations[&id("opaque")],
        CapturedComponentConfiguration::Unavailable { reason } if reason.contains("does not expose")
    ));
    assert!(matches!(
        &snapshot.topology.components[0].construction,
        ComponentConstruction::External { binding } if binding == "opaque"
    ));
}

#[tokio::test]
async fn resource_recipes_survive_component_updates_but_never_follow_a_rebound_provider() {
    let factory = Arc::new(Factory::new(false));
    let resource = ResourceId::try_new("provider").unwrap();
    let recipe = json!({"kind":"example","setting":"old-provider"});
    let mut graph = ComputationGraph::builder("provider-recipes")
        .component(factory.specification("before"), factory.clone())
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "provider".into(),
        })
        .unwrap()
        .resource_configuration(resource.clone(), recipe.clone())
        .unwrap()
        .provide_resource(
            resource.clone(),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(1usize)),
        )
        .unwrap()
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let before = control.configuration_snapshot().unwrap();
        let mut changed = before.topology.components[0].clone();
        changed.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                before.topology.revision,
                vec![DesiredMutation::UpdateComponent(changed)],
            )
            .await
            .unwrap();
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        let updated = control.configuration_snapshot().unwrap();
        assert_eq!(updated.topology.resource_configurations[&resource], recipe);
        let preview = control
            .preview(
                updated.topology.revision,
                vec![DesiredMutation::RebindResource(resource.clone())],
            )
            .await
            .unwrap();
        assert!(!preview
            .desired()
            .resource_configurations
            .contains_key(&resource));
        let mut bindings = TopologyBindings::default();
        bindings.resources.insert(
            resource.clone(),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(2usize)),
        );
        control.reconcile(preview, bindings).await.unwrap();
        assert!(!control
            .configuration_snapshot()
            .unwrap()
            .topology
            .resource_configurations
            .contains_key(&resource));
        assert_eq!(before.topology.resource_configurations[&resource], recipe);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}
