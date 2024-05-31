use std::sync::Arc;

use drasi_query_ast::ast;
use drasi_core::models::SourceMiddlewareConfig;
use serde_json::json;

pub fn remap_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
  MATCH (v:Vehicle)
  RETURN
    v.id,
    v.currentSpeed
    ",
    )
    .unwrap()
}

pub fn middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "Telemetry": {
            "insert": [{
                "selector": "$[?(@.additionalProperties.Source == 'netstar.telemetry')]",
                "op": "Update",
                "label": "Vehicle",
                "id": "$.vehicleId",
                "properties": {
                    "id": "$.vehicleId",
                    "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                }
            }]
        }
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new("map", "map", cfg))]
}

pub fn source_pipeline() -> Vec<String> {
    vec!["map".to_string()]
}
