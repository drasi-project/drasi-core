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

#![cfg(feature = "computation")]

use drasi_lib::{computation::v1::*, DrasiLib};
use drasi_plugin_sdk::computation::{
    PluginConfiguration, ReactionPluginFactory, SourcePluginFactory,
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn existing_descriptors_construct_native_graph_plugins_without_an_abi_change() {
    let drasi = DrasiLib::builder()
        .with_id("sdk-computation")
        .build()
        .await
        .expect("instance");
    let pipeline = drasi.computation_pipeline("descriptors").expect("pipeline");
    let source = SourcePluginFactory::new(
        Arc::new(drasi_source_mock::descriptor::MockSourceDescriptor),
        "mock",
        PluginConfiguration::new(
            "mock",
            "1.0.0",
            serde_json::json!({"dataType":{"type":"counter"},"intervalMs":10}),
        ),
        true,
    )
    .expect("source descriptor");
    let host = source
        .host(pipeline.services())
        .await
        .expect("source factory");
    let reaction = ReactionPluginFactory::new(
        Arc::new(drasi_reaction_log::descriptor::LogReactionDescriptor),
        "log",
        vec!["query".into()],
        PluginConfiguration::new("log", "1.0.0", serde_json::json!({})),
        true,
    )
    .expect("reaction descriptor");
    let reaction = reaction
        .host(
            pipeline.services(),
            pipeline.catalog(),
            ReactionPluginOptions {
                bootstrap_timeout_secs: 5,
                ..Default::default()
            },
        )
        .await
        .expect("reaction factory");
    let catalog = pipeline.catalog();
    let mut output = catalog.subscribe();
    let graph = pipeline
        .source(host, SourceSubscriptionOptions::default())
        .expect("source")
        .query(
            drasi_lib::Query::cypher("query")
                .query("MATCH (n) RETURN count(n) AS total")
                .from_source("mock")
                .enable_bootstrap(false)
                .build(),
        )
        .reaction(reaction, true)
        .build()
        .expect("graph");
    drasi
        .add_computation_graph(graph, ComputationOptions::default())
        .await
        .expect("register");
    drasi.start().await.expect("start");
    let event = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("source did not produce")
        .expect("output");
    assert_eq!(
        QueryChangeCodec::metadata(&event)
            .expect("metadata")
            .query_id,
        "query"
    );
    drasi.shutdown().await.expect("owned plugin shutdown");
}

#[test]
fn configuration_versions_and_schema_fail_before_plugin_construction() {
    let descriptor = Arc::new(drasi_source_mock::descriptor::MockSourceDescriptor);
    assert!(SourcePluginFactory::new(
        descriptor.clone(),
        "mock",
        PluginConfiguration::new("mock", "99.0.0", serde_json::json!({})),
        true
    )
    .is_err());
    assert!(SourcePluginFactory::new(
        descriptor,
        "mock",
        PluginConfiguration::new(
            "mock",
            "1.0.0",
            serde_json::json!({"dataType":{"type":"not-real"}})
        ),
        true
    )
    .is_err());
}

#[test]
fn original_reference_configuration_remains_unresolved_in_the_recipe() {
    let config = serde_json::json!({"intervalMs":{"kind":"EnvironmentVariable","name":"DRASI_UNUSED_TEST_INTERVAL","default":"10"}});
    let source = SourcePluginFactory::new(
        Arc::new(drasi_source_mock::descriptor::MockSourceDescriptor),
        "mock",
        PluginConfiguration::new("mock", "1.0.0", config.clone()),
        true,
    )
    .expect("unresolved configuration");
    assert_eq!(source.configuration().config, config);
}

struct ResolvingDescriptor(std::sync::Mutex<Vec<(String, String)>>);
#[async_trait::async_trait]
impl drasi_plugin_sdk::descriptor::SourcePluginDescriptor for ResolvingDescriptor {
    fn kind(&self) -> &str {
        "resolver-probe"
    }
    fn config_version(&self) -> &str {
        "1"
    }
    fn config_schema_name(&self) -> &str {
        "Probe"
    }
    fn config_schema_json(&self) -> String {
        serde_json::json!({"Probe":{"type":"object","required":["secret"],"properties":{"secret":{"$ref":"#/components/schemas/ConfigValueString"}}}}).to_string()
    }
    async fn create_source(
        &self,
        id: &str,
        config: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
        let mapper = drasi_plugin_sdk::mapper::DtoMapper::new();
        tokio::task::yield_now().await;
        let value = mapper
            .resolve_string(&serde_json::from_value(config["secret"].clone())?)
            .await?;
        self.0.lock().expect("values").push((id.to_owned(), value));
        drasi_source_mock::descriptor::MockSourceDescriptor
            .create_source(id, &serde_json::json!({}), auto_start)
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_descriptor_creation_uses_instance_secrets_without_global_resolver_mutation() {
    use drasi_plugin_sdk::{mapper::DtoMapper, ConfigValue};
    let descriptor = Arc::new(ResolvingDescriptor(std::sync::Mutex::new(Vec::new())));
    let recipe = PluginConfiguration::new(
        "resolver-probe",
        "1",
        serde_json::json!({"secret":{"kind":"Secret","name":"same-key"}}),
    );
    let one = DrasiLib::builder()
        .with_id("first-instance")
        .with_secret_store_provider(Arc::new(
            drasi_lib::secret_store::MemorySecretStoreProvider::new()
                .with_secret("same-key", "first-value"),
        ))
        .build()
        .await
        .expect("first instance");
    let two = DrasiLib::builder()
        .with_id("second-instance")
        .with_secret_store_provider(Arc::new(
            drasi_lib::secret_store::MemorySecretStoreProvider::new()
                .with_secret("same-key", "second-value"),
        ))
        .build()
        .await
        .expect("second instance");
    let first = SourcePluginFactory::new(descriptor.clone(), "one", recipe.clone(), true)
        .expect("first recipe");
    let second = SourcePluginFactory::new(descriptor.clone(), "two", recipe.clone(), true)
        .expect("second recipe");
    let (a, b) = tokio::join!(
        first.host(
            one.computation_plugin_services("graph")
                .expect("first services")
        ),
        second.host(
            two.computation_plugin_services("graph")
                .expect("second services")
        ),
    );
    let a = a.expect("first source");
    let b = b.expect("second source");
    let mut values = descriptor.0.lock().expect("values").clone();
    values.sort();
    assert_eq!(
        values,
        vec![
            ("one".into(), "first-value".into()),
            ("two".into(), "second-value".into())
        ]
    );
    assert_eq!(first.configuration().config, recipe.config);
    assert!(
        DtoMapper::new()
            .resolve_string(&ConfigValue::Secret {
                name: "same-key".into()
            })
            .await
            .is_err(),
        "scoped resolvers must not leak to unrelated descriptor calls"
    );
    a.shutdown().await.expect("first owner");
    b.shutdown().await.expect("second owner");
    one.shutdown().await.expect("first shutdown");
    two.shutdown().await.expect("second shutdown");
}

#[tokio::test]
async fn existing_bootstrap_identity_secret_and_index_descriptors_keep_their_real_contracts() {
    use drasi_core::computation::ComputationIndexProvider;
    use drasi_plugin_sdk::computation::{
        create_identity_provider, create_scoped_index_provider, create_secret_store,
        BootstrapPluginFactory,
    };
    let temp = tempfile::tempdir().expect("temp");
    let file = temp.path().join("secrets.json");
    tokio::fs::write(&file, r#"{"fixture-key":"fixture-value"}"#)
        .await
        .expect("fixture file");
    let secrets = create_secret_store(
        Arc::new(drasi_secret_store_file::FileSecretStoreDescriptor),
        &PluginConfiguration::new("file", "1.0.0", serde_json::json!({"path":file})),
    )
    .await
    .expect("existing secret plugin");
    assert_eq!(
        secrets.get_secret("fixture-key").await.expect("secret"),
        "fixture-value"
    );
    let identity = create_identity_provider(
        Arc::new(drasi_identity_test::TestIdentityProviderDescriptor),
        &PluginConfiguration::new("test", "1.0.0", serde_json::json!({})),
    )
    .await
    .expect("existing identity plugin");
    let credentials = identity
        .get_credentials(&drasi_lib::identity::CredentialContext::new())
        .await
        .expect("credentials");
    assert_eq!(
        credentials
            .try_into_auth_pair()
            .expect("password identity")
            .0,
        "test-user"
    );
    let drasi = DrasiLib::builder()
        .with_identity_provider(identity)
        .with_secret_store_provider(secrets)
        .build()
        .await
        .expect("instance");
    let services = drasi
        .computation_plugin_services("families")
        .expect("services");
    let bootstrap = BootstrapPluginFactory::new(
        Arc::new(drasi_bootstrap_noop::descriptor::NoOpBootstrapDescriptor),
        PluginConfiguration::new("noop", "1.0.0", serde_json::json!({})),
        serde_json::json!({}),
    )
    .expect("bootstrap recipe")
    .create_with_services(&services)
    .await
    .expect("existing bootstrap plugin");
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let result = bootstrap
        .bootstrap(
            drasi_lib::bootstrap::BootstrapRequest {
                query_id: "query".into(),
                node_labels: vec![],
                relation_labels: vec![],
                request_id: "request".into(),
            },
            &drasi_lib::bootstrap::BootstrapContext::new_minimal(
                services.scope.to_string(),
                "source".into(),
            ),
            sender,
            None,
        )
        .await
        .expect("bootstrap");
    assert_eq!(result.event_count, 0);
    assert!(receiver.recv().await.is_none());
    let provider = create_scoped_index_provider(
        Arc::new(drasi_index_rocksdb::RocksDbIndexDescriptor),
        &PluginConfiguration::new(
            "rocksdb",
            "1.1.0",
            serde_json::json!({
                "path":temp.path().join("indexes"), "memoryBudgetBytes":32 * 1024 * 1024,
            }),
        ),
        &services,
    )
    .await
    .expect("existing index plugin");
    let indexes = provider
        .create_indexes("families", "query")
        .await
        .expect("native index resources");
    assert!(
        indexes.atomic_result_transaction().is_err(),
        "legacy writers must not acquire an invented shared transaction"
    );
    indexes
        .cleanup()
        .expect("I/O owner")
        .shutdown()
        .await
        .expect("I/O cleanup");
    drop(indexes);
    provider.shutdown().await.expect("provider cleanup");
    drasi.shutdown().await.expect("instance cleanup");
}
