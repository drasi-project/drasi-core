use std::sync::Arc;

use drasi_middleware::monotonic_guard::MonotonicGuardFactory;
use serde_json::json;

use drasi_core::{
    evaluation::functions::FunctionRegistry,
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use crate::QueryTestConfig;

const OBSERVER_QUERY: &str = r#"
    MATCH (n:Sensor)
    RETURN n.id as id, n.temperature as temperature, n.last_modified_at as timestamp
"#;

fn create_middleware_registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(MonotonicGuardFactory::new()));
    Arc::new(registry)
}

fn create_monotonic_guard_config(
    name: &str,
    timestamp_property: &str,
) -> Arc<drasi_core::models::SourceMiddlewareConfig> {
    Arc::new(drasi_core::models::SourceMiddlewareConfig {
        kind: Arc::from("monotonic-guard"),
        name: Arc::from(name),
        config: json!({
            "timestamp_property": timestamp_property
        })
        .as_object()
        .unwrap()
        .clone(),
    })
}

async fn setup_query(
    config: &(impl QueryTestConfig + Send),
    middleware_name: &str,
    timestamp_property: &str,
) -> ContinuousQuery {
    let registry = create_middleware_registry();
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let mut builder =
        QueryBuilder::new(OBSERVER_QUERY, parser).with_function_registry(function_registry);

    builder = config.config_query(builder).await;
    builder = builder.with_middleware_registry(registry);

    let mw_config = create_monotonic_guard_config(middleware_name, timestamp_property);

    builder = builder.with_source_middleware(mw_config);
    builder = builder.with_source_pipeline("test_source", &[middleware_name.to_string()]);

    builder.build().await
}

fn create_sensor_insert(id: &str, temperature: i64, timestamp: i64) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    props.insert("id", ElementValue::String(Arc::from(id)));
    props.insert("temperature", ElementValue::Integer(temperature));
    props.insert("last_modified_at", ElementValue::Integer(timestamp));

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("Sensor")].into(),
                effective_from: timestamp as u64,
            },
            properties: props,
        },
    }
}

fn create_sensor_update(id: &str, temperature: i64, timestamp: i64) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    props.insert("id", ElementValue::String(Arc::from(id)));
    props.insert("temperature", ElementValue::Integer(temperature));
    props.insert("last_modified_at", ElementValue::Integer(timestamp));

    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("Sensor")].into(),
                effective_from: timestamp as u64,
            },
            properties: props,
        },
    }
}

fn create_sensor_update_no_timestamp(
    id: &str,
    temperature: i64,
    effective_from: u64,
) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    props.insert("id", ElementValue::String(Arc::from(id)));
    props.insert("temperature", ElementValue::Integer(temperature));

    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("Sensor")].into(),
                effective_from,
            },
            properties: props,
        },
    }
}

#[allow(clippy::unwrap_used)]
async fn test_new_entity(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 25, 100);
    let result = query.process_source_change(insert).await.unwrap();

    assert_eq!(result.len(), 1);
}

#[allow(clippy::unwrap_used)]
async fn test_valid_update_newer_timestamp(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 25, 100);
    query.process_source_change(insert).await.unwrap();

    let update = create_sensor_update("sensor1", 30, 200);
    let result = query.process_source_change(update).await.unwrap();

    assert_eq!(result.len(), 1);
}

#[allow(clippy::unwrap_used)]
async fn test_stale_update_older_timestamp(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 25, 200);
    query.process_source_change(insert).await.unwrap();

    let update = create_sensor_update("sensor1", 30, 100);
    let result = query.process_source_change(update).await.unwrap();

    assert_eq!(result.len(), 0);
}

#[allow(clippy::unwrap_used)]
async fn test_equal_timestamp_dropped(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 25, 150);
    query.process_source_change(insert).await.unwrap();

    let update = create_sensor_update("sensor1", 30, 150);
    let result = query.process_source_change(update).await.unwrap();

    assert_eq!(result.len(), 0);
}

#[allow(clippy::unwrap_used)]
async fn test_missing_timestamp_property_fallback(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 25, 100);
    query.process_source_change(insert).await.unwrap();

    let update = create_sensor_update_no_timestamp("sensor1", 30, 200);
    let result = query.process_source_change(update).await.unwrap();
    assert_eq!(result.len(), 1);

    let stale_update = create_sensor_update_no_timestamp("sensor1", 35, 150);
    let result = query.process_source_change(stale_update).await.unwrap();
    assert_eq!(result.len(), 0);
}

#[allow(clippy::unwrap_used)]
async fn test_multiple_updates_sequence(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert = create_sensor_insert("sensor1", 20, 100);
    query.process_source_change(insert).await.unwrap();

    let update1 = create_sensor_update("sensor1", 25, 150);
    let result = query.process_source_change(update1).await.unwrap();
    assert_eq!(result.len(), 1);

    let update2 = create_sensor_update("sensor1", 30, 200);
    let result = query.process_source_change(update2).await.unwrap();
    assert_eq!(result.len(), 1);

    let update3 = create_sensor_update("sensor1", 22, 120);
    let result = query.process_source_change(update3).await.unwrap();
    assert_eq!(result.len(), 0);

    let update4 = create_sensor_update("sensor1", 35, 250);
    let result = query.process_source_change(update4).await.unwrap();
    assert_eq!(result.len(), 1);
}

#[allow(clippy::unwrap_used)]
async fn test_independent_entity_tracking(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(config, "monotonic_guard", "last_modified_at").await;

    let insert1 = create_sensor_insert("sensor1", 20, 100);
    query.process_source_change(insert1).await.unwrap();

    let insert2 = create_sensor_insert("sensor2", 30, 50);
    query.process_source_change(insert2).await.unwrap();

    let update1 = create_sensor_update("sensor1", 25, 200);
    let result = query.process_source_change(update1).await.unwrap();
    assert_eq!(result.len(), 1);

    let update2 = create_sensor_update("sensor2", 35, 100);
    let result = query.process_source_change(update2).await.unwrap();
    assert_eq!(result.len(), 1);
}

pub async fn monotonic_guard_tests(config: &(impl QueryTestConfig + Send)) {
    test_new_entity(config).await;
    test_valid_update_newer_timestamp(config).await;
    test_stale_update_older_timestamp(config).await;
    test_equal_timestamp_dropped(config).await;
    test_missing_timestamp_property_fallback(config).await;
    test_multiple_updates_sequence(config).await;
    test_independent_entity_tracking(config).await;
}
