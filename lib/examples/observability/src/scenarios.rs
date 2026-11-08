// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{bail, ensure, Context, Result};
use drasi_http_common::{HttpElement, HttpSourceChange};
use drasi_lib::observability::{ExecutionContext, TelemetryProfile, TelemetrySettings};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio::sync::{watch, Mutex as AsyncMutex};

pub const SLOW_DELAY: Duration = Duration::from_millis(300);
const MAX_RECORDS: usize = 4;

pub struct Config {
    pub scenario: String,
    pub profile: String,
    pub sampling: String,
    pub tracing: bool,
    pub metrics: bool,
    pub logs: String,
    pub exclusion: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        fn switch(name: &str) -> Result<bool> {
            match super::setting(name, "true")?.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => bail!("{name} must be true or false"),
            }
        }
        let scenario = super::setting("DEMO_SCENARIO", "follow")?;
        ensure!(
            matches!(scenario.as_str(), "follow" | "controls" | "latency"),
            "DEMO_SCENARIO must be follow, controls, or latency"
        );
        let profile = super::setting("TELEMETRY_PROFILE", "debug")?;
        let parsed: TelemetryProfile = profile.parse()?;
        let tracing = switch("TRACE_ENABLED")? && parsed != TelemetryProfile::Off;
        let metrics = switch("METRICS_ENABLED")? && parsed != TelemetryProfile::Off;
        let config = Self {
            scenario,
            profile,
            sampling: super::setting("TRACE_SAMPLING", "always")?,
            tracing,
            metrics,
            logs: super::setting("DEMO_LOG_LEVEL", "info")?,
            exclusion: super::setting("METRIC_EXCLUDE", "")?,
        };
        ensure!(
            matches!(config.sampling.as_str(), "always" | "never"),
            "TRACE_SAMPLING must be always or never"
        );
        ensure!(
            config.scenario != "latency" || (config.metrics && config.exclusion.is_empty()),
            "latency requires metrics and no metric exclusions; tracing may be disabled"
        );
        Ok(config)
    }

    pub fn settings(&self) -> Result<TelemetrySettings> {
        let sampling = if self.sampling == "always" {
            drasi_lib::observability::RootSampling::Always
        } else {
            drasi_lib::observability::RootSampling::Never
        };
        let mut settings = TelemetrySettings::new(self.profile.parse()?)
            .with_tracing(self.tracing)
            .with_metrics(self.metrics)
            .with_sampling(sampling);
        if self.tracing {
            settings = settings.require_distributed_tracing();
        }
        if self.metrics {
            settings = settings.require_native_metrics();
        }
        if !self.exclusion.is_empty() {
            settings = settings.exclude_metrics(&self.exclusion);
        }
        if self.scenario == "latency" {
            settings = settings.with_handled_branch_latency(Default::default());
        }
        settings.resolve()?;
        Ok(settings)
    }

    pub fn json(&self) -> Value {
        json!({
            "scenario": self.scenario, "profile": self.profile,
            "sampling": self.sampling, "tracing": self.tracing,
            "metrics": self.metrics, "log_level": self.logs,
            "metric_exclusion": self.exclusion,
            "handled_branch_latency": self.scenario == "latency",
            "slow_delay_ms": SLOW_DELAY.as_millis(),
        })
    }
}

#[derive(Clone, Default)]
struct Handled {
    started: u64,
    envelopes: u64,
    rows: u64,
    trace_id: Option<String>,
}

impl Handled {
    fn json(&self) -> Value {
        json!({
            "started": self.started, "envelopes": self.envelopes,
            "rows": self.rows, "trace_id": self.trace_id,
        })
    }
}

#[derive(Default)]
struct Session {
    records: Vec<(u64, u64)>,
    next_id: u64,
    prepared: bool,
    baseline: BTreeMap<String, Handled>,
    metrics: String,
    issued: u64,
}

pub struct Lab {
    pub config: Config,
    metrics: PrometheusHandle,
    client: reqwest::Client,
    endpoint: String,
    gate: watch::Sender<bool>,
    handled: Mutex<BTreeMap<String, Handled>>,
    session: AsyncMutex<Session>,
    closing: AtomicBool,
}

impl Lab {
    pub fn new(config: Config, metrics: PrometheusHandle, port: u16) -> Result<Self> {
        let (gate, _) = watch::channel(false);
        Ok(Self {
            config,
            metrics,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            endpoint: format!("http://127.0.0.1:{port}/sources/http/events"),
            gate,
            handled: Mutex::new(BTreeMap::new()),
            session: AsyncMutex::new(Session {
                next_id: 1000,
                ..Default::default()
            }),
            closing: AtomicBool::new(false),
        })
    }

    pub fn release(&self) {
        self.gate.send_replace(false);
    }

    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.release();
    }

    pub async fn before_handle(&self, component: &str, slow: bool) -> Result<()> {
        {
            let mut handled = self
                .handled
                .lock()
                .map_err(|_| anyhow::anyhow!("demo observation lock poisoned"))?;
            handled.entry(component.into()).or_default().started += 1;
        }
        if slow {
            let mut paused = self.gate.subscribe();
            loop {
                if !*paused.borrow_and_update() {
                    break;
                }
                paused.changed().await.context("slow sink control closed")?;
            }
            tokio::time::sleep(SLOW_DELAY).await;
        }
        Ok(())
    }

    pub fn handled(&self, component: &str, rows: usize, context: &ExecutionContext) -> Result<()> {
        let mut handled = self
            .handled
            .lock()
            .map_err(|_| anyhow::anyhow!("demo observation lock poisoned"))?;
        let observed = handled.entry(component.into()).or_default();
        observed.envelopes += 1;
        observed.rows += u64::try_from(rows)?;
        observed.trace_id = context
            .trace()
            .map(|trace| opentelemetry::trace::TraceId::from_bytes(trace.trace_id()).to_string());
        Ok(())
    }

    fn counts(&self) -> Result<BTreeMap<String, Handled>> {
        self.handled
            .lock()
            .map(|values| values.clone())
            .map_err(|_| anyhow::anyhow!("demo observation lock poisoned"))
    }

    pub async fn snapshot(&self) -> Result<Value> {
        let session = self.session.lock().await;
        let counts = self.counts()?;
        let deltas: BTreeMap<_, _> = counts
            .iter()
            .map(|(name, count)| {
                let baseline = session.baseline.get(name).cloned().unwrap_or_default();
                (
                    name,
                    json!({
                        "started": count.started - baseline.started,
                        "envelopes": count.envelopes - baseline.envelopes,
                        "rows": count.rows - baseline.rows,
                        "trace_id": count.trace_id,
                    }),
                )
            })
            .collect();
        Ok(json!({
            "config": self.config.json(),
            "paused": *self.gate.borrow(),
            "prepared": session.prepared,
            "records": session.records.len(),
            "max_records": MAX_RECORDS,
            "handled": counts.iter().map(|(name, count)| (name, count.json())).collect::<BTreeMap<_, _>>(),
            "since_reset": deltas,
            "baseline_metrics": session.metrics,
            "issued": session.issued,
        }))
    }

    async fn publish(&self, operation: &str, id: u64, value: u64, copies: bool) -> Result<()> {
        let node_id = format!("demo-{id}");
        let labels = vec![if copies { "demo_copies" } else { "trace_items" }.to_owned()];
        let event = if operation == "delete" {
            HttpSourceChange::Delete {
                id: node_id,
                labels: Some(labels),
                timestamp: None,
            }
        } else {
            let element = HttpElement::Node {
                id: node_id,
                labels,
                properties: [
                    ("id".into(), json!(id)),
                    ("name".into(), json!("Dashboard event")),
                    ("value".into(), json!(value)),
                ]
                .into_iter()
                .collect(),
            };
            match operation {
                "insert" => HttpSourceChange::Insert {
                    element,
                    timestamp: None,
                },
                "update" => HttpSourceChange::Update {
                    element,
                    timestamp: None,
                },
                _ => bail!("invalid HTTP demo operation"),
            }
        };
        let response = self
            .client
            .post(&self.endpoint)
            .json(&event)
            .send()
            .await
            .context("send real HTTP source event")?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "HTTP source rejected demo event ({status}): {}",
            response.text().await?
        );
        Ok(())
    }

    async fn settle(&self, targets: &BTreeMap<String, u64>) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let counts = self.counts()?;
                let mut done = true;
                for (component, target) in targets {
                    let observed = *target == 0
                        || counts
                            .get(component)
                            .is_some_and(|count| count.envelopes >= *target);
                    let instrument_enabled = self.config.metrics
                        && (self.config.exclusion.is_empty()
                            || !"drasi.component.inputs_processed"
                                .contains(&self.config.exclusion));
                    done &= observed
                        && (!instrument_enabled
                            || metric_total(
                                &self.metrics.render(),
                                "drasi_component_inputs_processed",
                                component,
                            )? >= *target as f64);
                }
                if done {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("demo handlers did not finish; release the slow sink and retry")?
    }

    fn targets(&self, session: &Session) -> Result<BTreeMap<String, u64>> {
        let counts = self.counts()?;
        let components = if self.config.scenario == "latency" {
            vec!["http-fast", "http-slow"]
        } else {
            vec!["http-output"]
        };
        Ok(components
            .into_iter()
            .map(|name| {
                let baseline = session
                    .baseline
                    .get(name)
                    .map_or(0, |value| value.envelopes);
                let started = counts.get(name).map_or(0, |count| count.started);
                (name.to_owned(), (baseline + session.issued).max(started))
            })
            .collect())
    }

    pub async fn action(&self, action: &str) -> Result<Value> {
        ensure!(
            !self.closing.load(Ordering::Acquire),
            "demo is shutting down"
        );
        if matches!(action, "pause" | "release") {
            ensure!(
                self.config.scenario == "latency",
                "pause/release requires the latency scenario"
            );
            self.gate.send_replace(action == "pause");
            return Ok(json!({"action": action, "paused": action == "pause"}));
        }
        let mut session = self.session.lock().await;
        match action {
            "prepare" => {
                ensure!(
                    self.config.scenario == "latency",
                    "prepare requires latency"
                );
                ensure!(!session.prepared, "latency fan-out is already prepared");
                for index in 0..10 {
                    self.publish("insert", index, 0, true).await?;
                }
                tokio::time::timeout(Duration::from_secs(10), async {
                    while metric_total(
                        &self.metrics.render(),
                        "drasi_query_events_processed",
                        "http-query",
                    )? < 10.0
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("fan-out preparation did not reach the query")??;
                session.prepared = true;
                session.baseline = self.counts()?;
                session.metrics = self.metrics.render();
            }
            "insert" | "burst" => {
                ensure!(
                    self.config.scenario != "latency" || session.prepared,
                    "prepare the latency fan-out first"
                );
                let number = if action == "burst" { 3 } else { 1 };
                ensure!(
                    session.records.len() + number <= MAX_RECORDS,
                    "at most four demo nodes; delete or reset before sending more"
                );
                for _ in 0..number {
                    let id = session.next_id;
                    session.next_id = id.checked_add(1).context("demo ID exhausted")?;
                    self.publish("insert", id, 7, false).await?;
                    session.records.push((id, 7));
                    session.issued += 1;
                }
            }
            "update" | "delete" => {
                let &(id, value) = session.records.last().context("insert a demo node first")?;
                let value = value.checked_add(1).context("demo value exhausted")?;
                self.publish(action, id, value, false).await?;
                session.issued += 1;
                if action == "delete" {
                    session.records.pop();
                } else {
                    session
                        .records
                        .last_mut()
                        .context("demo node disappeared")?
                        .1 = value;
                }
            }
            "reset" => {
                self.release();
                self.settle(&self.targets(&session)?).await?;
                while let Some(&(id, value)) = session.records.last() {
                    self.publish("delete", id, value, false).await?;
                    session.records.pop();
                    session.issued += 1;
                    self.settle(&self.targets(&session)?).await?;
                }
                session.baseline = self.counts()?;
                session.metrics = self.metrics.render();
                session.issued = 0;
            }
            _ => bail!("unknown demo action: {action}"),
        }
        Ok(
            json!({"action": action, "records": session.records.len(), "prepared": session.prepared}),
        )
    }
}

fn metric_total(text: &str, name: &str, component: &str) -> Result<f64> {
    text.lines()
        .filter(|line| {
            line.starts_with(&format!("{name}{{"))
                && line.contains(&format!("component_id=\"{component}\""))
        })
        .try_fold(0.0, |sum, line| {
            let (_, value) = line
                .rsplit_once(' ')
                .context("invalid demo metric sample")?;
            Ok(sum + value.parse::<f64>().context("invalid demo metric value")?)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    fn lab(scenario: &str) -> Lab {
        Lab::new(
            Config {
                scenario: scenario.into(),
                profile: "debug".into(),
                sampling: "always".into(),
                tracing: true,
                metrics: true,
                logs: "info".into(),
                exclusion: String::new(),
            },
            PrometheusBuilder::new().build_recorder().handle(),
            19090,
        )
        .unwrap()
    }

    #[test]
    fn reset_targets_include_queued_envelopes() {
        let lab = lab("latency");
        let session = Session {
            issued: 4,
            baseline: [
                (
                    "http-fast".into(),
                    Handled {
                        envelopes: 1,
                        ..Default::default()
                    },
                ),
                (
                    "http-slow".into(),
                    Handled {
                        envelopes: 1,
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(
            lab.targets(&session).unwrap(),
            [("http-fast".into(), 5), ("http-slow".into(), 5)].into()
        );
    }

    #[test]
    fn reset_targets_include_an_already_running_seed() {
        let lab = lab("latency");
        lab.handled.lock().unwrap().insert(
            "http-slow".into(),
            Handled {
                started: 1,
                ..Default::default()
            },
        );
        assert_eq!(lab.targets(&Session::default()).unwrap()["http-slow"], 1);
    }

    #[test]
    fn metric_matching_is_exact_and_invalid_values_fail() {
        let metrics = "drasi_query_events_processed{component_id=\"http-query\"} 1\n\
            drasi_query_events_processed{component_id=\"http-query-other\"} 99";
        assert_eq!(
            metric_total(metrics, "drasi_query_events_processed", "http-query").unwrap(),
            1.0
        );
        assert_eq!(
            metric_total(metrics, "drasi_query_events", "http-query").unwrap(),
            0.0
        );
        assert!(metric_total("count{component_id=\"sink\"} invalid", "count", "sink").is_err());
    }

    #[tokio::test]
    async fn actions_are_bounded_and_scenario_specific() {
        let lab = lab("follow");
        assert!(lab.action("pause").await.is_err());
        assert!(lab.action("unknown").await.is_err());
        lab.session.lock().await.records = vec![(1000, 7); MAX_RECORDS];
        assert!(lab.action("insert").await.is_err());
        lab.close();
        assert!(lab.action("reset").await.is_err());
    }
}
