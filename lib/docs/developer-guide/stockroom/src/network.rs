// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

fn library(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!(
        "{}{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ))
}

// ANCHOR: load-native-factory
fn load_factory(
    directory: &Path,
    package: &str,
    implementation: &str,
) -> anyhow::Result<Arc<drasi_host_sdk::computation::NativeFactory>> {
    let path = library(directory, package);
    let plugin = drasi_host_sdk::computation::load(&path)
        .with_context(|| format!("load trusted native library {}", path.display()))?;
    plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == implementation
                && factory.metadata().implementation.version.as_ref() == "1"
        })
        .cloned()
        .with_context(|| format!("native library does not export {implementation} version 1"))
}

// ANCHOR_END: load-native-factory

pub(super) fn connect(
    mut batch: ComponentBatchBuilder,
    directory: &Path,
    query_id: &str,
) -> anyhow::Result<ComponentBatchBuilder> {
    for (package, implementation, id, configuration) in [
        (
            "drasi_computation_source_http",
            "drasi.network/http-source",
            "inventory",
            serde_json::json!({
                "stream":"inventory/out", "sourceId":"inventory",
                "host":"127.0.0.1", "port":9080,
                "ingressCapacity":32, "maxMessageBytes":65536
            }),
        ),
        (
            "drasi_computation_sink_sse",
            "drasi.network/sse-sink",
            "notifications",
            serde_json::json!({
                "queryStreams":{query_id:format!("{query_id}/out")},
                "host":"127.0.0.1", "port":9081, "ssePath":"/events",
                "broadcastCapacity":32
            }),
        ),
    ] {
        let factory = load_factory(directory, package, implementation)?;
        let specification = factory.specification(ComponentId::try_new(id)?, configuration)?;
        batch = batch.component(specification, factory);
    }
    Ok(batch.connect(
        EdgeDefinition::new(endpoint(query_id, "out")?, endpoint("notifications", "in")?),
        Box::new(BoundedPipeConfig { capacity: 8 }),
    ))
}

// ANCHOR: live-webhook
pub(super) async fn add_webhook(
    core: &DrasiLib,
    directory: &Path,
    url: &str,
) -> anyhow::Result<()> {
    let factory = load_factory(
        directory,
        "drasi_computation_sink_http",
        "drasi.network/http-sink",
    )?;
    let specification = factory.specification(
        ComponentId::try_new("webhook")?,
        serde_json::json!({
            "queryId":"low-stock", "stream":"low-stock/out", "url":url,
            "timeoutMs":5000, "maxRetries":0, "failurePolicy":"strict"
        }),
    )?;
    let component = core
        .add_computation_component(
            ComponentAddition::from_specification(specification, factory).auto_start(false),
        )
        .await?;
    let report = core
        .computation_control()?
        .connect(
            EdgeDefinition::new(endpoint("low-stock", "out")?, endpoint("webhook", "in")?),
            Box::new(BoundedPipeConfig { capacity: 8 }),
            RelationshipPolicy::default(),
        )
        .await?;
    if report.summary != OperationSummary::Completed {
        return Err(LifecycleReportError::Reconciliation(report).into());
    }
    tokio::time::timeout(Duration::from_secs(10), component.start()).await??;
    println!("Connected live webhook destination: {url}");
    Ok(())
}

// ANCHOR_END: live-webhook

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    webhook: Option<String>,
}

pub(super) async fn watch_destination(
    core: &DrasiLib,
    directory: &Path,
    shutdown: &mut cli::Shutdown,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    let mut connected: Option<String> = None;
    let mut last_error: Option<String> = None;
    println!(
        "Ready. Edit inventory.json; set a webhook URL in destination.json to add a destination."
    );
    loop {
        tokio::select! {
            result = shutdown.requested() => { result?; return Ok(()); }
            _ = interval.tick() => {}
        }
        let read = async {
            let text = tokio::fs::read_to_string("destination.json").await?;
            Ok::<Destination, anyhow::Error>(serde_json::from_str(&text)?)
        }
        .await;
        let destination = match read {
            Ok(value) => {
                last_error = None;
                value
            }
            Err(error) => {
                let message = error.root_cause().to_string();
                if last_error.as_ref() != Some(&message) {
                    eprintln!("Destination unchanged: {message}");
                    last_error = Some(message);
                }
                continue;
            }
        };
        match (&connected, destination.webhook) {
            (None, Some(url)) => {
                add_webhook(core, directory, &url).await?;
                connected = Some(url);
            }
            (Some(old), Some(url)) if old != &url =>
                anyhow::bail!("this lesson adds one destination; use reconciliation to replace an existing webhook"),
            (Some(_), None) =>
                anyhow::bail!("this lesson adds one destination; use an explicit removal policy to disconnect it"),
            _ => {}
        }
    }
}
