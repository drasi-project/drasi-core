// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{BootstrapLabels, StreamId};
use drasi_computation_plugin_sdk::{ExportedPlugin, Scope, ServedSource};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use serde_json::{json, Value};

const SOURCE_ID: &str = "4a4b64d8-9d8b-5ddd-867d-2f925822946c";

fn session() -> Result<BootstrapSession> {
    Ok(BootstrapSession::new(
        Some(ServedSource {
            source: ComponentId::try_new(SOURCE_ID)?,
            stream: StreamId::try_new("events/out")?,
        }),
        None,
    ))
}

fn create(request: &CreateRequest) -> Result<NativeBootstrapProvider> {
    HttpBootstrapFactory.create(request, &session()?, None)
}

fn configuration() -> Value {
    json!({
        "settings":{"endpoints":[{
            "url":"https://example.invalid/snapshot",
            "response":{"mappings":[{"elementType":"node","template":{"id":"{{item.id}}","labels":["Item"]}}]}
        }]}
    })
}
fn request() -> Result<CreateRequest> {
    Ok(CreateRequest {
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
        id: ComponentId::try_new("00edb855-e19a-5466-8575-631427e5d10d")?,
        implementation: HttpBootstrapFactory.metadata().implementation,
        configuration_version: 1,
        configuration: configuration(),
        scope: Some(Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
    })
}

#[test]
fn construction_is_query_scoped_and_does_not_resolve_or_fetch_credentials() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let mut request = request()?;
    request.configuration["settings"]["endpoints"][0]["auth"] = json!({
        "type":"bearer","tokenEnv":"DRASI_NATIVE_BOOTSTRAP_NOT_RESOLVED_AT_CONSTRUCTION"
    });
    let provider = create(&request)?;
    assert_eq!(
        provider.completion(),
        BootstrapCompletionKind::Quiescent,
        "the provider has its declared completion"
    );
    provider.validate_query_scope("instance", "graph", &request.id)?;
    assert!(provider
        .validate_query_scope("different", "graph", &request.id)
        .is_err());
    assert!(provider
        .validate_query_scope(
            "instance",
            "graph",
            &ComponentId::try_new("023b840b-e18b-5133-912e-33de6190a3ca")?
        )
        .is_err());
    request.scope = None;
    assert!(create(&request).is_err());
    Ok(())
}

#[test]
fn namespace_comes_from_the_graph_relation_not_configuration() -> Result<()> {
    let description = HttpBootstrapFactory.describe(&configuration())?;
    assert_eq!(description.namespace, ElementNamespace::ServedSource);
    assert_eq!(description.pairing, None);
    HttpBootstrapFactory
        .metadata()
        .validate_description(&description)?;
    let mut legacy = configuration();
    legacy["source"] = json!(SOURCE_ID);
    legacy["stream"] = json!("events/out");
    assert!(
        HttpBootstrapFactory.describe(&legacy).is_err(),
        "a configured source namespace is refused"
    );
    let request = request()?;
    let unserved = HttpBootstrapFactory.create(&request, &BootstrapSession::default(), None);
    assert!(
        unserved.is_err(),
        "a provider without a served source from its relation is refused"
    );
    Ok(())
}

fn node(label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, "1"),
                labels: std::sync::Arc::from([std::sync::Arc::from(label)]),
                effective_from: 0,
            },
            properties: ElementPropertyMap::new(),
        },
    }
}

fn relation(label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, "r"),
                labels: std::sync::Arc::from([std::sync::Arc::from(label)]),
                effective_from: 0,
            },
            in_node: ElementReference::new(SOURCE_ID, "1"),
            out_node: ElementReference::new(SOURCE_ID, "2"),
            properties: ElementPropertyMap::new(),
        },
    }
}

#[test]
fn query_labels_select_mapped_elements() -> Result<()> {
    let labels = BootstrapLabels::new(["Order"], ["PLACED"])?;
    assert!(snapshot::requested(Some(&labels), &node("Order")));
    assert!(!snapshot::requested(Some(&labels), &node("Item")));
    assert!(snapshot::requested(Some(&labels), &relation("PLACED")));
    assert!(
        !snapshot::requested(Some(&labels), &relation("Order")),
        "a node label does not select a relation"
    );
    assert!(
        snapshot::requested(None, &node("Item")),
        "no request selects everything"
    );
    Ok(())
}

#[test]
fn nested_typos_and_invalid_limits_are_rejected_before_io() -> Result<()> {
    for field in [
        "settings",
        "endpoint",
        "response",
        "mapping",
        "template",
        "auth",
        "bounds",
        "header",
        "snake-auth",
        "snake-pagination",
    ] {
        let mut request = request()?;
        match field {
            "settings" => request.configuration["settings"]["max_pages"] = json!(1),
            "endpoint" => request.configuration["settings"]["endpoints"][0]["extra"] = json!(true),
            "response" => {
                request.configuration["settings"]["endpoints"][0]["response"]["items_path"] =
                    json!("$")
            }
            "mapping" => {
                request.configuration["settings"]["endpoints"][0]["response"]["mappings"][0]
                    ["element_type"] = json!("node")
            }
            "template" => {
                request.configuration["settings"]["endpoints"][0]["response"]["mappings"][0]
                    ["template"]["extra"] = json!(true)
            }
            "auth" => {
                request.configuration["settings"]["endpoints"][0]["auth"] =
                    json!({"type":"bearer","tokenEnv":"TOKEN","usernameEnv":"USER"})
            }
            "bounds" => request.configuration["outputCapacity"] = json!(0),
            "header" => {
                request.configuration["settings"]["endpoints"][0]["headers"] =
                    json!({"x-test":"bad\nvalue"})
            }
            "snake-auth" => {
                request.configuration["settings"]["endpoints"][0]["auth"] =
                    json!({"type":"bearer","token_env":"TOKEN"})
            }
            "snake-pagination" => {
                request.configuration["settings"]["endpoints"][0]["pagination"] =
                    json!({"type":"offset-limit","page_size":10})
            }
            _ => unreachable!(),
        }
        assert!(create(&request).is_err(), "{field}");
        assert!(
            HttpBootstrapFactory
                .describe(&request.configuration)
                .is_err(),
            "{field} is refused when the graph asks for the description"
        );
    }
    Ok(())
}

#[test]
fn camel_case_authentication_and_pagination_shapes_are_accepted() -> Result<()> {
    for (auth, pagination) in [
        (
            json!({"type":"bearer","tokenEnv":"TOKEN"}),
            json!({"type":"offset-limit","pageSize":10}),
        ),
        (
            json!({"type":"basic","usernameEnv":"USER","passwordEnv":"PASSWORD"}),
            json!({"type":"page-number","pageSize":10,"pageSizeParam":"per_page"}),
        ),
        (
            json!({"type":"api-key","location":"query","name":"key","valueEnv":"KEY"}),
            json!({"type":"cursor","cursorParam":"cursor","cursorPath":"$.cursor"}),
        ),
        (
            json!({"type":"api-key","location":"header","name":"x-key","valueEnv":"KEY"}),
            json!({"type":"link-header"}),
        ),
        (
            json!({"type":"oauth2-client-credentials","tokenUrl":"https://example.invalid/token","clientIdEnv":"CLIENT","clientSecretEnv":"SECRET"}),
            json!({"type":"next-url","nextUrlPath":"$.next"}),
        ),
    ] {
        let mut request = request()?;
        request.configuration["settings"]["endpoints"][0]["auth"] = auth;
        request.configuration["settings"]["endpoints"][0]["pagination"] = pagination;
        create(&request)?;
    }
    Ok(())
}

#[test]
fn published_defaults_are_the_parsed_defaults() -> Result<()> {
    let definition = definition();
    let schema = definition.schema();
    assert!(HttpBootstrapFactory
        .metadata()
        .configuration
        .fields
        .keys()
        .eq(schema.fields.keys()));
    drasi_computation_plugin_sdk::testing::ensure_camel_case_configuration(&schema)?;
    let parsed = serde_json::to_value(HttpBootstrapConfig::from_value(&configuration())?)?;
    for name in schema.fields.keys() {
        if name != "settings" {
            assert_eq!(
                definition.details().fields[name].default.as_ref(),
                Some(&parsed[name]),
                "{name}"
            );
        }
    }
    Ok(())
}
