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

mod mock_source;

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{channels::ComponentStatus, DrasiLib, Query, Source};
use mock_source::{MockSource, MockSourceHandle};
use serde_json::{json, Value};

async fn wait_for_rows(lib: &DrasiLib, expected: &[Value]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = lib.get_query_status("paths").await?;
            if status == ComponentStatus::Error {
                bail!("query entered Error");
            }
            if status == ComponentStatus::Running
                && lib.get_query_results("paths").await? == expected
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("query did not reach the expected result set")?
}

fn middle(label: &str) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("first", "middle"),
            labels: Arc::new([Arc::from(label)]),
            effective_from: chrono::Utc::now().timestamp_millis() as u64,
        },
        properties: ElementPropertyMap::from(json!({"id": "middle"})),
    }
}

async fn source_with_script() -> Result<(MockSource, MockSourceHandle)> {
    let (source, handle) = MockSource::new("first")?;
    source
        .set_bootstrap_provider(Box::new(
            ScriptFileBootstrapProvider::builder()
                .with_file(format!(
                    "{}/tests/fixtures/variable_length_bootstrap.jsonl",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .build(),
        ))
        .await;
    Ok((source, handle))
}

#[tokio::test]
async fn bootstrap_and_live_updates_include_intermediate_nodes() -> Result<()> {
    let (first, handle) = source_with_script().await?;
    let (second, _second_handle) = MockSource::new("second")?;
    let mut query = Query::cypher("paths")
        .query("MATCH (a:Start)-[:R*2]->(b:End) RETURN b.id AS end")
        .from_source("first")
        .from_source("second")
        .build();
    query.sources[0].relations = vec!["R".into()];
    query.sources[1].nodes = vec!["Foreign".into()];
    let lib = DrasiLib::builder()
        .with_source(first)
        .with_source(second)
        .with_query(query)
        .build()
        .await?;
    let result: Result<()> = async {
        lib.start().await?;
        wait_for_rows(&lib, &[json!({"end": "b"})]).await?;

        handle
            .send(SourceChange::Update {
                element: middle("Foreign"),
            })
            .await?;
        wait_for_rows(&lib, &[]).await?;

        handle
            .send(SourceChange::Update {
                element: middle("Transit"),
            })
            .await?;
        wait_for_rows(&lib, &[json!({"end": "b"})]).await?;

        handle
            .send(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("first", "middle"),
                    labels: Arc::new([]),
                    effective_from: chrono::Utc::now().timestamp_millis() as u64,
                },
            })
            .await?;
        wait_for_rows(&lib, &[]).await?;

        handle
            .send(SourceChange::Insert {
                element: middle("Transit"),
            })
            .await?;
        wait_for_rows(&lib, &[json!({"end": "b"})]).await?;

        let external_node = |label| Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("second", "external"),
                ..middle(label).get_metadata().clone()
            },
            properties: ElementPropertyMap::new(),
        };
        handle
            .send(SourceChange::Insert {
                element: external_node("Transit"),
            })
            .await?;
        for (id, from, to) in [
            (
                "external-first",
                ElementReference::new("first", "a"),
                ElementReference::new("second", "external"),
            ),
            (
                "external-second",
                ElementReference::new("second", "external"),
                ElementReference::new("first", "b"),
            ),
        ] {
            handle
                .send(SourceChange::Insert {
                    element: Element::Relation {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("first", id),
                            ..middle("R").get_metadata().clone()
                        },
                        in_node: from,
                        out_node: to,
                        properties: ElementPropertyMap::new(),
                    },
                })
                .await?;
        }
        wait_for_rows(&lib, &[json!({"end": "b"}), json!({"end": "b"})]).await?;
        handle
            .send(SourceChange::Update {
                element: external_node("Foreign"),
            })
            .await?;
        wait_for_rows(&lib, &[json!({"end": "b"})]).await
    }
    .await;
    let stopped = lib.stop().await;
    result?;
    stopped?;
    Ok(())
}

#[cfg(feature = "middleware-relabel")]
#[tokio::test]
async fn fixed_query_selection_runs_after_label_changing_middleware() -> Result<()> {
    use drasi_core::models::SourceMiddlewareConfig;

    let (source, handle) = source_with_script().await?;
    let mut query = Query::cypher("paths")
        .query("MATCH (n:User) RETURN n.id AS end")
        .from_source("first")
        .with_middleware(SourceMiddlewareConfig {
            name: "rename".into(),
            kind: "relabel".into(),
            config: [(
                "labelMappings".into(),
                json!({"Person": "User", "Transit": "User"}),
            )]
            .into_iter()
            .collect(),
        })
        .build();
    query.sources[0].pipeline = vec!["rename".into()];
    let lib = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await?;
    let result = async {
        lib.start().await?;
        wait_for_rows(&lib, &[json!({"end": "middle"})]).await?;
        handle
            .send(SourceChange::Delete {
                metadata: ElementMetadata {
                    labels: Arc::new([]),
                    ..middle("").get_metadata().clone()
                },
            })
            .await?;
        wait_for_rows(&lib, &[]).await?;
        handle
            .send_node_insert("n", vec!["Person"], json!({"id": "n"}).into())
            .await?;
        wait_for_rows(&lib, &[json!({"end": "n"})]).await
    }
    .await;
    let stopped = lib.stop().await;
    result?;
    stopped?;
    Ok(())
}
