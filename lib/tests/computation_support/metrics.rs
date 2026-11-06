// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]
#![allow(dead_code)]

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey(String, BTreeMap<String, String>);

impl From<&Key> for MetricKey {
    fn from(key: &Key) -> Self {
        Self(
            key.name().into(),
            key.labels()
                .map(|label| (label.key().into(), label.value().into()))
                .collect(),
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct Values {
    pub counter: u64,
    pub gauge: f64,
    pub histogram: Vec<f64>,
    pub gauge_samples: Vec<f64>,
}

#[derive(Default)]
struct Measurement(Mutex<Values>, Arc<tokio::sync::Notify>);

impl CounterFn for Measurement {
    fn increment(&self, value: u64) {
        self.0.lock().unwrap().counter += value;
        self.1.notify_waiters();
    }
    fn absolute(&self, value: u64) {
        let mut current = self.0.lock().unwrap();
        current.counter = current.counter.max(value);
        self.1.notify_waiters();
    }
}
impl GaugeFn for Measurement {
    fn increment(&self, value: f64) {
        self.0.lock().unwrap().gauge += value;
        self.1.notify_waiters();
    }
    fn decrement(&self, value: f64) {
        self.0.lock().unwrap().gauge -= value;
        self.1.notify_waiters();
    }
    fn set(&self, value: f64) {
        let mut current = self.0.lock().unwrap();
        current.gauge = value;
        current.gauge_samples.push(value);
        self.1.notify_waiters();
    }
}
impl HistogramFn for Measurement {
    fn record(&self, value: f64) {
        self.0.lock().unwrap().histogram.push(value);
        self.1.notify_waiters();
    }
}

#[derive(Default, Clone)]
pub struct RecordingMetrics(
    Arc<Mutex<BTreeMap<MetricKey, Arc<Measurement>>>>,
    Arc<tokio::sync::Notify>,
);

impl RecordingMetrics {
    fn instrument(&self, key: &Key) -> Arc<Measurement> {
        self.0
            .lock()
            .unwrap()
            .entry(key.into())
            .or_insert_with(|| Arc::new(Measurement(Mutex::new(Values::default()), self.1.clone())))
            .clone()
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }

    pub fn labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .keys()
            .filter(|key| key.0 == name)
            .map(|key| key.1.clone())
            .collect()
    }

    pub async fn wait_for(&self, predicate: impl Fn() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let changed = self.1.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if predicate() {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("metric observation did not arrive");
    }

    pub fn matching(&self, name: &str, labels: &[(&str, &str)]) -> Vec<Values> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                key.0 == name
                    && labels
                        .iter()
                        .all(|(label, value)| key.1.get(*label).map(String::as_str) == Some(*value))
            })
            .map(|(_, measurement)| measurement.0.lock().unwrap().clone())
            .collect()
    }

    pub fn count(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        self.matching(name, labels)
            .iter()
            .map(|value| value.counter)
            .sum()
    }

    pub fn histogram(&self, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        self.matching(name, labels)
            .into_iter()
            .flat_map(|value| value.histogram)
            .collect()
    }

    pub fn gauge_samples(&self, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        self.matching(name, labels)
            .into_iter()
            .flat_map(|value| value.gauge_samples)
            .collect()
    }
}

impl Recorder for RecordingMetrics {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(self.instrument(key))
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(self.instrument(key))
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(self.instrument(key))
    }
}
