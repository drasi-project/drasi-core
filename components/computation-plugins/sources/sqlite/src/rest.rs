// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The REST listener: the shared SQLite routes served by the native network
//! listener (connection limit, header-read deadline, TLS, graceful drain)
//! with bearer authentication from the shared authenticator.

use crate::config::SourceConfig;
use anyhow::Result;
use drasi_computation_network_common::{
    security::Authenticator,
    server::{serve, ServerOptions},
};
use drasi_sqlite_common::{
    native::runtime::{RestServe, RestSettings},
    RestApiConfig,
};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

/// Deadline for a client to send request headers or finish a TLS handshake,
/// so a stalled client cannot hold a connection or a Drain.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The accepted bearer tokens, shared with the running listener so a token
/// rotation applies in place.
#[derive(Clone, Default)]
pub(crate) struct Tokens(Arc<RwLock<Authenticator>>);

impl Tokens {
    pub(crate) fn replace(&self, config: &SourceConfig) -> Result<()> {
        let authenticator = Authenticator::new(config.security()?.auth_tokens);
        *self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("SQLite REST token store poisoned"))? = authenticator;
        Ok(())
    }

    fn authorize(&self, header: Option<&str>) -> bool {
        // A poisoned store fails closed.
        self.0
            .read()
            .map(|authenticator| authenticator.authorize(header))
            .unwrap_or(false)
    }
}

/// The listener settings for one activation, or `None` when REST is off.
pub(crate) fn settings(config: &SourceConfig, tokens: &Tokens) -> Result<Option<RestSettings>> {
    let settings = &config.settings;
    if !settings.rest_api {
        return Ok(None);
    }
    let security = config.security()?;
    tokens.replace(config)?;
    let authorize = (!security.auth_tokens.is_empty()).then(|| {
        let tokens = tokens.clone();
        Arc::new(move |header: Option<&str>| tokens.authorize(header))
            as Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>
    });
    let tls = security
        .tls
        .as_ref()
        .map(|material| material.server_config(vec![b"h2".to_vec(), b"http/1.1".to_vec()]))
        .transpose()?;
    let max_connections = settings.max_connections;
    let serve: RestServe = Arc::new(move |listener, router, shutdown| {
        let options = ServerOptions {
            tls: tls.clone(),
            max_connections,
            header_read_timeout: HEADER_READ_TIMEOUT,
        };
        Box::pin(serve(listener, router, shutdown, options))
    });
    Ok(Some(RestSettings {
        address: RestApiConfig {
            host: settings.rest_host.clone(),
            port: settings.rest_port,
        },
        authorize,
        max_body_bytes: settings.max_request_bytes,
        request_timeout: Some(config.request_timeout()),
        page_limit: Some(settings.page_size.get()),
        sql_api: settings.sql_api,
        serve: Some(serve),
    }))
}
