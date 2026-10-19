// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::component::{failure, EventResponse, Ingress};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use drasi_computation_contracts::ProducerSession;
use drasi_http_common::webhook::{
    auth::{verify_auth, AuthResult},
    config::{ErrorBehavior, WebhookConfig},
    content_parser::{parse_content, ContentType},
    route_matcher::{convert_method, find_matching_mappings, headers_to_map, RouteMatcher},
    template_engine::{TemplateContext, TemplateEngine},
};
use std::sync::Arc;

pub(crate) struct Webhooks {
    pub config: WebhookConfig,
    matcher: RouteMatcher,
    engine: TemplateEngine,
}

impl Webhooks {
    pub fn new(config: WebhookConfig) -> Self {
        Self {
            matcher: RouteMatcher::new(&config.routes),
            engine: TemplateEngine::new(),
            config,
        }
    }
}

fn mapping_error(behavior: &ErrorBehavior, status: StatusCode, message: &str) -> Response {
    eprintln!("drasi.network HTTP webhook rejected or skipped: {message}");
    match behavior {
        ErrorBehavior::Reject => failure(status, message, 0),
        ErrorBehavior::AcceptAndLog | ErrorBehavior::AcceptAndSkip => Json(EventResponse {
            success: true,
            message: if *behavior == ErrorBehavior::AcceptAndLog {
                format!("Accepted with warning: {message}")
            } else {
                "Accepted".into()
            },
            error: None,
            events_processed: 0,
        })
        .into_response(),
    }
}

pub(crate) async fn handle(
    State(ingress): State<Arc<Ingress>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(webhooks) = &ingress.webhooks else {
        return failure(StatusCode::NOT_FOUND, "no matching route", 0);
    };
    let Some(http_method) = convert_method(&method) else {
        return mapping_error(
            &webhooks.config.error_behavior,
            StatusCode::METHOD_NOT_ALLOWED,
            "method not supported",
        );
    };
    let Some(route_match) =
        webhooks
            .matcher
            .match_route(uri.path(), &http_method, &webhooks.config.routes)
    else {
        return mapping_error(
            &webhooks.config.error_behavior,
            StatusCode::NOT_FOUND,
            "no matching route",
        );
    };
    let route = route_match.route;
    let behavior = route
        .error_behavior
        .as_ref()
        .unwrap_or(&webhooks.config.error_behavior);
    if let AuthResult::Failed(reason) = verify_auth(route.auth.as_ref(), &headers, &body) {
        return mapping_error(behavior, StatusCode::UNAUTHORIZED, &reason);
    }
    let content_type = ContentType::from_header(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );
    let payload = match parse_content(&body, content_type) {
        Ok(payload) => payload,
        Err(error) => return mapping_error(behavior, StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let header_values = headers_to_map(&headers);
    let mappings = find_matching_mappings(&route.mappings, &header_values, &payload);
    if mappings.is_empty() {
        return mapping_error(behavior, StatusCode::BAD_REQUEST, "no matching mapping");
    }
    let context = TemplateContext {
        payload: payload.clone(),
        route: route_match.path_params,
        query: url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect(),
        headers: header_values,
        method: method.to_string(),
        path: uri.path().to_string(),
        source_id: ingress.config.source_id.clone(),
    };
    let mut changes = Vec::with_capacity(mappings.len());
    let mut errors = Vec::new();
    for mapping in mappings {
        match webhooks
            .engine
            .process_mapping(mapping, &context, &ingress.config.source_id)
        {
            Ok(change) => changes.push(change),
            Err(error) => {
                eprintln!("drasi.network HTTP webhook mapping failed: {error:#}");
                errors.push(error.to_string());
            }
        }
    }
    // Durable requests must never acknowledge a subset under an explicit skip
    // policy: each producer sequence denotes a stable mapped event.
    if !errors.is_empty() && (ingress.durable().is_some() || *behavior == ErrorBehavior::Reject) {
        return failure(StatusCode::BAD_REQUEST, errors.join("; "), 0);
    }
    if changes.is_empty() {
        return mapping_error(behavior, StatusCode::BAD_REQUEST, &errors.join("; "));
    }
    if ingress.durable().is_some() {
        let identity = (|| -> anyhow::Result<(ProducerSession, u64)> {
            let session = headers.get("x-drasi-producer-session").ok_or_else(|| {
                anyhow::anyhow!("durable webhook requires x-drasi-producer-session")
            })?;
            let sequence = headers.get("x-drasi-first-sequence").ok_or_else(|| {
                anyhow::anyhow!("durable webhook requires x-drasi-first-sequence")
            })?;
            Ok((
                serde_json::from_str(session.to_str()?)?,
                sequence.to_str()?.parse()?,
            ))
        })();
        return match identity {
            Ok((session, sequence)) => crate::durable_http::admission_response(
                ingress.admit_durable(&session, sequence, changes).await,
            ),
            Err(error) => {
                crate::durable_http::admission_response(Err(crate::durable::invalid(error)))
            }
        };
    }
    match ingress.admit(changes).await {
        Ok(accepted) => Json(EventResponse {
            success: true,
            message: format!(
                "Processed {accepted} events, {} mappings skipped",
                errors.len()
            ),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            events_processed: accepted,
        })
        .into_response(),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error.error, error.accepted),
    }
}
