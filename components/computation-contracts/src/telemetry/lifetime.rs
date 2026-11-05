// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{Result, TelemetryError};
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, RwLock, Weak,
    },
    time::Instant,
};

/// Updates and retirement are linearized; callers never hold this across an await.
#[derive(Debug, Clone)]
pub struct TelemetryLifetime(Arc<Lifetime>);

#[derive(Debug)]
struct Lifetime {
    active: RwLock<bool>,
    retirements: Mutex<Vec<Weak<dyn TelemetryRetirement>>>,
}

pub(super) trait TelemetryRetirement: std::fmt::Debug + Send + Sync {
    fn retire(&self);
}

impl Default for TelemetryLifetime {
    fn default() -> Self {
        Self(Arc::new(Lifetime {
            active: RwLock::new(true),
            retirements: Mutex::new(Vec::new()),
        }))
    }
}

impl TelemetryLifetime {
    pub fn with_active<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let active = self.0.active.read().map_err(|_| TelemetryError::Poisoned)?;
        if !*active {
            return Err(TelemetryError::Retired);
        }
        work()
    }

    pub fn is_active(&self) -> Result<bool> {
        Ok(*self.0.active.read().map_err(|_| TelemetryError::Poisoned)?)
    }

    /// Run final gauge resets while late writers are excluded.
    pub fn revoke(&self, retire: impl FnOnce()) -> Result<bool> {
        let mut active = self
            .0
            .active
            .write()
            .map_err(|_| TelemetryError::Poisoned)?;
        if !*active {
            return Ok(false);
        }
        *active = false;
        let listeners = std::mem::take(
            &mut *self
                .0
                .retirements
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?,
        );
        for listener in listeners
            .into_iter()
            .filter_map(|listener| listener.upgrade())
        {
            listener.retire();
        }
        retire();
        Ok(true)
    }

    pub(super) fn on_retire(&self, listener: Weak<dyn TelemetryRetirement>) -> Result<()> {
        let mut listeners = self
            .0
            .retirements
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        listeners.retain(|listener| listener.strong_count() != 0);
        if listeners.len() >= 16_384 {
            return Err(TelemetryError::Capacity("telemetry retirement listeners"));
        }
        listeners.push(listener);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum TelemetryLoss {
    Full,
    Invalid,
    Closed,
    ShutdownTimeout,
    Capture,
    Metric,
    Unattributed,
    Exporter,
}

impl TelemetryLoss {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Invalid => "invalid",
            Self::Closed => "closed",
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::Capture => "capture",
            Self::Metric => "metric",
            Self::Unattributed => "unattributed",
            Self::Exporter => "exporter",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TelemetryLossSnapshot {
    pub full: u64,
    pub invalid: u64,
    pub closed: u64,
    pub shutdown_timeout: u64,
    pub capture: u64,
    pub metric: u64,
    pub unattributed: u64,
    pub exporter: u64,
}

struct Diagnostics {
    counts: [AtomicU64; 8],
    reported: [AtomicU64; 8],
}

#[derive(Clone)]
pub struct TelemetryDiagnostics(Arc<Diagnostics>);

impl Default for TelemetryDiagnostics {
    fn default() -> Self {
        Self(Arc::new(Diagnostics {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            reported: std::array::from_fn(|_| AtomicU64::new(u64::MAX)),
        }))
    }
}

impl std::fmt::Debug for TelemetryDiagnostics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(formatter)
    }
}

thread_local! {
    static REPORTING: Cell<bool> = const { Cell::new(false) };
}

impl TelemetryDiagnostics {
    pub fn record(&self, reason: TelemetryLoss) {
        self.record_many(reason, 1);
    }

    pub fn record_many(&self, reason: TelemetryLoss, count: u64) {
        if count == 0 {
            return;
        }
        let slot = reason as usize;
        let total = self.0.counts[slot]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(count))
            })
            .unwrap_or_else(|value| value)
            .saturating_add(count);
        if REPORTING.with(Cell::get) {
            return;
        }
        static START: OnceLock<Instant> = OnceLock::new();
        let now = START.get_or_init(Instant::now).elapsed().as_secs();
        let last = self.0.reported[slot].load(Ordering::Relaxed);
        if (last != u64::MAX && now.saturating_sub(last) < 1)
            || self.0.reported[slot]
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        struct Reporting;
        impl Drop for Reporting {
            fn drop(&mut self) {
                REPORTING.with(|reporting| reporting.set(false));
            }
        }
        REPORTING.with(|reporting| reporting.set(true));
        let _reporting = Reporting;
        log::warn!(
            target: "drasi::telemetry_loss",
            "Telemetry records rejected: reason={}, total={total}",
            reason.as_str()
        );
    }

    pub fn snapshot(&self) -> TelemetryLossSnapshot {
        let values: [u64; 8] =
            std::array::from_fn(|index| self.0.counts[index].load(Ordering::Relaxed));
        TelemetryLossSnapshot {
            full: values[0],
            invalid: values[1],
            closed: values[2],
            shutdown_timeout: values[3],
            capture: values[4],
            metric: values[5],
            unattributed: values[6],
            exporter: values[7],
        }
    }
}
