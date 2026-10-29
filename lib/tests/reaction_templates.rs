// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, str::FromStr};

use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};

fn template(value: &str) -> TemplateSpec {
    TemplateSpec {
        template: value.into(),
        extension: (),
    }
}

struct Routing {
    routes: HashMap<String, QueryConfig>,
    default: Option<QueryConfig>,
}

impl TemplateRouting for Routing {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default.as_ref()
    }
}

#[test]
fn test_operation_type_from_str() {
    for (name, operation) in [
        ("add", OperationType::Add),
        ("update", OperationType::Update),
        ("delete", OperationType::Delete),
    ] {
        assert_eq!(OperationType::from_str(name), Ok(operation));
        assert_eq!(OperationType::from_str(&name.to_uppercase()), Ok(operation));
    }
    assert!(OperationType::from_str("invalid").is_err());
}

#[test]
fn test_operation_type_as_str() {
    assert_eq!(OperationType::Add.as_str(), "add");
    assert_eq!(OperationType::Update.as_str(), "update");
    assert_eq!(OperationType::Delete.as_str(), "delete");
}

#[test]
fn test_template_spec_serialization() {
    let spec = template("{{after.id}}");
    let json = serde_json::to_string(&spec).unwrap();
    assert_eq!(serde_json::from_str::<TemplateSpec>(&json).unwrap(), spec);
}

#[test]
fn test_query_config_serialization() {
    let config = QueryConfig {
        added: Some(template("[ADD] {{after.id}}")),
        updated: None,
        deleted: Some(template("[DEL] {{before.id}}")),
    };
    let json = serde_json::to_string(&config).unwrap();
    assert_eq!(serde_json::from_str::<QueryConfig>(&json).unwrap(), config);
}

#[test]
fn test_template_routing_query_specific() {
    let config = Routing {
        routes: HashMap::from([(
            "query1".into(),
            QueryConfig {
                added: Some(template("query1 add")),
                updated: None,
                deleted: None,
            },
        )]),
        default: None,
    };
    assert_eq!(
        config.get_template_spec("query1", OperationType::Add),
        Some(&template("query1 add"))
    );
    assert!(config
        .get_template_spec("query1", OperationType::Update)
        .is_none());
}

#[test]
fn test_template_routing_default_fallback() {
    let config = Routing {
        routes: HashMap::new(),
        default: Some(QueryConfig {
            added: Some(template("default add")),
            updated: Some(template("default update")),
            deleted: Some(template("default delete")),
        }),
    };
    for (operation, expected) in [
        (OperationType::Add, "default add"),
        (OperationType::Update, "default update"),
        (OperationType::Delete, "default delete"),
    ] {
        assert_eq!(
            config.get_template_spec("any_query", operation),
            Some(&template(expected))
        );
    }
}

#[test]
fn test_template_routing_query_overrides_default() {
    let config = Routing {
        routes: HashMap::from([(
            "query1".into(),
            QueryConfig {
                added: Some(template("query1 add override")),
                updated: None,
                deleted: None,
            },
        )]),
        default: Some(QueryConfig {
            added: Some(template("default add")),
            updated: Some(template("default update")),
            deleted: None,
        }),
    };
    assert_eq!(
        config.get_template_spec("query1", OperationType::Add),
        Some(&template("query1 add override"))
    );
    assert_eq!(
        config.get_template_spec("query1", OperationType::Update),
        Some(&template("default update"))
    );
    assert_eq!(
        config.get_template_spec("query2", OperationType::Add),
        Some(&template("default add"))
    );
}
