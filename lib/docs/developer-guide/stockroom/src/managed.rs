// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::management::{ConfigurationStore, DesiredInstance, ManagementResourceResolver};
use drasi_state_store_redb::RedbConfigurationStore;

struct FileFactory(FactoryDescriptor);
struct AlertFactory(FactoryDescriptor);

fn file_factory() -> anyhow::Result<Arc<FileFactory>> {
    Ok(Arc::new(FileFactory(FactoryDescriptor {
        implementation: ImplementationIdentity::try_new("stockroom/file", "1")?,
        role: ComponentRole::Source,
        configuration_version: 1,
        configuration: ConfigurationSchema {
            fields: [(
                "path".into(),
                ConfigurationField {
                    value_type: ConfigurationType::String,
                    required: true,
                    secret: false,
                },
            )]
            .into(),
            allow_additional: false,
        },
        dependencies: Default::default(),
    })))
}

fn alert_factory() -> anyhow::Result<Arc<AlertFactory>> {
    Ok(Arc::new(AlertFactory(FactoryDescriptor {
        implementation: ImplementationIdentity::try_new("stockroom/alerts", "1")?,
        role: ComponentRole::Sink,
        configuration_version: 1,
        configuration: ConfigurationSchema::default(),
        dependencies: Default::default(),
    })))
}

fn source_descriptor() -> anyhow::Result<ComponentDescriptor> {
    descriptor(
        "inventory",
        &[(
            "out",
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
        )],
    )
}

fn alert_descriptor() -> anyhow::Result<ComponentDescriptor> {
    descriptor(
        "alerts",
        &[(
            "in",
            PortDirection::Input,
            QueryChangeCodec::schema().descriptor().clone(),
        )],
    )
}

#[async_trait]
impl ComponentFactory for FileFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        ensure!(
            spec.descriptor == source_descriptor()?,
            "file factory requires the inventory descriptor"
        );
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let build = || -> anyhow::Result<_> {
            let path = context
                .configuration()
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("missing inventory path")?;
            Ok(ConstructedComponent::source(Box::new(Inventory::new(
                Path::new(path),
            )?)))
        };
        build().map_err(ComponentCreationError::terminal)
    }
}

#[async_trait]
impl ComponentFactory for AlertFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        ensure!(
            spec.descriptor == alert_descriptor()?
                && spec.completion == Some(SinkCompletion::Handled),
            "invalid alert descriptor"
        );
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let (view, _) = watch::channel(ResultView::default());
        Ok(ConstructedComponent::sink(Box::new(Alerts {
            descriptor: context.specification.descriptor.clone(),
            query_id: "low-stock",
            view,
            output: tokio::io::stdout(),
            audit: false,
            reorders: false,
            display: true,
        })))
    }
}

struct MemoryResources;

#[async_trait]
impl ManagementResourceResolver for MemoryResources {
    async fn resolve(
        &self,
        _instance: &str,
        _graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle> {
        ensure!(
            specification.id.as_str() == "query-indexes"
                && specification.role == ResourceRole::IndexBackend
                && specification.binding.as_ref() == "query-indexes"
                && configuration == &serde_json::json!({"kind":"memory"}),
            "unsupported managed resource"
        );
        Ok(ResourceHandle::new(
            ResourceRole::IndexBackend,
            Arc::new(QueryIndexProviderResource(Arc::new(
                InMemoryComputationProvider,
            ))),
        ))
    }
}

fn desired(
    path: &Path,
    threshold: Option<i64>,
    factories: &FactoryRegistry,
) -> anyhow::Result<DesiredInstance> {
    let source = factories.get_by_name("stockroom/file", Some("1"))?.clone();
    let sink = factories
        .get_by_name("stockroom/alerts", Some("1"))?
        .clone();
    let query = factories
        .get_by_name("drasi/continuous-query", Some("1"))?
        .clone();
    let source_spec = ComponentSpecification {
        descriptor: source_descriptor()?,
        role: ComponentRole::Source,
        completion: None,
        implementation: source.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: [(
            "path".into(),
            ConfigurationValue::Literal(serde_json::json!(path)),
        )]
        .into(),
        dependencies: Default::default(),
    };
    let sink_spec = ComponentSpecification {
        descriptor: alert_descriptor()?,
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        implementation: sink.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: Default::default(),
        dependencies: Default::default(),
    };
    let text = threshold.map_or_else(
        || LOW_STOCK.to_owned(),
        |threshold| {
            format!(
        "MATCH (p:Product) WHERE p.quantity < {threshold} RETURN p.sku AS sku, p.quantity AS quantity"
    )
        },
    );
    let indexes = ResourceId::try_new("query-indexes")?;
    let mut query_spec = query.specification_from_config(
        ComponentId::try_new("low-stock")?,
        serde_json::json!({"query":text, "stream":"low-stock/out"}),
    )?;
    query_spec
        .dependencies
        .insert("indexes".into(), vec![indexes.clone()]);
    let mut batch = ComponentBatch::builder()
        .component(source_spec, source)
        .component(query_spec, query)
        .component(sink_spec, sink)
        .declare_resource(ResourceSpecification {
            id: indexes.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "query-indexes".into(),
        })?
        .resource_configuration(indexes, serde_json::json!({"kind":"memory"}))?;
    for producer in ["inventory", "low-stock"] {
        batch = batch.bind_stream(
            endpoint(producer, "out")?,
            StreamId::try_new(format!("{producer}/out"))?,
        );
    }
    for (producer, consumer) in [("inventory", "low-stock"), ("low-stock", "alerts")] {
        batch = batch.connect(
            EdgeDefinition::new(endpoint(producer, "out")?, endpoint(consumer, "in")?),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        );
    }
    // ANCHOR: desired-from-batch
    let batch = batch.build()?;
    DesiredInstance::from(batch.definition).normalized()
    // ANCHOR_END: desired-from-batch
}

async fn open(
    path: &Path,
    store: Arc<dyn ConfigurationStore>,
    threshold: Option<i64>,
) -> anyhow::Result<(DrasiLib, cli::Health)> {
    let mut factories = FactoryRegistry::standard();
    factories.register(file_factory()?)?;
    factories.register(alert_factory()?)?;
    // ANCHOR: managed-builder
    let core = DrasiLib::builder()
        .with_id("stockroom-managed")
        .with_component_factories(factories.clone())
        .with_management_resources(Arc::new(MemoryResources))
        .with_configuration_store(store)
        .build()
        .await?;
    // ANCHOR_END: managed-builder
    let setup = async {
        let mut health = cli::Health::new(&core)?;
        let current = core.desired_configuration()?;
        if current.revision == 0 || threshold.is_some() {
            let request = core.new_configuration_request_id().await?;
            core.apply_desired_state(
                current.revision,
                request,
                desired(path, threshold, &factories)?,
            )
            .await?;
        }
        ensure!(
            core.reconcile_desired_state().await?.converged(),
            "managed definition did not converge"
        );
        core.start().await?;
        for component in ["inventory", "low-stock", "alerts"] {
            cli::wait_started(&core, component, Duration::from_secs(10)).await?;
        }
        health.check()?;
        Ok(health)
    }
    .await;
    let health = match setup {
        Ok(health) => health,
        Err(error) => return finish(&core, Err(error)).await,
    };
    Ok((core, health))
}

pub(super) async fn run(
    directory: &Path,
    key_file: &Path,
    threshold: Option<i64>,
    shutdown: &mut cli::Shutdown,
) -> anyhow::Result<()> {
    let key: [u8; 32] = tokio::fs::read(key_file)
        .await?
        .try_into()
        .map_err(|_| anyhow::anyhow!("configuration key must contain exactly 32 bytes"))?;
    tokio::fs::create_dir_all(directory).await?;
    let path = tokio::fs::canonicalize("inventory.json").await?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.join("configuration.redb"),
        key,
    )?);
    println!("Starting managed Stockroom.");
    let (core, mut health) = open(&path, store, threshold).await?;
    let work = async {
        println!(
            "Ready at saved configuration revision {}. Edit inventory.json; Ctrl-C stops.",
            core.desired_configuration()?.revision
        );
        tokio::select! {
            biased;
            result = health.failed() => result?,
            result = shutdown.requested() => result?,
        }
        health.check()
    }
    .await;
    finish(&core, work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn configuration_reopens_without_resubmitting_the_seed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        tokio::fs::write(&path, "[]").await?;
        let database = directory.path().join("configuration.redb");
        let key = [19; 32];
        let (core, _health) = open(
            &path,
            Arc::new(RedbConfigurationStore::new(&database, key)?),
            Some(10),
        )
        .await?;
        let saved = core.desired_configuration()?;
        core.shutdown().await?;
        drop(core);
        let (core, _health) = open(
            &path,
            Arc::new(RedbConfigurationStore::new(&database, key)?),
            None,
        )
        .await?;
        let work = async {
            let reopened = core.desired_configuration()?;
            ensure!(reopened.revision == saved.revision);
            ensure!(reopened.desired == saved.desired);
            let control = core.computation_control()?;
            let preview = control.preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::Restart(GraphSelection::Exact(vec![
                        ComponentId::try_new("alerts")?,
                    ]))],
                )
                .await?;
            let error = control.reconcile(preview, TopologyBindings::default()).await
                .expect_err("managed mutation must use desired-state ownership");
            ensure!(error.to_string().contains("managed configuration must be changed"));
            let mut next = reopened.desired;
            let query = next.topology.components.iter_mut()
                .find(|component| component.descriptor.id().as_str() == "low-stock")
                .context("saved topology omitted query")?;
            let ComponentConstruction::Factory(spec) = &mut query.construction else {
                anyhow::bail!("query is not reconstructible");
            };
            spec.configuration.insert("query".into(), ConfigurationValue::Literal(
                serde_json::json!("MATCH (p:Product) WHERE p.quantity < 6 RETURN p.sku AS sku, p.quantity AS quantity"),
            ));
            core.apply_desired_state(reopened.revision, "change-threshold", next).await?;
            ensure!(core.reconcile_desired_state().await?.converged());
            ensure!(core.desired_configuration()?.revision > saved.revision);
            Ok(())
        }
        .await;
        finish(&core, work).await
    }
}
