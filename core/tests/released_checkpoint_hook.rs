// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{ElementIndex, IndexError},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_query_cypher::CypherParser;
use serde_json::json;
use tokio::sync::Mutex;

#[tokio::test]
async fn zero_argument_checkpoint_hook_remains_source_compatible() {
    let functions = Arc::new(FunctionRegistry::new());
    let index = Arc::new(InMemoryElementIndex::new());
    let query = QueryBuilder::new(
        "MATCH (p:Person) RETURN p.name",
        Arc::new(CypherParser::new(functions)),
    )
    .with_element_index(index.clone())
    .build()
    .await;
    let reference = ElementReference::new("test", "person");
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: reference.clone(),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 1,
            },
            properties: ElementPropertyMap::from(json!({"name": "Alice"})),
        },
    };
    let checkpoint = String::from("owned checkpoint");
    let staged = Arc::new(Mutex::new(None));
    let hook_staged = staged.clone();

    // Registry drasi-lib 0.8.9 passes a move || hook, not a result-taking callback.
    let changes = query
        .process_source_change_with_hook(change, move || async move {
            assert!(index.get_element(&reference).await?.is_some());
            assert!(hook_staged.lock().await.replace(checkpoint).is_none());
            Ok::<(), IndexError>(())
        })
        .await
        .unwrap();

    assert_eq!(staged.lock().await.as_deref(), Some("owned checkpoint"));
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
}
