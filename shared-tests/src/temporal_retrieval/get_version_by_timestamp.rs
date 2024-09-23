#![allow(clippy::unwrap_used)]
use std::sync::Arc;

use crate::QueryTestConfig;

use drasi_core::{
    evaluation::context::QueryPartEvaluationContext,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use serde_json::json;

macro_rules! variablemap {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::BTreeMap::new();
         $( map.insert($key.to_string().into(), $val); )*
         map
    }}
  }

pub fn test_query() -> &'static str {
    "MATCH 
        (t:Thing)
    RETURN
        t as now,
        drasi.getVersionByTimestamp(t, 999) as v0,
        drasi.getVersionByTimestamp(t, 1000) as v1,
        drasi.getVersionByTimestamp(t, 1001) as v1_1,
        drasi.getVersionByTimestamp(t, 2001) as v2
    "
}

pub async fn get_version_by_timestamp(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let mut builder = QueryBuilder::new(test_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let v0 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 0 })),
    };

    let v1 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 1000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 1 })),
    };

    let v2 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 2000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 2 })),
    };

    // bootstrap
    {
        _ = query
            .process_source_change(SourceChange::Insert {
                element: v0.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v1.clone(),
            })
            .await
            .unwrap();
    }

    let change = SourceChange::Update {
        element: v2.clone(),
    };

    let result = query.process_source_change(change.clone()).await.unwrap();
    assert_eq!(result.len(), 1);

    let after = match result[0] {
        QueryPartEvaluationContext::Updating { ref after, .. } => after,
        _ => panic!("Expected Updating"),
    };

    assert_eq!(
        after,
        &variablemap!(
        "now" => v2.to_expression_variable(),
        "v0" => v0.to_expression_variable(),
        "v1" => v1.to_expression_variable(),
        "v1_1" => v1.to_expression_variable(),
        "v2" => v2.to_expression_variable()
        )
    );
}
