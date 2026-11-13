// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, InputEnvelope, QueryChangeCodec, SinkCompletion,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};
use drasi_core_types::profiling::{timestamp_ns, ProfilingMetadata};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

fn window_size() -> usize {
    1000
}

fn report_interval_secs() -> u64 {
    60
}

pub(super) fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    ConfigDefinition::try_new([
        Field::new(
            "windowSize",
            Integer,
            false,
            "Maximum number of profiling samples retained for percentile reports.",
        )
        .default(window_size())
        .minimum(1),
        Field::new(
            "reportIntervalSecs",
            Integer,
            false,
            "Positive interval between profiling reports.",
        )
        .default(report_interval_secs())
        .minimum(1),
    ])
    .expect("constant profiler sink configuration")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfilerSinkConfig {
    #[serde(default = "window_size")]
    pub window_size: usize,
    #[serde(default = "report_interval_secs")]
    pub report_interval_secs: u64,
}

impl Default for ProfilerSinkConfig {
    fn default() -> Self {
        Self {
            window_size: window_size(),
            report_interval_secs: report_interval_secs(),
        }
    }
}

impl ProfilerSinkConfig {
    pub fn parse(value: serde_json::Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        ensure!(config.window_size > 0, "windowSize must be positive");
        ensure!(
            config.report_interval_secs > 0,
            "reportIntervalSecs must be positive"
        );
        Ok(config)
    }
}

#[derive(Default)]
struct Samples {
    values: VecDeque<ProfilingMetadata>,
    capacity: usize,
}

impl Samples {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, value: ProfilingMetadata) {
        self.values.push_back(value);
        if self.values.len() > self.capacity {
            self.values.pop_front();
        }
    }

    fn metric(
        &self,
        name: &str,
        value: impl Fn(&ProfilingMetadata) -> Option<u64>,
    ) -> Option<String> {
        let mut values: Vec<_> = self.values.iter().filter_map(value).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let percentile = |fraction: f64| {
            let index = (fraction * (values.len() - 1) as f64).round() as usize;
            values[index]
        };
        let mean = values.iter().map(|value| *value as f64).sum::<f64>() / values.len() as f64;
        Some(format!(
            "{name}: count={} mean_ns={mean:.0} min_ns={} max_ns={} p50_ns={} p95_ns={} p99_ns={}",
            values.len(),
            values[0],
            values[values.len() - 1],
            percentile(0.50),
            percentile(0.95),
            percentile(0.99),
        ))
    }

    fn report(&self, component: &str) {
        if self.values.is_empty() {
            log::info!("[{component}] no profiling data collected");
            return;
        }
        for line in [
            self.metric(
                "source_to_query",
                ProfilingMetadata::elapsed_source_to_query,
            ),
            self.metric(
                "query_processing",
                ProfilingMetadata::elapsed_query_processing,
            ),
            self.metric(
                "query_to_reaction",
                ProfilingMetadata::elapsed_query_to_reaction,
            ),
            self.metric(
                "reaction_processing",
                ProfilingMetadata::elapsed_reaction_processing,
            ),
            self.metric("total", ProfilingMetadata::elapsed_total),
        ]
        .into_iter()
        .flatten()
        {
            log::info!("[{component}] {line}");
        }
    }
}

pub(super) struct ProfilerSink {
    descriptor: ComponentDescriptor,
    config: ProfilerSinkConfig,
    samples: Arc<Mutex<Samples>>,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
}

impl ProfilerSink {
    pub(super) fn new(descriptor: ComponentDescriptor, config: ProfilerSinkConfig) -> Self {
        Self {
            descriptor,
            samples: Arc::new(Mutex::new(Samples::new(config.window_size))),
            config,
            cancel: None,
            task: None,
        }
    }
}

#[async_trait]
impl NativeComponent for ProfilerSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    async fn start(&mut self) -> Result<()> {
        ensure!(self.task.is_none(), "profiler sink already started");
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let samples = self.samples.clone();
        let component = self.descriptor.id().to_string();
        let interval = Duration::from_secs(self.config.report_interval_secs);
        self.task = Some(tokio::spawn(async move {
            let mut reports = tokio::time::interval(interval);
            reports.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = reports.tick() => samples.lock().await.report(&component),
                }
            }
        }));
        self.cancel = Some(cancel);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeSink for ProfilerSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        ensure!(input.port.as_str() == "in", "unknown profiler input port");
        if let Some(mut profiling) = QueryChangeCodec::metadata(&input.envelope)?.profiling {
            let received = timestamp_ns();
            profiling.reaction_receive_ns = Some(received);
            profiling.reaction_complete_ns = Some(timestamp_ns());
            self.samples.lock().await.push(profiling);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiling_samples_are_bounded_and_report_native_latencies() {
        let mut samples = Samples::new(1);
        for offset in [0, 10] {
            samples.push(ProfilingMetadata {
                source_receive_ns: Some(offset),
                source_send_ns: Some(1 + offset),
                query_receive_ns: Some(2 + offset),
                query_core_call_ns: Some(3 + offset),
                query_core_return_ns: Some(4 + offset),
                query_send_ns: Some(5 + offset),
                reaction_receive_ns: Some(6 + offset),
                reaction_complete_ns: Some(7 + offset),
                ..Default::default()
            });
        }
        assert_eq!(samples.values.len(), 1);
        assert!(samples
            .metric("total", ProfilingMetadata::elapsed_total)
            .is_some());
    }
}
