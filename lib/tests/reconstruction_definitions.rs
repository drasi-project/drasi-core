// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use serde_json::json;

struct Inspectable(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Inspectable {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(json!({"setting":"inspectable, not a reconstruction recipe"}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Inspectable {
    async fn run(&mut self) -> Result<()> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn inspectable_objects_are_not_promoted_to_empty_factory_recipes() -> Result<()> {
    let id = ComponentId::try_new("application-object")?;
    let mut graph = ComputationGraph::builder("recipes")
        .service(Box::new(Inspectable(ComponentDescriptor::try_new(
            id.clone(),
            Vec::new(),
        )?)))
        .build()?;
    let snapshot = graph.configuration_snapshot()?;
    assert!(matches!(
        &snapshot.configurations[&id],
        CapturedComponentConfiguration::Available { values }
            if values["setting"] == "inspectable, not a reconstruction recipe"
    ));
    let error = snapshot
        .reconstructible_topology()
        .expect_err("no factory identity");
    assert!(error.to_string().contains("inspectable"), "{error}");
    assert!(matches!(
        snapshot.topology.components[0].construction,
        ComponentConstruction::External { .. }
    ));
    graph.dispose().await?;
    Ok(())
}

#[test]
fn reconstruction_keeps_unavailable_implementations_and_secret_references() -> Result<()> {
    let mut topology = drasi_lib::management::DesiredInstance::default().topology;
    let id = ComponentId::try_new("source")?;
    let secret = ResourceId::try_new("secrets")?;
    let descriptor = ComponentDescriptor::try_new(id.clone(), Vec::new())?
        .with_semantic_kind(ComponentSemanticKind::Source);
    topology.components.push(DesiredComponent {
        descriptor: descriptor.clone(),
        role: ComponentRole::Service,
        completion: None,
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::default(),
        construction: ComponentConstruction::Factory(ComponentSpecification {
            descriptor,
            role: ComponentRole::Service,
            completion: None,
            implementation: ImplementationIdentity::try_new("unavailable/source", "17")?,
            configuration_version: 4,
            configuration: BTreeMap::from([(
                Arc::from("password"),
                ConfigurationValue::Reference {
                    resource: secret.clone(),
                    key: "secret:password".into(),
                    secret: true,
                },
            )]),
            dependencies: BTreeMap::new(),
        }),
    });
    topology.resources.push(ResourceSpecification {
        id: secret.clone(),
        role: ResourceRole::SecretStore,
        ownership: ResourceOwnership::Graph,
        binding: "configuration".into(),
    });
    topology
        .resource_configurations
        .insert(secret, json!({"kind":"configuration"}));
    let snapshot = GraphConfigurationSnapshot {
        topology: topology.clone(),
        configurations: BTreeMap::from([(
            id,
            CapturedComponentConfiguration::Available {
                values: json!({"password":"resolved-private-value"}),
            },
        )]),
    };
    let exported = snapshot.reconstructible_topology()?;
    assert_eq!(exported, topology);
    let serialized = serde_json::to_string(&exported)?;
    assert!(serialized.contains("secret:password"));
    assert!(!serialized.contains("resolved-private-value"));
    Ok(())
}

#[test]
fn opaque_resources_remain_external_even_with_descriptive_bindings() -> Result<()> {
    let mut topology = drasi_lib::management::DesiredInstance::default().topology;
    topology.resources.push(ResourceSpecification {
        id: ResourceId::try_new("database")?,
        role: ResourceRole::IndexBackend,
        ownership: ResourceOwnership::Borrowed,
        binding: "rocksdb:/not-a-construction-recipe".into(),
    });
    let snapshot = GraphConfigurationSnapshot {
        topology,
        configurations: BTreeMap::new(),
    };
    let error = snapshot
        .reconstructible_topology()
        .expect_err("binding is not a recipe");
    assert!(error.to_string().contains("database"), "{error}");
    Ok(())
}
