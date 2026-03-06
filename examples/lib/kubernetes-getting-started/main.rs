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

use anyhow::Result;
use drasi_bootstrap_kubernetes::KubernetesBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_kubernetes::{
    AuthMode, KubernetesSource, KubernetesSourceConfig, ResourceSpec, StartFrom,
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let source_config = KubernetesSourceConfig {
        resources: vec![ResourceSpec {
            api_version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        }],
        namespaces: vec!["default".to_string()],
        auth_mode: AuthMode::Kubeconfig,
        kubeconfig_path: default_kubeconfig_path(),
        include_owner_relations: false,
        start_from: StartFrom::Beginning,
        ..Default::default()
    };

    let bootstrap_provider = KubernetesBootstrapProvider::builder()
        .with_source_config(source_config.clone())
        .build()?;

    let source = KubernetesSource::builder("k8s-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("configmap-monitor")
        .query(
            r#"
            MATCH (c:ConfigMap)
            WHERE c.name = 'drasi-demo'
            RETURN c.name AS name, c.namespace AS namespace, c.data.color AS color
            "#,
        )
        .from_source("k8s-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let log_reaction = LogReaction::builder("k8s-log")
        .with_query("configmap-monitor")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "[ADD] {{after.namespace}}/{{after.name}} color={{after.color}}",
            )),
            updated: Some(TemplateSpec::new(
                "[UPDATE] {{after.namespace}}/{{after.name}} {{before.color}} -> {{after.color}}",
            )),
            deleted: Some(TemplateSpec::new(
                "[DELETE] {{before.namespace}}/{{before.name}}",
            )),
        })
        .build()?;

    let core = DrasiLib::builder()
        .with_id("kubernetes-getting-started")
        .with_source(source)
        .with_query(query)
        .with_reaction(log_reaction)
        .build()
        .await?;

    println!("Kubernetes getting-started example running.");
    println!("Use ./test-updates.sh in another shell to create/update/delete drasi-demo ConfigMap.");
    println!("Press Ctrl+C to stop.");

    core.start().await?;
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}

fn default_kubeconfig_path() -> Option<String> {
    if let Ok(kubeconfig) = std::env::var("KUBECONFIG") {
        return Some(kubeconfig);
    }
    std::env::var("HOME")
        .ok()
        .map(|home| format!("{home}/.kube/config"))
}
