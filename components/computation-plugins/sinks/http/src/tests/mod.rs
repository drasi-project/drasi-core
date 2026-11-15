// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! In-crate tests: a recording HTTP destination, the sink conformance kit,
//! delivery semantics, and the delivery consumer.

mod consumer;
mod sink;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use drasi_computation_contracts::{
    ComponentId, GraphChangeCodec, InputBinding, InputEnvelope, OutputEndpoint, PortId, StreamId,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

pub(crate) const SINK_ID: &str = "8b1f0c2e-4d6a-4e8b-9c0d-1e2f3a4b5c6d";
pub(crate) const OUTPUT_ID: &str = "63d14bfc-2827-577e-aed6-d4e1f755df25";
pub(crate) const STREAM: &str = "q/out";

/// One request seen by the destination.
#[derive(Clone, Debug)]
pub(crate) struct Captured {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
    pub fn json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// A scripted reply: status and extra headers.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }
}

/// Destination state: recorded requests, scripted replies and a default.
pub(crate) struct Destination {
    pub requests: Mutex<Vec<Captured>>,
    pub script: Mutex<VecDeque<Reply>>,
    pub status: AtomicU16,
    pub stall: AtomicBool,
    pub responder: Option<fn(&Captured) -> Reply>,
}

impl Destination {
    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().expect("requests").clone()
    }
    pub fn push(&self, reply: Reply) {
        self.script.lock().expect("script").push_back(reply);
    }
    pub async fn wait_for(&self, count: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.requests.lock().expect("requests").len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .with_context(|| format!("destination did not see {count} requests"))
    }
}

async fn record(
    State(state): State<Arc<Destination>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let captured = Captured {
        method,
        path: uri.path().to_owned(),
        headers,
        body,
    };
    state
        .requests
        .lock()
        .expect("requests")
        .push(captured.clone());
    while state.stall.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let scripted = state.script.lock().expect("script").pop_front();
    let reply = scripted
        .or_else(|| state.responder.map(|responder| responder(&captured)))
        .unwrap_or_else(|| Reply::status(state.status.load(Ordering::Acquire)));
    let mut response = (
        StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        reply.body,
    )
        .into_response();
    for (name, value) in reply.headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(name), value);
        }
    }
    response
}

/// A recording HTTP server on 127.0.0.1, optionally with TLS.
pub(crate) struct Server {
    pub address: SocketAddr,
    pub state: Arc<Destination>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start() -> Result<Self> {
        Self::start_with(None, None).await
    }

    pub async fn start_with(
        tls: Option<drasi_computation_network_common::config::TlsMaterial>,
        responder: Option<fn(&Captured) -> Reply>,
    ) -> Result<Self> {
        let tls = tls
            .map(|tls| tls.server_config(vec![b"http/1.1".to_vec()]))
            .transpose()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Destination {
            requests: Mutex::default(),
            script: Mutex::default(),
            status: AtomicU16::new(200),
            stall: AtomicBool::new(false),
            responder,
        });
        let router = Router::new().fallback(record).with_state(state.clone());
        let task = tokio::spawn(async move {
            let options = drasi_computation_network_common::server::ServerOptions {
                tls,
                max_connections: 64,
                header_read_timeout: Duration::from_secs(5),
            };
            let _ = drasi_computation_network_common::server::serve(
                listener,
                router,
                std::future::pending::<()>(),
                options,
            )
            .await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }
}

pub(crate) fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new(OUTPUT_ID).expect("component"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new(STREAM).expect("stream"),
    }]
}

fn metadata(id: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("facilities", id),
        labels: Arc::from([Arc::<str>::from("Room")]),
        effective_from: 1_000,
    }
}

pub(crate) fn insert(id: &str, name: &str) -> SourceChange {
    let mut properties = ElementPropertyMap::new();
    properties.insert("name", ElementValue::String(name.into()));
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id),
            properties,
        },
    }
}

pub(crate) fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(id),
    }
}

pub(crate) fn envelope(sequence: u64, changes: &[SourceChange]) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new(STREAM)?,
            sequence,
            None,
        )?,
    })
}

pub(crate) fn input(sequence: u64) -> Result<InputEnvelope> {
    envelope(sequence, &[insert(&format!("room-{sequence}"), "one")])
}
