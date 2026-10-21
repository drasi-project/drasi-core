// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::{ExportedPlugin, Scope};
use serde_json::{json, Value};

fn configuration() -> Value {
    json!({
        "source":"events","stream":"events/out","settings":{"endpoints":[{
            "url":"https://example.invalid/snapshot",
            "response":{"mappings":[{"elementType":"node","template":{"id":"{{item.id}}","labels":["Item"]}}]}
        }]}
    })
}
fn request() -> Result<CreateRequest> {
    Ok(CreateRequest {
        activation_credentials_bound: false,
        id: ComponentId::try_new("query")?,
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
        "type":"bearer","token_env":"DRASI_NATIVE_BOOTSTRAP_NOT_RESOLVED_AT_CONSTRUCTION"
    });
    let provider = HttpBootstrapFactory.create(&request, None)?;
    provider.validate_query_scope("instance", "graph", &request.id)?;
    assert!(provider
        .validate_query_scope("different", "graph", &request.id)
        .is_err());
    assert!(provider
        .validate_query_scope("instance", "graph", &ComponentId::try_new("other")?)
        .is_err());
    request.scope = None;
    assert!(HttpBootstrapFactory.create(&request, None).is_err());
    Ok(())
}

#[test]
fn nested_typos_and_invalid_limits_are_rejected_before_io() -> Result<()> {
    for field in [
        "settings", "endpoint", "response", "mapping", "template", "auth", "bounds", "header",
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
                    json!({"type":"bearer","token_env":"TOKEN","username_env":"USER"})
            }
            "bounds" => request.configuration["outputCapacity"] = json!(0),
            "header" => {
                request.configuration["settings"]["endpoints"][0]["headers"] =
                    json!({"x-test":"bad\nvalue"})
            }
            _ => unreachable!(),
        }
        assert!(
            HttpBootstrapFactory.create(&request, None).is_err(),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn published_authentication_and_pagination_shapes_are_accepted() -> Result<()> {
    for (auth, pagination) in [
        (
            json!({"type":"bearer","token_env":"TOKEN"}),
            json!({"type":"offset-limit","page_size":10}),
        ),
        (
            json!({"type":"basic","username_env":"USER","password_env":"PASSWORD"}),
            json!({"type":"page-number","page_size":10}),
        ),
        (
            json!({"type":"api-key","location":"query","name":"key","value_env":"KEY"}),
            json!({"type":"cursor","cursor_param":"cursor","cursor_path":"$.cursor"}),
        ),
        (
            json!({"type":"api-key","location":"header","name":"x-key","value_env":"KEY"}),
            json!({"type":"link-header"}),
        ),
        (
            json!({"type":"oauth2-client-credentials","token_url":"https://example.invalid/token","client_id_env":"CLIENT","client_secret_env":"SECRET"}),
            json!({"type":"next-url","next_url_path":"$.next"}),
        ),
    ] {
        let mut request = request()?;
        request.configuration["settings"]["endpoints"][0]["auth"] = auth;
        request.configuration["settings"]["endpoints"][0]["pagination"] = pagination;
        HttpBootstrapFactory.create(&request, None)?;
    }
    Ok(())
}
