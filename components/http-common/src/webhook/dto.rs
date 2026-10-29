// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published HTTP camelCase configuration, resolved to runtime mapping
//! types without importing the legacy plugin SDK or its secret resolver.

use super::config::*;
use drasi_source_mapping::SourceMappingDto;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebhookDto {
    #[serde(default)]
    error_behavior: ErrorBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cors: Option<CorsDto>,
    routes: Vec<RouteDto>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RouteDto {
    path: String,
    #[serde(default = "methods")]
    methods: Vec<HttpMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<AuthDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_behavior: Option<ErrorBehavior>,
    mappings: Vec<MappingDto>,
}
fn methods() -> Vec<HttpMethod> {
    vec![HttpMethod::Post]
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<SignatureDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer: Option<BearerDto>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignatureDto {
    #[serde(rename = "type")]
    algorithm: SignatureAlgorithm,
    secret_env: String,
    header: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(default)]
    encoding: SignatureEncoding,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BearerDto {
    token_env: String,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CorsDto {
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default = "origins")]
    allow_origins: Vec<String>,
    #[serde(default = "cors_methods")]
    allow_methods: Vec<String>,
    #[serde(default = "headers")]
    allow_headers: Vec<String>,
    #[serde(default)]
    expose_headers: Vec<String>,
    #[serde(default)]
    allow_credentials: bool,
    #[serde(default = "max_age")]
    max_age: u64,
}
fn enabled() -> bool {
    true
}
fn origins() -> Vec<String> {
    CorsConfig::default().allow_origins
}
fn cors_methods() -> Vec<String> {
    CorsConfig::default().allow_methods
}
fn headers() -> Vec<String> {
    CorsConfig::default().allow_headers
}
fn max_age() -> u64 {
    CorsConfig::default().max_age
}

#[derive(Serialize)]
#[serde(transparent)]
struct MappingDto(SourceMappingDto);
impl<'de> Deserialize<'de> for MappingDto {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        fn fields(value: &serde_json::Value, allowed: &[&str]) -> Result<(), String> {
            let object = value
                .as_object()
                .ok_or_else(|| "expected a mapping object".to_owned())?;
            for key in object.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(format!("unknown HTTP mapping field {key}"));
                }
            }
            Ok(())
        }
        fields(
            &value,
            &[
                "when",
                "operation",
                "operationFrom",
                "operationMap",
                "elementType",
                "effectiveFrom",
                "template",
            ],
        )
        .map_err(serde::de::Error::custom)?;
        if let Some(condition) = value.get("when").filter(|value| !value.is_null()) {
            fields(
                condition,
                &["header", "field", "equals", "contains", "regex"],
            )
            .map_err(serde::de::Error::custom)?;
        }
        if let Some(template) = value.get("template") {
            fields(template, &["id", "labels", "properties", "from", "to"])
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(timestamp) = value.get("effectiveFrom").filter(|value| value.is_object()) {
            fields(timestamp, &["value", "format"]).map_err(serde::de::Error::custom)?;
        }
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl From<WebhookDto> for WebhookConfig {
    fn from(dto: WebhookDto) -> Self {
        Self {
            error_behavior: dto.error_behavior,
            cors: dto.cors.map(|cors| CorsConfig {
                enabled: cors.enabled,
                allow_origins: cors.allow_origins,
                allow_methods: cors.allow_methods,
                allow_headers: cors.allow_headers,
                expose_headers: cors.expose_headers,
                allow_credentials: cors.allow_credentials,
                max_age: cors.max_age,
            }),
            routes: dto
                .routes
                .into_iter()
                .map(|route| WebhookRoute {
                    path: route.path,
                    methods: route.methods,
                    error_behavior: route.error_behavior,
                    auth: route.auth.map(|auth| AuthConfig {
                        signature: auth.signature.map(|signature| SignatureConfig {
                            algorithm: signature.algorithm,
                            secret_env: signature.secret_env,
                            header: signature.header,
                            prefix: signature.prefix,
                            encoding: signature.encoding,
                        }),
                        bearer: auth.bearer.map(|bearer| BearerConfig {
                            token_env: bearer.token_env,
                        }),
                    }),
                    mappings: route
                        .mappings
                        .into_iter()
                        .map(|mapping| mapping.0.into())
                        .collect(),
                })
                .collect(),
        }
    }
}

impl TryFrom<&WebhookConfig> for WebhookDto {
    type Error = serde_json::Error;
    fn try_from(config: &WebhookConfig) -> Result<Self, Self::Error> {
        let routes = config
            .routes
            .iter()
            .map(|route| {
                let mappings = route
                    .mappings
                    .iter()
                    .map(|mapping| {
                        let mut value = serde_json::to_value(mapping)?;
                        let object = value
                            .as_object_mut()
                            .expect("mapping serializes as an object");
                        for (internal, wire) in [
                            ("operation_from", "operationFrom"),
                            ("operation_map", "operationMap"),
                            ("element_type", "elementType"),
                            ("effective_from", "effectiveFrom"),
                        ] {
                            if let Some(value) = object.remove(internal) {
                                object.insert(wire.into(), value);
                            }
                        }
                        serde_json::from_value(value).map(MappingDto)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(RouteDto {
                    path: route.path.clone(),
                    methods: route.methods.clone(),
                    error_behavior: route.error_behavior.clone(),
                    auth: route.auth.as_ref().map(|auth| AuthDto {
                        signature: auth.signature.as_ref().map(|signature| SignatureDto {
                            algorithm: signature.algorithm.clone(),
                            secret_env: signature.secret_env.clone(),
                            header: signature.header.clone(),
                            prefix: signature.prefix.clone(),
                            encoding: signature.encoding.clone(),
                        }),
                        bearer: auth.bearer.as_ref().map(|bearer| BearerDto {
                            token_env: bearer.token_env.clone(),
                        }),
                    }),
                    mappings,
                })
            })
            .collect::<Result<Vec<_>, Self::Error>>()?;
        Ok(Self {
            error_behavior: config.error_behavior.clone(),
            cors: config.cors.as_ref().map(|cors| CorsDto {
                enabled: cors.enabled,
                allow_origins: cors.allow_origins.clone(),
                allow_methods: cors.allow_methods.clone(),
                allow_headers: cors.allow_headers.clone(),
                expose_headers: cors.expose_headers.clone(),
                allow_credentials: cors.allow_credentials,
                max_age: cors.max_age,
            }),
            routes,
        })
    }
}

pub mod optional {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        config: &Option<WebhookConfig>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        config
            .as_ref()
            .map(WebhookDto::try_from)
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<WebhookConfig>, D::Error> {
        Option::<WebhookDto>::deserialize(deserializer).map(|value| value.map(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Serialize, Deserialize)]
    struct Configuration {
        #[serde(with = "optional")]
        webhooks: Option<WebhookConfig>,
    }
    #[test]
    fn published_dto_round_trips_without_internal_field_aliases() {
        let value = serde_json::json!({"webhooks":{"errorBehavior":"reject","routes":[{
            "path":"/events","mappings":[{"elementType":"node","operation":"insert",
                "effectiveFrom":{"value":"{{payload.time}}","format":"unix_millis"},
                "template":{"id":"{{payload.id}}","labels":["Item"]}}]
        }]}});
        let config: Configuration = serde_json::from_value(value.clone()).unwrap();
        let serialized = serde_json::to_value(&config).unwrap();
        assert_eq!(
            serialized["webhooks"]["routes"][0]["mappings"][0]["elementType"],
            "node"
        );
        assert_eq!(
            config.webhooks.as_ref().unwrap().routes[0].mappings[0].element_type,
            ElementType::Node
        );
        for field in ["element_type", "effective_from", "operation_from"] {
            let mut invalid = value.clone();
            invalid["webhooks"]["routes"][0]["mappings"][0][field] = serde_json::json!("invalid");
            assert!(serde_json::from_value::<Configuration>(invalid).is_err());
        }
    }
}
