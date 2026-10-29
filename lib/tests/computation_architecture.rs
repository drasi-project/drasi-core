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

#![cfg(test)]

#[test]
fn generic_graph_modules_do_not_depend_on_legacy_runtime_contracts() {
    let files = [
        ("graph", include_str!("../src/computation/v1/graph.rs")),
        (
            "controller",
            include_str!("../src/computation/v1/graph/controller.rs"),
        ),
        (
            "addition",
            include_str!("../src/computation/v1/graph/addition.rs"),
        ),
        (
            "reconcile",
            include_str!("../src/computation/v1/graph/reconcile.rs"),
        ),
        (
            "resources",
            include_str!("../src/computation/v1/graph/resources.rs"),
        ),
        (
            "registry",
            include_str!("../src/computation/v1/graph/registry.rs"),
        ),
        (
            "specification",
            include_str!("../src/computation/v1/graph/specification.rs"),
        ),
        (
            "topology",
            include_str!("../src/computation/v1/graph/topology.rs"),
        ),
        (
            "entities",
            include_str!("../src/computation/v1/entities.rs"),
        ),
    ];
    for (name, source) in files {
        for dependency in [
            "crate::component_graph",
            "crate::context",
            "crate::sources",
            "crate::reactions",
            "LegacyStateStoreResource",
            "LegacyIdentityResource",
            "LegacyBootstrapResource",
            "LegacyWalResource",
            "LegacySecretStoreResource",
            "SourcePluginAdapterFactory",
            "ReactionPluginAdapterFactory",
            "drasi/lib-runtime-component",
        ] {
            assert!(
                !source.contains(dependency),
                "{name} depends on compatibility detail {dependency}"
            );
        }
    }
}

#[tokio::test]
async fn shared_status_and_query_contracts_preserve_legacy_import_paths() {
    use std::sync::Arc;

    let (sender, mut receiver) =
        tokio::sync::mpsc::channel::<drasi_lib::component_graph::ComponentUpdate>(1);
    let handle: drasi_lib::channels::ComponentStatusHandle =
        drasi_lib::component_graph::ComponentStatusHandle::new_wired("component", sender);
    handle
        .set_status(drasi_lib::ComponentStatus::Running, None)
        .await;
    assert!(matches!(
        receiver.recv().await,
        Some(drasi_lib::channels::ComponentUpdate::Status {
            status: drasi_lib::ComponentStatus::Running,
            ..
        })
    ));
    let _: fn(
        Arc<dyn drasi_lib::queries::traits::Query>,
    ) -> Arc<dyn drasi_lib::queries::manager::Query> = |query| query;
}
