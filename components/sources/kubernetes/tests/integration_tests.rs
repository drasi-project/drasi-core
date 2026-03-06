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

mod k3s_helpers;

use std::time::Duration;

use anyhow::Result;
use drasi_bootstrap_kubernetes::KubernetesBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::{Subscription, SubscriptionOptions};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_kubernetes::{
    AuthMode, KubernetesSource, KubernetesSourceConfig, ResourceSpec, StartFrom,
};
use k3s_helpers::K3sGuard;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::Api;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{sleep, Instant};

#[tokio::test]
#[ignore]
async fn test_kubernetes_source_configmap_crud_cycle() -> Result<()> {
    let k3s = K3sGuard::start().await?;
    let config = KubernetesSourceConfig {
        resources: vec![ResourceSpec {
            api_version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        }],
        namespaces: vec!["default".to_string()],
        auth_mode: AuthMode::Kubeconfig,
        kubeconfig_path: None,
        kubeconfig_content: Some(k3s.kubeconfig().to_string()),
        include_owner_relations: false,
        start_from: StartFrom::Now,
        ..Default::default()
    };

    let bootstrap_provider = KubernetesBootstrapProvider::builder()
        .with_source_config(config.clone())
        .build()?;

    let source = KubernetesSource::builder("k8s-source")
        .with_config(config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("q1")
        .query("MATCH (c:ConfigMap) RETURN c.name AS name, c.data.color AS color")
        .from_source("k8s-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, reaction_handle) = ApplicationReaction::builder("r1").with_query("q1").build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("k8s-source-it")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;
    sleep(Duration::from_secs(2)).await; // allow watchers to enter steady state

    let mut subscription = reaction_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(1)))
        .await?;

    let client = k3s.client().await?;
    let cms: Api<ConfigMap> = Api::namespaced(client, "default");

    let cm_name = "drasi-k8s-it-cm";
    let cm: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": cm_name
        },
        "data": {
            "color": "red"
        }
    }))?;

    cms.create(&PostParams::default(), &cm).await?;

    wait_for_query_row(&core, "q1", |rows| row_has(rows, cm_name, Some("red"))).await?;
    wait_for_subscription_data(&mut subscription, "ADD", cm_name, Some("red")).await?;

    cms.patch(
        cm_name,
        &PatchParams::default(),
        &Patch::Merge(json!({
            "data": { "color": "blue" }
        })),
    )
    .await?;

    wait_for_query_row(&core, "q1", |rows| row_has(rows, cm_name, Some("blue"))).await?;
    wait_for_subscription_data(&mut subscription, "UPDATE", cm_name, Some("blue")).await?;

    cms.delete(cm_name, &DeleteParams::default()).await?;
    wait_for_subscription_data(&mut subscription, "DELETE", cm_name, None).await?;
    wait_for_query_row(&core, "q1", |rows| !row_has(rows, cm_name, None)).await?;

    core.stop().await?;
    Ok(())
}

async fn wait_for_query_row<F>(core: &Arc<DrasiLib>, query_id: &str, predicate: F) -> Result<()>
where
    F: Fn(&[Value]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rows = core.get_query_results(query_id).await?;
        if predicate(&rows) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow::anyhow!("Timed out waiting for query condition"));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn row_has(rows: &[Value], name: &str, color: Option<&str>) -> bool {
    rows.iter().any(|row| {
        let row_name = row.get("name").and_then(Value::as_str);
        if row_name != Some(name) {
            return false;
        }
        if let Some(expected) = color {
            return row.get("color").and_then(Value::as_str) == Some(expected);
        }
        true
    })
}

async fn wait_for_subscription_data(
    subscription: &mut Subscription,
    diff_type: &str,
    name: &str,
    color: Option<&str>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(result) = subscription.recv().await {
            for diff in &result.results {
                let value = match diff {
                    drasi_lib::channels::ResultDiff::Add { data } if diff_type == "ADD" => data,
                    drasi_lib::channels::ResultDiff::Update { data, .. }
                        if diff_type == "UPDATE" =>
                    {
                        data
                    }
                    drasi_lib::channels::ResultDiff::Delete { data } if diff_type == "DELETE" => {
                        data
                    }
                    _ => continue,
                };

                let row_name = value.get("name").and_then(Value::as_str);
                if row_name != Some(name) {
                    continue;
                }

                if let Some(expected) = color {
                    let row_color = value.get("color").and_then(Value::as_str);
                    if row_color == Some(expected) {
                        return Ok(());
                    }
                    continue;
                }

                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "Timed out waiting for subscription data for name={name}"
            ));
        }
    }
}
