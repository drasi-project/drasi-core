// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Webhook routes: authentication, parsing, matching and mapping of arbitrary
//! HTTP payloads into graph changes.

use crate::component::{accepted_response, invalid, rejection, EventResponse, HttpIngress};
use crate::ingress::{IngressError, IngressErrorKind};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use drasi_computation_contracts::ProducerSession;
use drasi_computation_network_common::diag;
use drasi_http_common::webhook::{
    auth::{AuthResult, ResolvedAuth},
    config::{ErrorBehavior, WebhookConfig, WebhookRoute},
    content_parser::{parse_content, ContentType},
    route_matcher::{convert_method, find_matching_mappings, headers_to_map, RouteMatcher},
    template_engine::{TemplateContext, TemplateEngine},
};
use std::sync::Arc;

/// Header carrying an explicit producer session for durable webhooks.
pub(crate) const SESSION_HEADER: &str = "x-drasi-producer-session";
/// Header carrying the first producer sequence for durable webhooks.
pub(crate) const SEQUENCE_HEADER: &str = "x-drasi-first-sequence";

pub(crate) struct Webhooks {
    pub config: WebhookConfig,
    matcher: RouteMatcher,
    engine: TemplateEngine,
    /// Each route's authentication with secrets resolved, by route index.
    auth: Vec<Option<ResolvedAuth>>,
}

impl Webhooks {
    /// Resolves every route's secrets once for this configuration; a missing
    /// secret refuses the configuration instead of failing each request.
    pub fn new(config: WebhookConfig) -> anyhow::Result<Self> {
        let auth = config
            .routes
            .iter()
            .map(|route| {
                route
                    .auth
                    .as_ref()
                    .map(ResolvedAuth::resolve)
                    .transpose()
                    .map_err(|error| error.context(format!("webhook route {}", route.path)))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            matcher: RouteMatcher::new(&config.routes),
            engine: TemplateEngine::new(),
            auth,
            config,
        })
    }

    fn verify(&self, route: &WebhookRoute, headers: &HeaderMap, body: &[u8]) -> AuthResult {
        // Fails closed: the route always comes from this configuration.
        match self
            .config
            .routes
            .iter()
            .position(|candidate| std::ptr::eq(candidate, route))
        {
            Some(index) => self.auth[index]
                .as_ref()
                .map_or(AuthResult::NotConfigured, |auth| auth.verify(headers, body)),
            None => AuthResult::Failed("route is not part of the resolved configuration".into()),
        }
    }
}

/// Applies the route's error policy to a mapping or routing problem. Never
/// used for authentication, capacity or admission failures.
fn mapping_error(behavior: &ErrorBehavior, status: StatusCode, message: &str) -> Response {
    diag!(Debug, "HTTP webhook rejected or skipped: {message}");
    match behavior {
        ErrorBehavior::Reject => {
            let mut response = invalid(message);
            *response.status_mut() = status;
            response
        }
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

fn explicit_session(headers: &HeaderMap) -> Option<anyhow::Result<(ProducerSession, u64)>> {
    let session = headers.get(SESSION_HEADER);
    let sequence = headers.get(SEQUENCE_HEADER);
    if session.is_none() && sequence.is_none() {
        return None;
    }
    Some((|| {
        let session = session
            .ok_or_else(|| anyhow::anyhow!("{SEQUENCE_HEADER} requires {SESSION_HEADER}"))?;
        let sequence = sequence
            .ok_or_else(|| anyhow::anyhow!("{SESSION_HEADER} requires {SEQUENCE_HEADER}"))?;
        Ok((
            serde_json::from_str(session.to_str()?)?,
            sequence.to_str()?.parse()?,
        ))
    })())
}

pub(crate) async fn handle(
    State(ingress): State<Arc<HttpIngress>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let settings = ingress.settings();
    let Some(webhooks) = settings.webhooks.as_deref() else {
        return invalid("no matching route");
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
    // Authentication failures are always 401 with a generic message, whatever
    // the error policy: callers learn nothing about the configured secrets.
    if let AuthResult::Failed(reason) = webhooks.verify(route, &headers, &body) {
        diag!(
            Debug,
            "HTTP webhook authentication failed for {}: {reason}",
            uri.path()
        );
        return rejection(
            &IngressError::new(
                IngressErrorKind::Unauthorized,
                "webhook authentication failed",
            ),
            0,
        );
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
    let durable = ingress.shared.durable().is_some();
    let explicit = explicit_session(&headers);
    if explicit.is_some() {
        // Idempotent delivery cannot be honoured without durable admission;
        // accepting the request volatile would silently drop the guarantee.
        if let Err(error) = ingress.shared.admission_service() {
            return crate::durable_http::admission_response(Err(error));
        }
    }
    if explicit.is_some()
        && mappings
            .iter()
            .any(|mapping| mapping.effective_from.is_none())
    {
        // An explicit sequence denotes a stable event, so a retry must map to
        // an identical change; only payload-derived timestamps guarantee that.
        return invalid(
            "explicit-session webhook admission requires effectiveFrom on every mapping",
        );
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
        source_id: ingress.shared.source_id.clone(),
    };
    let mut changes = Vec::with_capacity(mappings.len());
    let mut errors = Vec::new();
    for mapping in mappings {
        match webhooks
            .engine
            .process_mapping(mapping, &context, ingress.shared.source_id.as_str())
        {
            Ok(change) => changes.push(change),
            Err(error) => {
                diag!(Debug, "HTTP webhook mapping failed: {error:#}");
                errors.push(error.to_string());
            }
        }
    }
    // Durable requests never acknowledge a subset under a skip policy: each
    // committed event must correspond to a mapped part of the request.
    if !errors.is_empty() && (durable || *behavior == ErrorBehavior::Reject) {
        return invalid(errors.join("; "));
    }
    if changes.is_empty() {
        return mapping_error(behavior, StatusCode::BAD_REQUEST, &errors.join("; "));
    }
    match explicit {
        Some(Ok((session, sequence))) => crate::durable_http::admission_response(
            ingress
                .shared
                .admit_durable(&session, sequence, changes)
                .await,
        ),
        Some(Err(error)) => {
            crate::durable_http::admission_response(Err(crate::durable::invalid(error)))
        }
        None => accepted_response(ingress.shared.admit(changes).await, errors.len()),
    }
}
