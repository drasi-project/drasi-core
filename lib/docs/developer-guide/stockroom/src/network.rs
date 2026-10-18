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
        let path = library(directory, package);
        let plugin = drasi_host_sdk::computation::load(&path)
            .with_context(|| format!("load trusted native library {}", path.display()))?;
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == implementation)
            .with_context(|| format!("native library does not export {implementation}"))?
            .clone();
        let specification = factory.specification(ComponentId::try_new(id)?, configuration)?;
        batch = batch.component(specification, factory);
    }
    Ok(batch.connect(
        EdgeDefinition::new(endpoint(query_id, "out")?, endpoint("notifications", "in")?),
        Box::new(BoundedPipeConfig { capacity: 8 }),
    ))
}
