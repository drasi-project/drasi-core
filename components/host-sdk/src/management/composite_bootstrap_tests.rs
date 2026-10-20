// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component")
}
fn resource(name: &str) -> ResourceId {
    ResourceId::try_new(name).expect("resource")
}
fn specification() -> ResourceSpecification {
    ResourceSpecification {
        id: resource("combined"),
        role: ResourceRole::Bootstrap,
        ownership: ResourceOwnership::Graph,
        binding: "combined".into(),
    }
}
fn config() -> CompositeBootstrapConfig {
    CompositeBootstrapConfig {
        component: id("query"),
        providers: vec![resource("postgres"), resource("http")],
    }
}
struct Child {
    owner: ComponentId,
    reader: Option<SourceProgressReader>,
}
#[async_trait]
impl ComputationBootstrapProvider for Child {
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        component: &ComponentId,
    ) -> Result<()> {
        anyhow::ensure!(
            instance == "instance" && graph == "graph" && component == &self.owner,
            "foreign bootstrap scope"
        );
        Ok(())
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.reader.clone()
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::empty()),
            watermarks: Vec::new(),
        })
    }
}
fn child(owner: &str, reader: Option<SourceProgressReader>) -> ResourceHandle {
    ResourceHandle::new(
        ResourceRole::Bootstrap,
        Arc::new(QueryBootstrapResource(Arc::new(Child {
            owner: id(owner),
            reader,
        }))),
    )
}
fn dependencies(progress: Arc<QuerySourceProgress>) -> BTreeMap<ResourceId, ResourceHandle> {
    BTreeMap::from([
        (resource("postgres"), child("query", Some(progress.into()))),
        (resource("http"), child("query", None)),
    ])
}

#[tokio::test]
async fn composite_recipe_resolves_actual_ordered_children_and_query_owner() -> Result<()> {
    let owner = Arc::new(QuerySourceProgress::new("graph", id("query"))?);
    let config = config();
    let dependencies = dependencies(owner.clone());
    let handle = config.resolve("instance", "graph", &specification(), &dependencies)?;
    let bootstrap = handle.get::<QueryBootstrapResource>()?;
    assert!(bootstrap
        .0
        .recovery_reader()
        .expect("actual reader")
        .same_owner(&owner.into()));
    assert_eq!(bootstrap.0.prepare().await?, BootstrapPreparation::Ready);
    assert!(bootstrap.0.snapshot().await?.watermarks.is_empty());
    bootstrap.0.stop().await?;
    let encoded = serde_json::to_value(&config)?;
    let decoded: CompositeBootstrapConfig = serde_json::from_value(encoded)?;
    assert_eq!(decoded.providers, config.providers);
    let recipe: Recipe = serde_json::from_value(serde_json::json!({
        "kind":"compositeBootstrap","component":"query","providers":["postgres","http"],
    }))?;
    assert!(matches!(recipe, Recipe::CompositeBootstrap(_)));
    Ok(())
}

#[test]
fn composite_recipe_rejects_missing_foreign_alias_and_wrong_type_dependencies() -> Result<()> {
    let actual = Arc::new(QuerySourceProgress::new("graph", id("query"))?);
    let config = config();
    let valid = dependencies(actual);
    let mut missing = valid.clone();
    missing.remove(&resource("http"));
    assert!(config
        .resolve("instance", "graph", &specification(), &missing)
        .is_err());
    let mut wrong_type = valid.clone();
    wrong_type.insert(
        resource("http"),
        ResourceHandle::new(ResourceRole::Bootstrap, Arc::new("not a provider")),
    );
    assert!(config
        .resolve("instance", "graph", &specification(), &wrong_type)
        .is_err());
    let mut foreign = valid.clone();
    foreign.insert(resource("http"), child("other-query", None));
    assert!(config
        .resolve("instance", "graph", &specification(), &foreign)
        .is_err());
    let mut impostor = valid.clone();
    impostor.insert(
        resource("http"),
        child(
            "query",
            Some(Arc::new(QuerySourceProgress::new("graph", id("query"))?).into()),
        ),
    );
    assert!(config
        .resolve("instance", "graph", &specification(), &impostor)
        .is_err());
    let mut alias = valid.clone();
    alias.insert(resource("http"), valid[&resource("postgres")].clone());
    assert!(config
        .resolve("instance", "graph", &specification(), &alias)
        .is_err());
    assert!(config
        .resolve("other-instance", "graph", &specification(), &valid)
        .is_err());
    assert!(config
        .resolve("instance", "other-graph", &specification(), &valid)
        .is_err());
    Ok(())
}

#[test]
fn composite_recipe_requires_exact_bounded_nonrecursive_bootstrap_membership() -> Result<()> {
    let valid = config();
    let dependencies = valid
        .providers
        .iter()
        .map(|provider| (provider.clone(), ResourceRole::Bootstrap))
        .collect();
    valid.validate(&specification(), &dependencies)?;
    for providers in [
        Vec::new(),
        vec![resource("postgres"), resource("postgres")],
        vec![resource("combined")],
        (0..=MAX_COMPOSITE_BOOTSTRAP_PROVIDERS)
            .map(|index| resource(&format!("member-{index}")))
            .collect(),
    ] {
        let config = CompositeBootstrapConfig {
            providers,
            component: id("query"),
        };
        let dependencies = config
            .providers
            .iter()
            .map(|provider| (provider.clone(), ResourceRole::Bootstrap))
            .collect();
        assert!(config.validate(&specification(), &dependencies).is_err());
    }
    let mut borrowed = specification();
    borrowed.ownership = ResourceOwnership::Borrowed;
    assert!(valid.validate(&borrowed, &dependencies).is_err());
    let wrong_roles = valid
        .providers
        .iter()
        .map(|provider| (provider.clone(), ResourceRole::Checkpoint))
        .collect();
    assert!(valid.validate(&specification(), &wrong_roles).is_err());
    Ok(())
}
