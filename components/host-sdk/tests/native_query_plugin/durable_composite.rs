// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

fn query(
    left: &Path,
    right: &Path,
    indexes: &Path,
    generation: u64,
) -> Result<NativeComponentProxy> {
    let plugin = computation::load(sqlite::bootstrap_library())?;
    let bootstrap = plugin
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
        })
        .context("missing file bootstrap")?;
    let mut members = Vec::new();
    for (path, name, table) in [
        (left, "orders", "Orders"),
        (right, "customers", "Customers"),
    ] {
        let mut configuration = sqlite::provider_configuration(path);
        configuration["source"] = json!(name);
        configuration["stream"] = json!(format!("{name}/out"));
        configuration["settings"]["tables"] = json!([table]);
        let provider: Arc<dyn ComputationBootstrapProvider> = bootstrap.create_query_provider(
            ComponentId::try_new(COMPONENT)?,
            configuration,
            scope(generation),
            None,
        )?;
        members.push((ResourceId::try_new(name)?, provider));
    }
    let composite = Arc::new(CompositeBootstrapProvider::new(
        GRAPH,
        GRAPH,
        ComponentId::try_new(COMPONENT)?,
        members,
    )?);
    let mut query = factory()?.create_component_with_processing(ComponentId::try_new(COMPONENT)?,
        json!({"query":"MATCH (o:Orders), (c:Customers) WHERE o.customer=c.id RETURN o.id AS id, c.name AS name, o.amount AS amount",
            "bootstrap":true}), scope(generation), super::super::provider(indexes)?)?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(composite)))?;
    Ok(query)
}

fn joined(snapshot: &OutputSnapshot) -> Result<()> {
    assert_eq!(snapshot.records().len(), 1);
    let row =
        QueryChangeCodec::decode_row(snapshot.records().values().next().expect("joined row"))?;
    assert_eq!(
        QueryChangeCodec::row_values_to_json(&row.values),
        json!({"id":1,"name":"Ada","amount":4})
    );
    assert_eq!(snapshot.cursor().sequence(), 0);
    Ok(())
}

async fn composite_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let left = directory.path().join("orders.sqlite");
    let right = directory.path().join("customers.sqlite");
    let indexes = directory.path().join("indexes");
    rusqlite::Connection::open(&left)?.execute_batch(
        "CREATE TABLE Orders(id INTEGER PRIMARY KEY, customer INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO Orders VALUES(1,7,4);")?;
    rusqlite::Connection::open(&right)?.execute_batch(
        "CREATE TABLE Customers(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO Customers VALUES(7,'Ada');",
    )?;
    let mut loading = query(&left, &right, &indexes, 1)?;
    loading.start().await?;
    joined(&reader(&loading)?.snapshot().await?)?;
    assert!(
        loading
            .bootstrap_state()
            .expect("bootstrap state")
            .read()
            .await?
            .is_none(),
        "recordless file providers must not fabricate handover metadata"
    );
    assert!(loading
        .processing_progress_owner()
        .expect("actual progress owner")
        .snapshot()
        .checkpoints
        .is_empty());
    loading.stop().await?;
    drop(loading);
    let mut restored = query(&left, &right, &indexes, 2)?;
    restored.start().await?;
    joined(&reader(&restored)?.snapshot().await?)?;
    restored.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_composite_bootstrap_joins_actual_providers_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), composite_bootstrap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_composite_bootstrap_joins_actual_providers_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), composite_bootstrap()).await?
}
