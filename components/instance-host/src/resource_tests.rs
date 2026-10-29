// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::management::{DesiredInstance, NoManagementResources};
use std::sync::atomic::{AtomicBool, Ordering};

fn specification(name: &str, role: ResourceRole) -> ResourceSpecification {
    ResourceSpecification {
        id: ResourceId::try_new(name).expect("test resource"),
        role,
        ownership: ResourceOwnership::Graph,
        binding: name.into(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn control_lookup_is_weak_and_namespaces_cannot_be_shared() -> Result<()> {
    let resources = InstanceResources::new(Arc::new(NoManagementResources), ".");
    let spec = specification("ingress", ResourceRole::Component);
    let config = serde_json::json!({"kind":"bridgeIngressControl"});
    let handle = resources.resolve("one", "graph", &spec, &config).await?;
    let actual = handle.get::<BridgeIngressControl>()?;
    assert!(Arc::ptr_eq(&actual, &resources.ingress(&spec.id)?));
    assert!(resources
        .resolve("other", "graph", &spec, &config)
        .await
        .is_err());
    drop(actual);
    drop(handle);
    assert!(resources.ingress(&spec.id).is_err());
    Ok(())
}

struct Guard(AtomicBool);

#[async_trait]
impl ManagementResourceResolver for Guard {
    fn validate_transition(
        &self,
        previous: &DesiredTopology,
        desired: &DesiredTopology,
    ) -> Result<()> {
        self.0.store(true, Ordering::Release);
        ensure!(
            previous.resources.len() == 1 && desired.resources.len() == 1,
            "ordinary declarations were not preserved"
        );
        anyhow::bail!("ordinary recovery guard refused transition")
    }

    async fn resolve(
        &self,
        _: &str,
        _: &str,
        _: &ResourceSpecification,
        _: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        anyhow::bail!("not used")
    }
}

#[test]
fn ordinary_recovery_policy_is_not_replaced_by_control_recipes() -> Result<()> {
    let guard = Arc::new(Guard(AtomicBool::new(false)));
    let resources = InstanceResources::new(guard.clone(), ".");
    let mut topology = DesiredInstance::default().topology;
    for (name, role, config) in [
        (
            "ordinary",
            ResourceRole::IndexBackend,
            serde_json::json!({"kind":"memoryIndexes"}),
        ),
        (
            "control",
            ResourceRole::Component,
            serde_json::json!({"kind":"bridgeIngressControl"}),
        ),
    ] {
        let spec = specification(name, role);
        topology
            .resource_configurations
            .insert(spec.id.clone(), config);
        topology.resources.push(spec);
    }
    let error = resources
        .validate_transition(&topology, &topology)
        .expect_err("guard must run");
    assert!(error.to_string().contains("ordinary recovery guard"));
    assert!(guard.0.load(Ordering::Acquire));
    Ok(())
}

#[test]
fn tls_validation_does_not_read_keys_and_in_place_recipe_changes_are_rejected() -> Result<()> {
    let resources = InstanceResources::new(Arc::new(NoManagementResources), ".");
    let mut topology = DesiredInstance::default().topology;
    let spec = specification("tls", ResourceRole::Identity);
    topology.resources.push(spec.clone());
    topology.resource_configurations.insert(
        spec.id.clone(),
        serde_json::json!({
            "kind":"bridgeTls", "version":1, "role":"server",
            "trust_roots":["not-opened.der"], "certificate_chain":["not-opened.der"],
            "private_key":"not-opened.pk8", "peer_certificate_sha256":"0".repeat(64)
        }),
    );
    resources.validate_transition(&topology, &topology)?;
    let mut changed = topology.clone();
    changed
        .resource_configurations
        .get_mut(&spec.id)
        .expect("TLS recipe")["role"] = "client".into();
    assert!(resources.validate_transition(&topology, &changed).is_err());
    let mut invalid = topology.clone();
    invalid.resources[0].role = ResourceRole::Component;
    assert!(resources
        .validate_transition(&DesiredInstance::default().topology, &invalid)
        .is_err());
    Ok(())
}
