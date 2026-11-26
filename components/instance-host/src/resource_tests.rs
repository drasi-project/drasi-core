// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

struct Journals;

#[async_trait]
impl ManagementResourceResolver for Journals {
    fn validate_transition(&self, _: &DesiredTopology, _: &DesiredTopology) -> Result<()> {
        Ok(())
    }

    async fn resolve(
        &self,
        _: &str,
        _: &str,
        _: &ResourceSpecification,
        _: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("receiver/in")?,
            capacity: std::num::NonZeroUsize::new(4).expect("nonzero capacity"),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("capture".into(), SubscriptionStart::Earliest)]),
        };
        Ok(QosChannel::volatile(definition)?.resource())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn journal_lookup_is_weak_and_namespaces_cannot_be_shared() -> Result<()> {
    let resources = InstanceResources::new(Arc::new(Journals));
    let spec = ResourceSpecification {
        id: ResourceId::try_new("journal")?,
        role: ResourceRole::StateStore,
        ownership: ResourceOwnership::Graph,
        binding: "journal".into(),
    };
    let config = serde_json::json!({"kind":"qos"});
    let handle = resources.resolve("one", "graph", &spec, &config).await?;
    let actual = handle.get::<QosChannel>()?;
    assert!(Arc::ptr_eq(&actual, &resources.journal(&spec.id)?));
    assert!(resources
        .resolve("other", "graph", &spec, &config)
        .await
        .is_err());
    drop(actual);
    drop(handle);
    assert!(resources.journal(&spec.id).is_err());
    Ok(())
}
