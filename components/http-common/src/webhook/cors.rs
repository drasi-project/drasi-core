// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::config::CorsConfig;
use anyhow::{ensure, Result};
use axum::http::{HeaderName, HeaderValue, Method};
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};

pub fn build_cors_layer(config: &CorsConfig) -> Result<CorsLayer> {
    let origins_any = config.allow_origins == ["*"];
    let headers_any = config.allow_headers == ["*"];
    ensure!(
        !config.allow_credentials
            || (!origins_any && !headers_any && !config.expose_headers.iter().any(|h| h == "*")),
        "credentialed CORS requires explicit origins, allowed headers and exposed headers"
    );
    let mut cors = CorsLayer::new();
    cors = if origins_any {
        cors.allow_origin(Any)
    } else {
        cors.allow_origin(
            config
                .allow_origins
                .iter()
                .map(|origin| origin.parse::<HeaderValue>())
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    cors = cors.allow_methods(
        config
            .allow_methods
            .iter()
            .map(|method| method.parse::<Method>())
            .collect::<Result<Vec<_>, _>>()?,
    );
    cors = if headers_any {
        cors.allow_headers(Any)
    } else {
        cors.allow_headers(
            config
                .allow_headers
                .iter()
                .map(|name| name.parse::<HeaderName>())
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    if !config.expose_headers.is_empty() {
        cors = cors.expose_headers(
            config
                .expose_headers
                .iter()
                .map(|name| name.parse::<HeaderName>())
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(cors
        .allow_credentials(config.allow_credentials)
        .max_age(Duration::from_secs(config.max_age)))
}
