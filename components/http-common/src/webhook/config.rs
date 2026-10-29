// Copyright 2025 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Webhook configuration shared with the original HTTP source.

use serde::{Deserialize, Serialize};

pub use drasi_source_mapping::{
    EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
    SourceMapping as WebhookMapping, TimestampFormat,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookConfig {
    #[serde(default)]
    pub error_behavior: ErrorBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    pub routes: Vec<WebhookRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorsConfig {
    #[serde(default = "default_cors_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cors_origins")]
    pub allow_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allow_methods: Vec<String>,
    #[serde(default = "default_cors_headers")]
    pub allow_headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
    #[serde(default = "default_cors_max_age")]
    pub max_age: u64,
}

fn default_cors_enabled() -> bool {
    true
}
fn default_cors_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_cors_methods() -> Vec<String> {
    ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
        .into_iter()
        .map(String::from)
        .collect()
}
fn default_cors_headers() -> Vec<String> {
    ["Content-Type", "Authorization", "X-Requested-With"]
        .into_iter()
        .map(String::from)
        .collect()
}
fn default_cors_max_age() -> u64 {
    3600
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_origins: default_cors_origins(),
            allow_methods: default_cors_methods(),
            allow_headers: default_cors_headers(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: default_cors_max_age(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErrorBehavior {
    #[default]
    AcceptAndLog,
    AcceptAndSkip,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookRoute {
    pub path: String,
    #[serde(default = "default_methods")]
    pub methods: Vec<HttpMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_behavior: Option<ErrorBehavior>,
    pub mappings: Vec<WebhookMapping>,
}

fn default_methods() -> Vec<HttpMethod> {
    vec![HttpMethod::Post]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<BearerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignatureConfig {
    #[serde(rename = "type")]
    pub algorithm: SignatureAlgorithm,
    pub secret_env: String,
    pub header: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default)]
    pub encoding: SignatureEncoding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureAlgorithm {
    HmacSha1,
    HmacSha256,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SignatureEncoding {
    #[default]
    Hex,
    Base64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BearerConfig {
    pub token_env: String,
}

impl WebhookConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.routes.is_empty(),
            "Validation error: webhooks.routes cannot be empty"
        );
        for (idx, route) in self.routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow::anyhow!("Validation error in route[{idx}]: {e}"))?;
        }
        if let Some(cors) = self.cors.as_ref().filter(|cors| cors.enabled) {
            super::cors::build_cors_layer(cors)?;
        }
        Ok(())
    }
}

impl WebhookRoute {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.path.is_empty(), "path cannot be empty");
        anyhow::ensure!(self.path.starts_with('/'), "path must start with '/'");
        anyhow::ensure!(!self.methods.is_empty(), "methods cannot be empty");
        anyhow::ensure!(!self.mappings.is_empty(), "mappings cannot be empty");
        for (idx, mapping) in self.mappings.iter().enumerate() {
            mapping
                .validate()
                .map_err(|e| anyhow::anyhow!("mappings[{idx}]: {e}"))?;
        }
        Ok(())
    }
}
