// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, HttpSinkConfig},
    wire,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComputationComponent, EnvelopeSink, InputEnvelope, SinkCompletion,
};

pub(crate) struct HttpSink {
    descriptor: ComponentDescriptor,
    config: HttpSinkConfig,
    client: Option<reqwest::Client>,
}
impl HttpSink {
    pub fn new(descriptor: ComponentDescriptor, config: HttpSinkConfig) -> Result<Self> {
        Ok(Self {
            descriptor,
            config,
            client: None,
        })
    }
    async fn deliver(&self, notification: &wire::Notification) -> Result<()> {
        let client = self.client.as_ref().context("HTTP sink has not started")?;
        let body = serde_json::to_vec(notification)?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &self.config.headers {
            headers.insert(name.parse::<reqwest::header::HeaderName>()?, value.parse()?);
        }
        for attempt in 0..=self.config.max_retries {
            let result = async {
                let response = client
                    .post(&self.config.url)
                    .headers(headers.clone())
                    .body(body.clone())
                    .send()
                    .await?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "HTTP delivery returned {}",
                    response.status()
                );
                Ok(())
            }
            .await;
            match result {
                Ok(()) => return Ok(()),
                Err(error) if attempt == self.config.max_retries => return Err(error),
                Err(error) => {
                    eprintln!(
                        "drasi.network HTTP retry: query={} sequence={} attempt={} error={error:#}",
                        notification.query_id,
                        notification.sequence_id,
                        attempt + 1
                    );
                    backoff(attempt).await;
                }
            }
        }
        unreachable!("retry loop always returns")
    }
}
#[async_trait]
impl ComputationComponent for HttpSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(self.client.is_none(), "HTTP sink already started");
        self.client = Some(
            reqwest::Client::builder()
                .timeout(duration(self.config.timeout_ms, "timeoutMs")?)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        );
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.client = None;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for HttpSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        anyhow::ensure!(self.client.is_some(), "HTTP sink has not started");
        for notification in wire::notifications(&input, &self.config.query_id, &self.config.stream)?
        {
            self.config.failure_policy.delivery(
                self.deliver(&notification).await,
                self.descriptor.id().as_str(),
                &self.config.query_id,
                notification.sequence_id,
            )?;
        }
        Ok(())
    }
}
